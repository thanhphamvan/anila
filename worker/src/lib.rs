//! Contracts C1 and C4: the HTTP API, and forwarding websocket upgrades to
//! the host room with identity headers the room can trust.
//!
//! Static files (the frontend) are served by Workers static assets before
//! this code runs; see `wrangler.toml`.

use anila_proto::{
    AgentStatus, HostId, api,
    limits::{AGENT_SKEW_SECS, SESSION_SECS},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::*;

pub mod agent_sig;
mod auth;
pub mod auth_core;
mod room;
pub mod tenancy;
mod util;
pub mod webauthn;
pub mod webpush;

pub use auth::AuthStore;
pub use room::HostRoom;

/// Headers the worker sets for the room. Any incoming copy is stripped.
pub(crate) const H_ROLE: &str = "Term-Role";
pub(crate) const H_HOST: &str = "Term-Host";
pub(crate) const H_SID_HASH: &str = "Term-Sid-Hash";
pub(crate) const H_EXPIRES: &str = "Term-Expires";
pub(crate) const H_ACCOUNT: &str = "Term-Account";
pub(crate) const H_HOST_NAME: &str = "Term-Host-Name";
pub(crate) const H_HOST_KEY: &str = "Term-Host-Key";

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    match dispatch(req, env).await {
        Ok(response) => Ok(response),
        Err(error) => {
            console_error!("worker request: {error}");
            util::json_error(500, "internal", "request failed")
        }
    }
}

fn auth_stub(env: &Env) -> Result<Stub> {
    env.durable_object("AUTH")?.id_from_name("auth")?.get_stub()
}

async fn call_auth(env: &Env, route: &str, body: &Value) -> Result<Response> {
    auth_stub(env)?
        .fetch_with_request(util::json_request(&format!("https://auth/{route}"), body)?)
        .await
}

fn cookie_hash(req: &Request) -> Result<Option<String>> {
    Ok(
        auth_core::read_cookie(req.headers().get("Cookie")?.as_deref())
            .map(|sid| auth_core::hash_sid(&sid)),
    )
}

