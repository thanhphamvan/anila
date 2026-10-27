//! Signed HTTP calls for enrollment, browser sign-out and push notifications.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anila_proto::api::{PAIR_HOST, PairRequest, Paired};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::{config::Config, keys::ServiceKey};

/// A one-time code for registering the first passkey.
#[derive(Debug, Deserialize)]
pub struct EnrollCode {
    /// The code to enter in the browser.
    pub code: String,
    /// Expiration time in Unix seconds.
    pub expires_at: i64,
}

/// Requests a one-time enrollment code from the worker.
pub fn enroll(config: &Config, key: &ServiceKey) -> anyhow::Result<EnrollCode> {
    post(
        &config.worker,
        config.host.as_str(),
        &config.access_headers()?,
        key,
        "/api/agent/enroll-code",
        b"{}",
    )?
    .into_json()
    .context("invalid enrollment response from the worker")
}

/// Signs out every browser registered with this host.
pub fn revoke_all(config: &Config, key: &ServiceKey) -> anyhow::Result<()> {
    post(
        &config.worker,
        config.host.as_str(),
        &config.access_headers()?,
        key,
        "/api/agent/revoke-all",
        b"{}",
    )?;
    Ok(())
}

/// Delivery counts returned by the worker.
#[derive(Debug, Deserialize)]
pub struct NotifyResult {
    /// Devices successfully notified.
    pub sent: u64,
    /// Expired subscriptions removed.
    pub removed: u64,
    /// Deliveries that failed.
    pub failed: u64,
}

/// Sends a push notification to this host's subscribed devices.
pub fn notify(
    config: &Config,
    key: &ServiceKey,
    title: &str,
    body: &str,
    session: Option<&str>,
) -> anyhow::Result<NotifyResult> {
    #[derive(Serialize)]
    struct Notification<'a> {
        title: &'a str,
        body: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<&'a str>,
    }
    let bytes = serde_json::to_vec(&Notification {
        title,
        body,
        session,
    })?;
    post(
        &config.worker,
        config.host.as_str(),
        &config.access_headers()?,
        key,
        "/api/agent/notify",
        &bytes,
    )?
    .into_json()
    .context("invalid notification response from the worker")
}

/// Shown when Cloudflare Access answers instead of the worker: what to fix,
/// depending on whether this service sent a service token.
pub fn access_blocked(token_sent: bool) -> &'static str {
    if token_sent {
        "Cloudflare Access rejected this service token: in Zero Trust, the Access application for /api/agent/ and /ws/agent needs a Service Auth policy that includes this token (check the Client ID, and that the token is not expired or revoked)"
    } else {
        "Cloudflare Access blocked this request and no service token is set: add access_client_id = \"<Client ID>\" and access_client_secret = \"<Client Secret>\" near the top of service.toml (~/.config/anila/service.toml of the user running the service), before any [[preset]] section, or set ANILA_ACCESS_CLIENT_ID/ANILA_ACCESS_CLIENT_SECRET; then restart anila-service"
    }
}

/// Whether `location` is a Cloudflare Access sign-in redirect.
pub fn is_access_redirect(location: &str) -> bool {
    location.contains("cloudflareaccess.com")
}

fn post(
    worker: &str,
    host: &str,
    access: &[(&str, String)],
    key: &ServiceKey,
    path: &str,
    body: &[u8],
) -> anyhow::Result<ureq::Response> {
    let url = format!("{}{path}", worker.trim_end_matches('/'));
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs()
        .try_into()
        .context("system clock is outside the supported range")?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .redirects(0)
        .build();
    let mut request = agent.post(&url).set("Content-Type", "application/json");
    for (name, value) in key.signed_headers(host, "POST", path, ts, body) {
        request = request.set(name, &value);
    }
    for (name, value) in access {
        request = request.set(name, value);
    }
    let response = match request.send_bytes(body) {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(err)) => bail!("could not reach {url}: {err}"),
    };
    let status = response.status();
    if (300..400).contains(&status) && response.header("location").is_some_and(is_access_redirect) {
        bail!("{}", access_blocked(!access.is_empty()));
    }
    if status == 503 && path == "/api/agent/notify" {
        bail!(
            "Push is not configured on the worker: run `anila-service vapid-keygen` and set VAPID_PRIVATE_KEY."
        );
    }
    if path == "/api/agent/pair" {
        match status {
            404 => {
                bail!("the pairing code is wrong or has expired; create a new one in the web app")
            }
            409 => bail!(
                "this machine is already paired (its key is registered); to pair it again, remove it in the web app first"
            ),
            _ => {}
        }
    }
    if status == 401 {
        bail!(
            "the worker rejected this service's signature: this machine is not paired with the worker or its clock is off (run `anila-service pair` and check the clock)"
        );
    }
    if !(200..300).contains(&status) {
        let body = response.into_json::<serde_json::Value>().ok();
        let msg = body
            .as_ref()
            .and_then(|body| body.get("error")?.get("msg")?.as_str());
        match msg {
            Some(msg) => bail!("the worker answered {status}: {msg}"),
            None => bail!("the worker answered {status}"),
        }
    }
    Ok(response)
}

/// Pairs this machine with the account that issued the code.
pub fn pair(
    worker: &str,
    access: &[(&str, String)],
    key: &ServiceKey,
    request: &PairRequest,
) -> anyhow::Result<Paired> {
    let body = serde_json::to_vec(request)?;
    post(worker, PAIR_HOST, access, key, "/api/agent/pair", &body)?
        .into_json()
        .context("invalid pairing response from the worker")
}