async fn dispatch(mut req: Request, env: Env) -> Result<Response> {
    let path = req.path();
    let method = req.method();
    if method == Method::Get && path == "/ws/term" {
        return term_upgrade(req, env).await;
    }
    if method == Method::Get && path == "/ws/agent" {
        return agent_upgrade(req, env).await;
    }
    if !path.starts_with("/api/") {
        return util::json_error(404, "not_found", "unknown route");
    }
    // Signed service calls carry no cookie, so CSRF does not apply; their
    // signature is checked below instead.
    let signed_call = path.starts_with("/api/agent/");
    if method != Method::Get
        && !signed_call
        && auth_core::check_mutation(
            method.as_ref(),
            req.headers().get("Content-Type")?.as_deref(),
            req.headers().get("Origin")?.as_deref(),
            &env.var("ORIGIN")?.to_string(),
        )
        .is_err()
    {
        return util::json_error(403, "forbidden", "invalid content type or origin");
    }
    if method == Method::Post
        && matches!(
            path.as_str(),
            "/api/agent/pair"
                | "/api/agent/enroll-code"
                | "/api/agent/revoke-all"
                | "/api/agent/notify"
        )
    {
        return signed_agent_call(req, &env).await;
    }
    let hash = cookie_hash(&req)?;
    let session_body = json!({"sid_hash": hash});
    if let Some(route) = tenancy::parse_route(method.as_ref(), &path) {
        return tenancy_route(req, &env, route, hash).await;
    }
    match (method, path.as_str()) {
        (Method::Get, "/api/push/key") => {
            let response = call_auth(&env, "session", &session_body).await?;
            if response.status_code() != 200 {
                return Ok(response);
            }
            let Some(key) = vapid_key(&env)? else {
                return push_unconfigured();
            };
            let public = webpush::vapid_public_key(key).map_err(push_error)?;
            Response::from_json(&json!({"key": public}))
        }
        (Method::Post, "/api/push/subscribe" | "/api/push/unsubscribe") => {
            let response = call_auth(&env, "session", &session_body).await?;
            if response.status_code() != 200 {
                return Ok(response);
            }
            let body = match req.json::<Value>().await {
                Ok(body) if body.is_object() => body,
                _ => return util::json_error(400, "bad_frame", "expected a JSON object"),
            };
            let Some(endpoint) = body["endpoint"].as_str() else {
                return util::json_error(400, "bad_frame", "missing endpoint");
            };
            if path.ends_with("unsubscribe") {
                return call_auth(
                    &env,
                    "push/unsubscribe",
                    &json!({"sid_hash": hash, "endpoint": endpoint}),
                )
                .await;
            }
            if !push_endpoint_allowed(endpoint, &env)? {
                return util::json_error(400, "bad_frame", "invalid push endpoint");
            }
            let (Ok(public), Ok(auth)) = (
                push_key_bytes(&body["keys"]["p256dh"]),
                push_key_bytes(&body["keys"]["auth"]),
            ) else {
                return util::json_error(400, "bad_frame", "invalid subscription keys");
            };
            if public.len() != 65
                || auth.len() != 16
                || p256::PublicKey::from_sec1_bytes(&public).is_err()
            {
                return util::json_error(400, "bad_frame", "invalid subscription keys");
            }
            call_auth(
                &env,
                "push/subscribe",
                &json!({"sid_hash": hash, "endpoint": endpoint,
                "p256dh": B64.encode(public), "auth": B64.encode(auth)}),
            )
            .await
        }
        (Method::Get, "/api/me") => call_auth(&env, "me", &session_body).await,
        (Method::Get, "/api/passkeys") => call_auth(&env, "passkeys", &session_body).await,
        (Method::Delete, _) if path.starts_with("/api/passkeys/") => {
            let id = &path["/api/passkeys/".len()..];
            call_auth(
                &env,
                "passkeys/delete",
                &json!({"sid_hash": hash, "id": id}),
            )
            .await
        }
        (Method::Post, "/api/logout") => {
            if hash.is_some() {
                let response = call_auth(&env, "logout", &session_body).await?;
                if response.status_code() != 200 {
                    return Ok(response);
                }
            }
            let mut response = Response::empty()?.with_status(204);
            response
                .headers_mut()
                .set("Set-Cookie", &auth_core::clear_cookie())?;
            Ok(response)
        }
        (
            Method::Post,
            "/api/passkeys/register/options"
            | "/api/passkeys/register/verify"
            | "/api/passkeys/login/options"
            | "/api/passkeys/login/verify",
        ) => {
            let mut body = match req.json::<Value>().await {
                Ok(body) if body.is_object() => body,
                _ => return util::json_error(400, "bad_frame", "expected a JSON object"),
            };
            // Never trust a session hash supplied in a public request body.
            body["sid_hash"] = json!(hash);
            let route = &path["/api/passkeys/".len()..];
            let mut response = call_auth(&env, route, &body).await?;
            if response.status_code() != 200 || !route.ends_with("/verify") {
                return Ok(response);
            }
            let body: Value = response.json().await?;
            let sid = body["sid"]
                .as_str()
                .ok_or_else(|| Error::RustError("missing session id".into()))?;
            let mut response = if route == "register/verify" {
                Response::from_json(&json!({"id": body["id"]}))?.with_status(201)
            } else {
                Response::empty()?.with_status(204)
            };
            response
                .headers_mut()
                .set("Set-Cookie", &auth_core::set_cookie(sid, SESSION_SECS))?;
            Ok(response)
        }
        _ => util::json_error(404, "not_found", "unknown route"),
    }
}

async fn signed_agent_call(mut req: Request, env: &Env) -> Result<Response> {
    let path = req.path();
    let bytes = req.bytes().await?;
    let host = req
        .headers()
        .get(anila_proto::sig::HEADER_HOST)?
        .unwrap_or_default();
    let pair = path == "/api/agent/pair";
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(body) if body.is_object() => body,
        _ => return util::json_error(400, "bad_frame", "expected a JSON object"),
    };
    let pair_request = if pair {
        if host != api::PAIR_HOST {
            return util::json_error(401, "unauthorized", "invalid pairing host");
        }
        match serde_json::from_value::<api::PairRequest>(body.clone()) {
            Ok(request) => Some(request),
            Err(_) => return util::json_error(400, "bad_frame", "invalid pair request"),
        }
    } else {
        None
    };
    if !util::insecure_dev_allowed(env)? {
        let key = if let Some(request) = &pair_request {
            request.public_key.clone()
        } else {
            let mut response = call_auth(env, "hosts/key", &json!({"host": host})).await?;
            if response.status_code() == 404 {
                return util::json_error(401, "unauthorized", "machine not paired");
            }
            if response.status_code() != 200 {
                return Ok(response);
            }
            let body: Value = response.json().await?;
            required_str(&body, "public_key")?.to_string()
        };
        let ts = req
            .headers()
            .get(anila_proto::sig::HEADER_TS)?
            .and_then(|ts| ts.parse::<i64>().ok());
        let signature = req
            .headers()
            .get(anila_proto::sig::HEADER_SIG)?
            .unwrap_or_default();
        let verified = ts.is_some_and(|ts| {
            util::now_secs().abs_diff(ts) <= AGENT_SKEW_SECS as u64
                && agent_sig::verify_with_key(
                    &key,
                    anila_proto::sig::http_sign_input(
                        "POST",
                        &path,
                        ts,
                        &util::hex(&Sha256::digest(&bytes)),
                    )
                    .as_bytes(),
                    &signature,
                )
        });
        if !verified {
            return util::json_error(401, "unauthorized", "invalid service signature");
        }
    }
    if let Some(request) = pair_request {
        return call_auth(env, "agent/pair", &serde_json::to_value(request)?).await;
    }
    if path == "/api/agent/notify" {
        return notify(env, &host, body).await;
    }
    let route = if path.ends_with("enroll-code") {
        "agent/enroll-code"
    } else {
        "agent/revoke-all"
    };
    call_auth(env, route, &json!({"host": host})).await
}

async fn tenancy_route(
    mut req: Request,
    env: &Env,
    route: tenancy::ApiRoute,
    hash: Option<String>,
) -> Result<Response> {
    use tenancy::ApiRoute::*;
    let body = if matches!(req.method(), Method::Post | Method::Patch) {
        match req.json::<Value>().await {
            Ok(body) if body.is_object() => body,
            _ => return util::json_error(400, "bad_frame", "expected a JSON object"),
        }
    } else {
        json!({})
    };
    let list = matches!(&route, ListHosts);
    let view = matches!(&route, RenameHost(_) | RedeemShare);
    let (route, mut body) = match route {
        ListHosts => ("hosts/list", json!({})),
        RenameHost(host) => ("hosts/rename", json!({"host": host, "name": body["name"]})),
        RemoveHost(host) => ("hosts/remove", json!({"host": host})),
        ListMembers(host) => ("hosts/members", json!({"host": host})),
        RemoveMember(host, account) => (
            "hosts/members/remove",
            json!({"host": host, "account": account}),
        ),
        CreateShareCode(host) => ("codes/create", json!({"kind": "share", "host": host})),
        RedeemShare => ("share/redeem", json!({"code": body["code"]})),
        CreatePairCode => ("codes/create", json!({"kind": "pair"})),
        CreateDeviceCode => ("codes/create", json!({"kind": "device"})),
        CheckCode => ("codes/check", json!({"code": body["code"]})),
        CreateInvite => ("codes/create", json!({"kind": "invite"})),
        ListAccounts => ("admin/accounts", json!({})),
        DeleteAccount(account) => ("admin/accounts/delete", json!({"account": account})),
    };
    body["sid_hash"] = json!(hash);
    let mut response = call_auth(env, route, &body).await?;
    if !(200..300).contains(&response.status_code()) {
        return Ok(response);
    }
    if list {
        let rows: Vec<HostRow> = response.json().await?;
        let mut hosts = Vec::with_capacity(rows.len());
        for row in rows {
            hosts.push(host_view(env, row).await);
        }
        return Response::from_json(&hosts);
    }
    if view {
        return Response::from_json(&host_view(env, response.json().await?).await);
    }
    Ok(response)
}

#[derive(Deserialize)]
struct HostRow {
    id: HostId,
    name: HostId,
    role: api::Role,
    owner_name: String,
}

async fn room_status(env: &Env, host: &str) -> Result<AgentStatus> {
    let stub = env
        .durable_object("HOST_ROOM")?
        .id_from_name(host)?
        .get_stub()?;
    let mut response = stub
        .fetch_with_request(util::json_request(
            "https://room/internal/status",
            &json!({}),
        )?)
        .await?;
    if response.status_code() != 200 {
        return Err(Error::RustError("room status unavailable".into()));
    }
    response.json().await
}

async fn host_view(env: &Env, row: HostRow) -> api::HostView {
    let status = room_status(env, row.id.as_str()).await.unwrap_or_default();
    api::HostView {
        id: row.id,
        name: row.name,
        role: row.role,
        owner_name: row.owner_name,
        online: status.online,
        since: status.since,
    }
}

fn is_websocket_upgrade(req: &Request) -> Result<bool> {
    Ok(req
        .headers()
        .get("Upgrade")?
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket")))
}

/// Browser socket. Requires the configured Origin and a valid session cookie.
async fn term_upgrade(req: Request, env: Env) -> Result<Response> {
    if !is_websocket_upgrade(&req)? {
        return util::json_error(426, "bad_frame", "expected a websocket upgrade");
    }
    let origin = env.var("ORIGIN")?.to_string();
    if req.headers().get("Origin")?.as_deref() != Some(origin.as_str()) {
        return util::json_error(403, "forbidden", "wrong origin");
    }
    let host = req
        .url()?
        .query_pairs()
        .find(|(k, _)| k == "host")
        .map(|(_, v)| HostId::new(v.into_owned()));
    let host = match host.transpose() {
        Ok(host) => host,
        Err(_) => return util::json_error(400, "invalid_name", "invalid host"),
    };
    if let Some(hash) = cookie_hash(&req)? {
        let mut body = json!({"sid_hash": hash});
        if let Some(host) = &host {
            body["host"] = json!(host);
        }
        let mut response = call_auth(&env, "access", &body).await?;
        if response.status_code() == 200 {
            let access: Value = response.json().await?;
            let expires = access["expires_at"]
                .as_i64()
                .ok_or_else(|| Error::RustError("missing session expiry".into()))?
                .to_string();
            return forward(
                req,
                &env,
                required_str(&access, "host")?,
                &[
                    (H_ROLE, "browser"),
                    (H_SID_HASH, &hash),
                    (H_EXPIRES, &expires),
                    (H_ACCOUNT, required_str(&access, "account_id")?),
                    (H_HOST_NAME, required_str(&access, "host_name")?),
                ],
            )
            .await;
        }
        if response.status_code() != 401 || !util::insecure_dev_allowed(&env)? {
            return Ok(response);
        }
    }
    if !util::insecure_dev_allowed(&env)? {
        return util::json_error(401, "unauthorized", "not signed in");
    }
    let host = match host {
        Some(host) => host.to_string(),
        None => env.var("DEFAULT_HOST")?.to_string(),
    };
    forward(req, &env, &host, &[(H_ROLE, "browser")]).await
}

/// Service socket. The room verifies its challenge response with the stored key.
async fn agent_upgrade(req: Request, env: Env) -> Result<Response> {
    if !is_websocket_upgrade(&req)? {
        return util::json_error(426, "bad_frame", "expected a websocket upgrade");
    }
    let url = req.url()?;
    let host = url
        .query_pairs()
        .find(|(k, _)| k == "host")
        .map(|(_, v)| v.into_owned());
    let Some(host) = host.and_then(|h| HostId::new(h).ok()) else {
        return util::json_error(400, "invalid_name", "missing or invalid host");
    };
    let mut response = call_auth(&env, "hosts/key", &json!({"host": host})).await?;
    if response.status_code() == 404 {
        if util::insecure_dev_allowed(&env)? {
            return forward(req, &env, host.as_str(), &[(H_ROLE, "agent")]).await;
        }
        return util::json_error(404, "not_found", "machine not paired");
    }
    if response.status_code() != 200 {
        return Ok(response);
    }
    let key: Value = response.json().await?;
    forward(
        req,
        &env,
        host.as_str(),
        &[
            (H_ROLE, "agent"),
            (H_HOST_KEY, required_str(&key, "public_key")?),
        ],
    )
    .await
}

fn required_str<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    body[field]
        .as_str()
        .ok_or_else(|| Error::RustError(format!("missing {field} in auth response")))
}

async fn forward(req: Request, env: &Env, host: &str, set: &[(&str, &str)]) -> Result<Response> {
    let mut req = req.clone_mut()?;
    let headers = req.headers_mut()?;
    let incoming: Vec<String> = headers
        .keys()
        .filter(|k| k.to_ascii_lowercase().starts_with("term-"))
        .collect();
    for k in incoming {
        headers.delete(&k)?;
    }
    headers.set(H_HOST, host)?;
    for (k, v) in set {
        headers.set(k, v)?;
    }
    let stub = env
        .durable_object("HOST_ROOM")?
        .id_from_name(host)?
        .get_stub()?;
    stub.fetch_with_request(req).await
}

fn push_error(error: webpush::PushError) -> Error {
    Error::RustError(format!("push: {error:?}"))
}

fn push_unconfigured() -> Result<Response> {
    util::json_error(503, "internal", "push is not configured")
}

fn vapid_key(env: &Env) -> Result<Option<[u8; 32]>> {
    let value = env
        .secret("VAPID_PRIVATE_KEY")
        .map(|v| v.to_string())
        .or_else(|_| env.var("VAPID_PRIVATE_KEY").map(|v| v.to_string()));
    let Ok(value) = value else {
        return Ok(None);
    };
    let key = B64
        .decode(value)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| push_error(webpush::PushError::BadKey))?;
    webpush::vapid_public_key(key).map_err(push_error)?;
    Ok(Some(key))
}

fn push_key_bytes(value: &Value) -> Result<Vec<u8>> {
    value
        .as_str()
        .and_then(|s| B64.decode(s).ok())
        .ok_or_else(|| push_error(webpush::PushError::BadKey))
}

fn localhost_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
}

fn push_endpoint_allowed(endpoint: &str, env: &Env) -> Result<bool> {
    let Ok(url) = Url::parse(endpoint) else {
        return Ok(false);
    };
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Ok(false);
    }
    if url.scheme() == "https" {
        return Ok(true);
    }
    let local_origin = env
        .var("ORIGIN")
        .ok()
        .and_then(|v| Url::parse(&v.to_string()).ok())
        .is_some_and(|u| localhost_url(&u));
    Ok(url.scheme() == "http"
        && localhost_url(&url)
        && (local_origin || util::insecure_dev_allowed(env)?))
}

#[derive(Deserialize, Serialize)]
struct Notification {
    title: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<anila_proto::SessionName>,
}

async fn notify(env: &Env, host: &str, body: Value) -> Result<Response> {
    let notification = match serde_json::from_value::<Notification>(body) {
        Ok(value) if value.title.chars().count() <= 100 && value.body.chars().count() <= 500 => {
            value
        }
        _ => return util::json_error(400, "bad_frame", "invalid notification"),
    };
    let Some(key) = vapid_key(env)? else {
        return push_unconfigured();
    };
    let subject = env
        .var("VAPID_SUBJECT")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "mailto:admin@localhost".into());
    let mut response = call_auth(env, "agent/recipients", &json!({"host": host})).await?;
    if response.status_code() != 200 {
        return Ok(response);
    }
    let recipients: Value = response.json().await?;
    let subscriptions = recipients["subscriptions"]
        .as_array()
        .ok_or_else(|| Error::RustError("missing push subscriptions".into()))?;
    let mut payload = serde_json::to_value(&notification)?;
    payload["host"] = json!(host);
    payload["host_name"] = json!(required_str(&recipients, "name")?);
    let payload = serde_json::to_vec(&payload)?;
    let (mut sent, mut removed, mut failed) = (0, 0, 0);
    for subscription in subscriptions {
        match send_push(subscription, &payload, &subject, key).await {
            Ok(404 | 410) => {
                match call_auth(
                    env,
                    "push/remove",
                    &json!({"endpoint": subscription["endpoint"]}),
                )
                .await
                {
                    Ok(response) if response.status_code() == 204 => removed += 1,
                    _ => failed += 1,
                }
            }
            Ok(200..=299) => sent += 1,
            _ => failed += 1,
        }
    }
    Response::from_json(&json!({"sent": sent, "removed": removed, "failed": failed}))
}

async fn send_push(
    subscription: &Value,
    payload: &[u8],
    subject: &str,
    key: [u8; 32],
) -> Result<u16> {
    let endpoint = subscription["endpoint"]
        .as_str()
        .ok_or_else(|| push_error(webpush::PushError::BadEndpoint))?;
    let public = push_key_bytes(&subscription["p256dh"])?;
    let auth = push_key_bytes(&subscription["auth"])?;
    let encrypted = webpush::encrypt(
        payload,
        &public,
        &auth,
        util::random_bytes(),
        util::random_bytes(),
    )
    .map_err(push_error)?;
    let headers = Headers::new();
    headers.set("Content-Encoding", "aes128gcm")?;
    headers.set("Content-Type", "application/octet-stream")?;
    headers.set("TTL", "86400")?;
    headers.set("Urgency", "high")?;
    headers.set(
        "Authorization",
        &webpush::vapid_header(endpoint, subject, key, util::now_secs() + 12 * 60 * 60)
            .map_err(push_error)?,
    )?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_headers(headers);
    init.with_body(Some(js_sys::Uint8Array::from(encrypted.as_slice()).into()));
    let request = Request::new_with_init(endpoint, &init)?;
    Ok(Fetch::Request(request).send().await?.status_code())
}
