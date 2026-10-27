//! SQLite-backed accounts, passkeys, machines and one-time ceremonies.

use anila_proto::{
    HostId,
    api::{CodeKind, Role},
    limits::{
        CHALLENGE_SECS, MAX_ACCOUNTS, MAX_HOSTS_PER_ACCOUNT, MAX_MEMBERS_PER_HOST,
        MAX_PASSKEYS_PER_ACCOUNT, SESSION_SECS, STEP_UP_SECS,
    },
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use worker::*;

use crate::{
    agent_sig, auth_core as core,
    tenancy::{self, Action},
    util, webauthn,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, is_admin INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS credentials (
  id TEXT PRIMARY KEY, public_key TEXT NOT NULL, alg INTEGER NOT NULL,
  sign_count INTEGER NOT NULL, label TEXT NOT NULL,
  created_at INTEGER NOT NULL, last_used_at INTEGER, account_id TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
  sid_hash TEXT PRIMARY KEY,
  cred_id TEXT NOT NULL REFERENCES credentials(id) ON DELETE CASCADE,
  auth_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hosts (
  id TEXT PRIMARY KEY, owner_id TEXT, name TEXT NOT NULL,
  public_key TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS members (
  host_id TEXT NOT NULL, account_id TEXT NOT NULL, created_at INTEGER NOT NULL,
  PRIMARY KEY (host_id, account_id)
);
CREATE TABLE IF NOT EXISTS codes (
  code_hash TEXT PRIMARY KEY, kind TEXT NOT NULL, account_id TEXT,
  host_id TEXT, expires_at INTEGER NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS flows (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, challenge TEXT NOT NULL,
  expires_at INTEGER NOT NULL, code_hash TEXT, user_id TEXT, code_kind TEXT
);
CREATE TABLE IF NOT EXISTS push_subs (
  endpoint TEXT PRIMARY KEY, p256dh TEXT NOT NULL, auth TEXT NOT NULL,
  sid_hash TEXT, created_at INTEGER NOT NULL, account_id TEXT
);
CREATE TABLE IF NOT EXISTS code_failures (minute INTEGER PRIMARY KEY, count INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
";

#[derive(Deserialize)]
struct Flow {
    kind: String,
    challenge: String,
    expires_at: i64,
    code_hash: Option<String>,
    code_kind: Option<String>,
    user_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Session {
    auth_at: i64,
    expires_at: i64,
    account_id: String,
}

#[derive(Deserialize)]
struct Credential {
    public_key: String,
    sign_count: u32,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
struct Meta {
    v: String,
}

#[derive(Deserialize)]
struct Account {
    id: String,
    name: String,
    is_admin: i64,
}

#[derive(Deserialize)]
struct Host {
    id: String,
    name: String,
    owner_id: Option<String>,
    owner_name: String,
    public_key: String,
}

impl Host {
    fn view(&self, role: Role) -> Value {
        json!({"id": self.id, "name": self.name, "role": role, "owner_name": self.owner_name})
    }
}

#[derive(Deserialize)]
struct Code {
    code_hash: String,
    kind: String,
    account_id: Option<String>,
    host_id: Option<String>,
}

// All mutations finish before the first room call yields to another request.
struct Revocation {
    hosts: Vec<String>,
    body: Value,
}

/// The one auth Durable Object, addressed by the name `auth`.
#[durable_object]
pub struct AuthStore {
    state: State,
    env: Env,
}

impl DurableObject for AuthStore {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        match self.handle(req).await {
            Ok(response) => Ok(response),
            Err(_) => {
                // SQL/runtime errors may contain bound secrets; do not log them.
                console_error!("auth store request failed");
                util::json_error(500, "internal", "auth store failed")
            }
        }
    }
}

impl AuthStore {
    fn exec(&self, sql: &str, args: Vec<SqlStorageValue>) -> Result<SqlCursor> {
        self.state.storage().sql().exec(sql, args)
    }

    fn rows<T: DeserializeOwned>(&self, sql: &str, args: Vec<SqlStorageValue>) -> Result<Vec<T>> {
        self.exec(sql, args)?.to_array::<T>()
    }

    fn first<T: DeserializeOwned>(
        &self,
        sql: &str,
        args: Vec<SqlStorageValue>,
    ) -> Result<Option<T>> {
        Ok(self.rows(sql, args)?.into_iter().next())
    }

    fn ids(&self, sql: &str, args: Vec<SqlStorageValue>) -> Result<Vec<String>> {
        Ok(self
            .rows::<Id>(sql, args)?
            .into_iter()
            .map(|r| r.id)
            .collect())
    }

    fn count(&self, sql: &str, args: Vec<SqlStorageValue>) -> Result<usize> {
        #[derive(Deserialize)]
        struct Count {
            n: usize,
        }
        self.first::<Count>(sql, args)?
            .map(|r| r.n)
            .ok_or_else(|| Error::RustError("missing count".into()))
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .first::<Meta>("SELECT v FROM meta WHERE k = ?", vec![key.into()])?
            .map(|r| r.v))
    }

    fn initialize(&self, now: i64) -> Result<()> {
        let meta_exists = !self
            .ids(
                "SELECT name AS id FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
                vec![],
            )?
            .is_empty();
        if meta_exists && !core::store_schema_supported(self.meta("schema")?.as_deref()) {
            return Err(Error::RustError(
                "unsupported auth store schema: use a fresh open-source deployment".into(),
            ));
        }
        self.state.storage().sql().exec(SCHEMA, None)?;
        #[derive(Deserialize)]
        struct Column {
            name: String,
        }
        for (table, column) in [
            ("credentials", "account_id"),
            ("push_subs", "account_id"),
            ("flows", "code_hash"),
            ("flows", "user_id"),
            ("flows", "code_kind"),
        ] {
            if !self
                .rows::<Column>(&format!("PRAGMA table_info({table})"), vec![])?
                .iter()
                .any(|c| c.name == column)
            {
                self.exec(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"),
                    vec![],
                )?;
            }
        }
        self.exec(
            "INSERT OR IGNORE INTO meta (k, v) VALUES ('user_id', ?)",
            vec![URL_SAFE_NO_PAD.encode(util::random_bytes::<16>()).into()],
        )?;
        if self.meta("schema")?.as_deref() != Some("2") {
            self.exec("DROP TABLE IF EXISTS enroll_codes", vec![])?;
            if self.account_count()? == 0
                && self.count("SELECT COUNT(*) AS n FROM credentials", vec![])? > 0
            {
                self.exec("INSERT INTO accounts (id, name, is_admin, created_at) SELECT v, 'Owner', 1, ? FROM meta WHERE k = 'user_id'", vec![now.into()])?;
                for table in ["credentials", "push_subs"] {
                    self.exec(&format!("UPDATE {table} SET account_id = (SELECT v FROM meta WHERE k = 'user_id') WHERE account_id IS NULL"), vec![])?;
                }
            }
            self.exec(
                "INSERT OR REPLACE INTO meta (k, v) VALUES ('schema', '2')",
                vec![],
            )?;
        }
        let keys = self.env.var("HOST_KEYS")?.to_string();
        let key_hash = core::hash_sid(&keys);
        if self.meta("host_keys")?.as_deref() != Some(&key_hash) {
            agent_sig::HostKeys::parse(&keys).map_err(Error::RustError)?;
            let pairs: std::collections::BTreeMap<String, String> = serde_json::from_str(&keys)?;
            for (name, key) in pairs {
                self.exec("INSERT INTO hosts (id, name, public_key, owner_id, created_at) VALUES (?, ?, ?, (SELECT id FROM accounts WHERE is_admin = 1 ORDER BY created_at, id LIMIT 1), ?) ON CONFLICT(id) DO UPDATE SET public_key = excluded.public_key",
                    vec![name.clone().into(), name.into(), key.into(), now.into()])?;
            }
            self.exec(
                "INSERT OR REPLACE INTO meta (k, v) VALUES ('host_keys', ?)",
                vec![key_hash.into()],
            )?;
        }
        self.exec("UPDATE hosts SET owner_id = (SELECT id FROM accounts WHERE is_admin = 1 ORDER BY created_at, id LIMIT 1) WHERE owner_id IS NULL", vec![])?;
        for table in ["flows", "codes", "sessions"] {
            self.exec(
                &format!("DELETE FROM {table} WHERE expires_at <= ?"),
                vec![now.into()],
            )?;
        }
        self.exec(
            "DELETE FROM code_failures WHERE minute < ?",
            vec![tenancy::failure_minute(now).into()],
        )?;
        Ok(())
    }

    fn account_count(&self) -> Result<usize> {
        self.count("SELECT COUNT(*) AS n FROM accounts", vec![])
    }

    fn account(&self, id: &str) -> Result<Option<Account>> {
        self.first(
            "SELECT id, name, is_admin FROM accounts WHERE id = ?",
            vec![id.into()],
        )
    }

    fn session(&self, hash: &str, now: i64) -> Result<Option<Session>> {
        Ok(self.first::<Session>("SELECT s.auth_at, s.expires_at, c.account_id FROM sessions s JOIN credentials c ON c.id = s.cred_id JOIN accounts a ON a.id = c.account_id WHERE s.sid_hash = ?", vec![hash.into()])?
            .filter(|s| core::session_valid(now, s.expires_at)))
    }

    fn host(&self, id: &str) -> Result<Option<Host>> {
        self.first("SELECT h.id, h.name, h.owner_id, h.public_key, COALESCE(a.name, '') AS owner_name FROM hosts h LEFT JOIN accounts a ON a.id = h.owner_id WHERE h.id = ?", vec![id.into()])
    }

    fn role(&self, host: &Host, account: &str) -> Result<Option<Role>> {
        let members = self.ids(
            "SELECT account_id AS id FROM members WHERE host_id = ? AND account_id = ?",
            vec![host.id.as_str().into(), account.into()],
        )?;
        Ok(tenancy::access(
            host.owner_id.as_deref(),
            &members.iter().map(String::as_str).collect::<Vec<_>>(),
            account,
        ))
    }

    fn accessible_hosts(&self, account: &str) -> Result<Vec<String>> {
        self.ids("SELECT h.id FROM hosts h WHERE h.owner_id = ? OR EXISTS (SELECT 1 FROM members m WHERE m.host_id = h.id AND m.account_id = ?) ORDER BY CASE WHEN h.owner_id = ? THEN 0 ELSE 1 END, h.name, h.id", vec![account.into(), account.into(), account.into()])
    }

    fn passkey_ids(&self, account: &str) -> Result<Vec<String>> {
        self.ids(
            "SELECT id FROM credentials WHERE account_id = ? ORDER BY created_at, id",
            vec![account.into()],
        )
    }

    fn owned_count(&self, account: &str) -> Result<usize> {
        self.count(
            "SELECT COUNT(*) AS n FROM hosts WHERE owner_id = ?",
            vec![account.into()],
        )
    }

    fn code(&self, hash: &str, now: i64) -> Result<Option<Code>> {
        self.first("SELECT code_hash, kind, account_id, host_id FROM codes WHERE code_hash = ? AND expires_at > ?", vec![hash.into(), now.into()])
    }

    fn lookup_code(&self, input: &str, now: i64) -> Result<Option<Code>> {
        match core::normalize_enroll_code(input) {
            Some(code) => self.code(&core::hash_code(&code), now),
            None => Ok(None),
        }
    }

    fn blocked(&self, now: i64) -> Result<bool> {
        #[derive(Deserialize)]
        struct Failures {
            count: u32,
        }
        Ok(self
            .first::<Failures>(
                "SELECT count FROM code_failures WHERE minute = ?",
                vec![tenancy::failure_minute(now).into()],
            )?
            .is_some_and(|r| tenancy::redemptions_blocked(r.count)))
    }

    fn failed_code(&self, now: i64) -> Result<()> {
        self.exec("INSERT INTO code_failures (minute, count) VALUES (?, 1) ON CONFLICT(minute) DO UPDATE SET count = count + 1", vec![tenancy::failure_minute(now).into()])?;
        Ok(())
    }

    fn issue_code(
        &self,
        kind: CodeKind,
        account: Option<&str>,
        host: Option<&str>,
        now: i64,
    ) -> Result<Response> {
        loop {
            let code = core::enroll_code_from_bytes(util::random_bytes());
            let normalized = core::normalize_enroll_code(&code)
                .ok_or_else(|| Error::RustError("invalid generated code".into()))?;
            let hash = core::hash_code(&normalized);
            if self.code(&hash, now)?.is_some() {
                continue;
            }
            let expires_at = now + tenancy::code_lifetime(kind) as i64;
            self.exec("INSERT INTO codes (code_hash, kind, account_id, host_id, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?)", vec![hash.into(), tenancy::code_kind_str(kind).into(), account.into(), host.into(), expires_at.into(), now.into()])?;
            return Response::from_json(&anila_proto::api::CodeIssued { code, expires_at });
        }
    }

    fn new_session(&self, id: &str, now: i64) -> Result<Value> {
        let sid = core::sid_from_bytes(util::random_bytes());
        let expires_at = now + SESSION_SECS as i64;
        self.exec(
            "INSERT INTO sessions (sid_hash, cred_id, auth_at, expires_at) VALUES (?, ?, ?, ?)",
            vec![sid.hash.into(), id.into(), now.into(), expires_at.into()],
        )?;
        Ok(json!({"sid": sid.value, "expires_at": expires_at}))
    }

    fn new_flow(
        &self,
        kind: &str,
        code: Option<&Code>,
        user: Option<&str>,
        now: i64,
    ) -> Result<(String, [u8; 32])> {
        let id = URL_SAFE_NO_PAD.encode(util::random_bytes::<16>());
        let challenge = util::random_bytes::<32>();
        self.exec("INSERT INTO flows (id, kind, challenge, expires_at, code_hash, code_kind, user_id) VALUES (?, ?, ?, ?, ?, ?, ?)", vec![id.clone().into(), kind.into(), URL_SAFE_NO_PAD.encode(challenge).into(), (now + CHALLENGE_SECS as i64).into(), code.map(|c| c.code_hash.as_str()).into(), code.map(|c| c.kind.as_str()).into(), user.into()])?;
        Ok((id, challenge))
    }

    fn take_flow(&self, id: &str, kind: &str, now: i64) -> Result<Option<Flow>> {
        Ok(self.first::<Flow>("DELETE FROM flows WHERE id = ? RETURNING kind, challenge, expires_at, code_hash, code_kind, user_id", vec![id.into()])?
            .filter(|f| f.kind == kind && now < f.expires_at))
    }

    async fn revoke(&self, revocation: Revocation) -> Result<()> {
        let namespace = self.env.durable_object("HOST_ROOM")?;
        for host in revocation.hosts {
            let response = namespace
                .id_from_name(&host)?
                .get_stub()?
                .fetch_with_request(util::json_request(
                    "https://room/internal/revoke",
                    &revocation.body,
                )?)
                .await?;
            if response.status_code() != 200 {
                return Err(Error::RustError("room revocation failed".into()));
            }
        }
        Ok(())
    }

    fn remove_host(&self, host: &str, revocations: &mut Vec<Revocation>) -> Result<()> {
        revocations.push(Revocation {
            hosts: vec![host.into()],
            body: json!({"host_removed": true}),
        });
        self.exec("DELETE FROM members WHERE host_id = ?", vec![host.into()])?;
        self.exec("DELETE FROM codes WHERE host_id = ?", vec![host.into()])?;
        self.exec("DELETE FROM hosts WHERE id = ?", vec![host.into()])?;
        Ok(())
    }

    async fn handle(&self, mut req: Request) -> Result<Response> {
        if req.method() != Method::Post {
            return util::json_error(405, "bad_frame", "expected POST");
        }
        let body = match req.json::<Value>().await {
            Ok(value) if value.is_object() => value,
            _ => return util::json_error(400, "bad_frame", "expected a JSON object"),
        };
        let now = util::now_secs();
        self.initialize(now)?;
        let mut revocations = Vec::new();
        let response = self.dispatch(&req.path(), &body, now, &mut revocations)?;
        for revocation in revocations {
            self.revoke(revocation).await?;
        }
        Ok(response)
    }

    fn register_options(&self, body: &Value, now: i64) -> Result<Response> {
        let mut code = None;
        if body.get("code").is_some() {
            if self.blocked(now)? {
                return too_many();
            }
            code = self.lookup_code(field(body, "code"), now)?;
            let valid = match code.as_ref() {
                Some(c) if c.kind == "device" => match c.account_id.as_deref() {
                    Some(id) => self.account(id)?.is_some(),
                    None => self.account_count()? == 0,
                },
                Some(c) if c.kind == "invite" => true,
                _ => false,
            };
            if !valid {
                self.failed_code(now)?;
                code = None;
            }
        }
        let (user, name) = match code.as_ref() {
            Some(c) if c.kind == "device" && c.account_id.is_some() => {
                let account = self
                    .account(c.account_id.as_deref().unwrap_or_default())?
                    .ok_or_else(|| Error::RustError("missing device account".into()))?;
                (account.id, account.name)
            }
            Some(c) => {
                if c.kind == "invite" && self.account_count()? >= MAX_ACCOUNTS {
                    return limit("too many accounts");
                }
                (
                    tenancy::account_id_from_bytes(util::random_bytes()),
                    "anila".into(),
                )
            }
            None => {
                let Some(session) = self.session(field(body, "sid_hash"), now)? else {
                    return unauthorized();
                };
                if !core::step_up_ok(now, session.auth_at, STEP_UP_SECS) {
                    return step_up();
                }
                let account = self
                    .account(&session.account_id)?
                    .ok_or_else(|| Error::RustError("missing session account".into()))?;
                (account.id, account.name)
            }
        };
        let credentials = self.passkey_ids(&user)?;
        if credentials.len() >= MAX_PASSKEYS_PER_ACCOUNT {
            return limit("too many passkeys");
        }
        let exclude = credentials
            .iter()
            .map(|id| decode(id))
            .collect::<Result<Vec<_>>>()?;
        let (flow, challenge) = self.new_flow("register", code.as_ref(), Some(&user), now)?;
        Response::from_json(&json!({"flow": flow, "publicKey": core::creation_options(
            &challenge, &self.env.var("RP_ID")?.to_string(), "anila", &decode(&user)?, &name, &exclude)}))
    }

    fn verify(&self, register: bool, body: &Value, now: i64) -> Result<Response> {
        let kind = if register { "register" } else { "login" };
        let Some(flow) = self.take_flow(field(body, "flow"), kind, now)? else {
            return unauthorized();
        };
        let challenge = decode(&flow.challenge)?;
        let origin = self.env.var("ORIGIN")?.to_string();
        let rp_id = self.env.var("RP_ID")?.to_string();
        let expect = webauthn::Expect {
            challenge: &challenge,
            origin: &origin,
            rp_id: &rp_id,
        };
        if register {
            let Some(user) = flow.user_id.as_deref() else {
                return unauthorized();
            };
            let existing = self.account(user)?;
            let invite = flow.code_kind.as_deref() == Some("invite");
            let bootstrap = flow.code_kind.as_deref() == Some("device") && existing.is_none();
            let name = if invite || bootstrap {
                let input = match body.get("account_name") {
                    None if bootstrap => Some("Owner"),
                    Some(v) => v.as_str(),
                    None => None,
                };
                let Some(name) = input.and_then(tenancy::normalize_account_name) else {
                    return util::json_error(400, "invalid_name", "invalid account name");
                };
                Some(name)
            } else {
                None
            };
            if let Some(hash) = flow.code_hash.as_deref() {
                let Some(code) = self.code(hash, now)? else {
                    return unauthorized();
                };
                if Some(code.kind.as_str()) != flow.code_kind.as_deref() {
                    return unauthorized();
                }
                match code.kind.as_str() {
                    "device" => match code.account_id.as_deref() {
                        Some(id) if id == user && existing.is_some() => {}
                        None if bootstrap && self.account_count()? == 0 => {}
                        _ => return unauthorized(),
                    },
                    "invite" if existing.is_none() => {
                        if self.account_count()? >= MAX_ACCOUNTS {
                            return limit("too many accounts");
                        }
                    }
                    _ => return unauthorized(),
                }
                if self.passkey_ids(user)?.len() >= MAX_PASSKEYS_PER_ACCOUNT {
                    return limit("too many passkeys");
                }
                self.exec("DELETE FROM codes WHERE code_hash = ?", vec![hash.into()])?;
            } else {
                // The handle was bound to a stepped-up session when options were issued.
                if existing.is_none() || flow.code_kind.is_some() {
                    return unauthorized();
                }
                if self.passkey_ids(user)?.len() >= MAX_PASSKEYS_PER_ACCOUNT {
                    return limit("too many passkeys");
                }
            }
            let parsed = match core::parse_registration(&body["credential"]) {
                Ok(parsed) => parsed,
                Err(error) => return util::json_error(400, "bad_frame", &error),
            };
            let credential = match webauthn::verify_registration(
                &parsed.client_data_json,
                &parsed.attestation_object,
                &expect,
            ) {
                Ok(credential) => credential,
                Err(error) => {
                    return util::json_error(400, "bad_frame", &format!("{error:?}"));
                }
            };
            if credential.id != parsed.id {
                return util::json_error(400, "bad_frame", "credential id mismatch");
            }
            let id = URL_SAFE_NO_PAD.encode(credential.id);
            if !self
                .rows::<Id>(
                    "SELECT id FROM credentials WHERE id = ?",
                    vec![id.clone().into()],
                )?
                .is_empty()
            {
                return util::json_error(400, "bad_frame", "credential already registered");
            }
            let label: String = body["label"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(64)
                .collect();
            let label = if label.is_empty() { "passkey" } else { &label };
            if let Some(name) = name {
                self.exec(
                    "INSERT INTO accounts (id, name, is_admin, created_at) VALUES (?, ?, ?, ?)",
                    vec![
                        user.into(),
                        name.into(),
                        i64::from(bootstrap).into(),
                        now.into(),
                    ],
                )?;
                if bootstrap {
                    self.exec(
                        "UPDATE hosts SET owner_id = ? WHERE owner_id IS NULL",
                        vec![user.into()],
                    )?;
                }
            }
            self.exec("INSERT INTO credentials (id, public_key, alg, sign_count, label, created_at, account_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
                        vec![id.clone().into(), URL_SAFE_NO_PAD.encode(credential.public_key_cose).into(),
                            credential.alg.into(), i64::from(credential.sign_count).into(), label.into(), now.into(), user.into()])?;
            let mut session = self.new_session(&id, now)?;
            session["id"] = json!(id);
            Response::from_json(&session)
        } else {
            let parsed = match core::parse_authentication(&body["credential"]) {
                Ok(parsed) => parsed,
                Err(error) => return util::json_error(401, "unauthorized", &error),
            };
            let id = URL_SAFE_NO_PAD.encode(parsed.id);
            let Some(credential) = self
                .rows::<Credential>(
                    "SELECT public_key, sign_count FROM credentials WHERE id = ?",
                    vec![id.clone().into()],
                )?
                .into_iter()
                .next()
            else {
                return unauthorized();
            };
            let count = match webauthn::verify_assertion(
                &parsed.client_data_json,
                &parsed.authenticator_data,
                &parsed.signature,
                &decode(&credential.public_key)?,
                credential.sign_count,
                &expect,
            ) {
                Ok(count) => count,
                Err(error) => {
                    return util::json_error(401, "unauthorized", &format!("{error:?}"));
                }
            };
            self.exec(
                "UPDATE credentials SET sign_count = ?, last_used_at = ? WHERE id = ?",
                vec![i64::from(count).into(), now.into(), id.clone().into()],
            )?;
            Response::from_json(&self.new_session(&id, now)?)
        }
    }

    fn dispatch(
        &self,
        path: &str,
        body: &Value,
        now: i64,
        revocations: &mut Vec<Revocation>,
    ) -> Result<Response> {
        match path {
            "/login/options" => {
                let (flow, challenge) = self.new_flow("login", None, None, now)?;
                return Response::from_json(
                    &json!({"flow": flow, "publicKey": core::request_options(&challenge, &self.env.var("RP_ID")?.to_string())}),
                );
            }
            "/register/options" => return self.register_options(body, now),
            "/register/verify" | "/login/verify" => {
                return self.verify(path == "/register/verify", body, now);
            }
            "/codes/check" => {
                if self.blocked(now)? {
                    return too_many();
                }
                if let Some(code) = self.lookup_code(field(body, "code"), now)? {
                    let valid = match code.kind.as_str() {
                        "invite" => true,
                        "device" => match code.account_id.as_deref() {
                            Some(id) => self.account(id)?.is_some(),
                            None => self.account_count()? == 0,
                        },
                        _ => false,
                    };
                    if valid {
                        return Response::from_json(&json!({"kind": code.kind}));
                    }
                }
                self.failed_code(now)?;
                return not_found();
            }
            "/agent/pair" => return self.pair(body, now),
            "/agent/enroll-code" | "/agent/revoke-all" | "/agent/recipients" | "/hosts/key" => {
                return self.agent(path, body, now, revocations);
            }
            "/push/remove" => {
                let Some(endpoint) = body["endpoint"].as_str() else {
                    return util::json_error(400, "bad_frame", "missing endpoint");
                };
                self.exec(
                    "DELETE FROM push_subs WHERE endpoint = ?",
                    vec![endpoint.into()],
                )?;
                return Response::empty().map(|r| r.with_status(204));
            }
            _ => {}
        }
        let hash = field(body, "sid_hash");
        if path == "/logout" {
            if let Some(session) = self.session(hash, now)? {
                revocations.push(Revocation {
                    hosts: self.accessible_hosts(&session.account_id)?,
                    body: json!({"sid_hash": hash}),
                });
            }
            self.exec("DELETE FROM sessions WHERE sid_hash = ?", vec![hash.into()])?;
            return empty();
        }
        if !matches!(
            path,
            "/session"
                | "/me"
                | "/passkeys"
                | "/passkeys/delete"
                | "/codes/create"
                | "/share/redeem"
                | "/hosts/list"
                | "/hosts/rename"
                | "/hosts/remove"
                | "/hosts/members"
                | "/hosts/members/remove"
                | "/access"
                | "/admin/accounts"
                | "/admin/accounts/delete"
                | "/push/subscribe"
                | "/push/unsubscribe"
        ) {
            return util::json_error(404, "not_found", "unknown auth route");
        }
        let Some(session) = self.session(hash, now)? else {
            return unauthorized();
        };
        let account = session.account_id.as_str();
        if matches!(
            path,
            "/passkeys/delete" | "/codes/create" | "/hosts/remove" | "/admin/accounts/delete"
        ) && !core::step_up_ok(now, session.auth_at, STEP_UP_SECS)
        {
            return step_up();
        }
        match path {
            "/session" => Response::from_json(&session),
            "/me" => {
                let Some(a) = self.account(account)? else { return unauthorized(); };
                Response::from_json(&anila_proto::api::Me {
                    account: anila_proto::api::AccountInfo { id: a.id, name: a.name, admin: a.is_admin != 0 },
                    auth_at: session.auth_at, expires_at: session.expires_at,
                })
            }
            "/passkeys" => Response::from_json(&self.rows::<Value>("SELECT id, label, created_at, last_used_at FROM credentials WHERE account_id = ? ORDER BY created_at, id", vec![account.into()])?),
            "/passkeys/delete" => {
                let id = field(body, "id");
                let credentials = self.passkey_ids(account)?;
                if !credentials.iter().any(|c| c == id) { return not_found(); }
                if credentials.len() <= 1 { return limit("cannot delete the last passkey"); }
                let hosts = self.accessible_hosts(account)?;
                for sid in self.ids("SELECT sid_hash AS id FROM sessions WHERE cred_id = ?", vec![id.into()])? {
                    revocations.push(Revocation { hosts: hosts.clone(), body: json!({"sid_hash": sid}) });
                }
                self.exec("DELETE FROM sessions WHERE cred_id = ?", vec![id.into()])?;
                self.exec("DELETE FROM credentials WHERE id = ?", vec![id.into()])?;
                empty()
            }
            "/codes/create" => {
                let Some(kind) = tenancy::code_kind_parse(field(body, "kind")) else { return util::json_error(400, "bad_frame", "unknown code kind"); };
                let mut host_id = None;
                match kind {
                    CodeKind::Device => {},
                    CodeKind::Invite => {
                        if !self.account(account)?.is_some_and(|a| a.is_admin != 0) { return forbidden(); }
                    }
                    CodeKind::Pair => {
                        if self.owned_count(account)? >= MAX_HOSTS_PER_ACCOUNT { return limit("too many machines"); }
                    }
                    CodeKind::Share => {
                        let Some(host) = self.host(field(body, "host"))? else { return not_found(); };
                        let Some(role) = self.role(&host, account)? else { return not_found(); };
                        if !tenancy::allowed(role, Action::CreateShareCode) { return forbidden(); }
                        host_id = Some(host.id);
                    }
                }
                self.issue_code(kind, Some(account), host_id.as_deref(), now)
            }
            "/share/redeem" => {
                if self.blocked(now)? { return too_many(); }
                let code = self.lookup_code(field(body, "code"), now)?.filter(|c| c.kind == "share");
                let Some(code) = code else { self.failed_code(now)?; return not_found(); };
                let Some(host) = self.host(code.host_id.as_deref().unwrap_or_default())? else { self.failed_code(now)?; return not_found(); };
                if self.role(&host, account)?.is_some() { return exists("already has access"); }
                if self.count("SELECT COUNT(*) AS n FROM members WHERE host_id = ?", vec![host.id.as_str().into()])? >= MAX_MEMBERS_PER_HOST { return limit("too many members"); }
                self.exec("INSERT INTO members (host_id, account_id, created_at) VALUES (?, ?, ?)", vec![host.id.as_str().into(), account.into(), now.into()])?;
                self.exec("DELETE FROM codes WHERE code_hash = ?", vec![code.code_hash.into()])?;
                Response::from_json(&host.view(Role::Member))
            }
            "/hosts/list" => {
                let mut hosts = Vec::new();
                for id in self.accessible_hosts(account)? {
                    if let Some(host) = self.host(&id)? && let Some(role) = self.role(&host, account)? { hosts.push(host.view(role)); }
                }
                Response::from_json(&hosts)
            }
            "/access" => {
                let id = match body.get("host") {
                    Some(value) => value.as_str().unwrap_or_default().to_owned(),
                    None => {
                        let hosts = self.accessible_hosts(account)?;
                        if hosts.len() != 1 { return util::json_error(400, "bad_frame", "name a machine"); }
                        hosts[0].clone()
                    }
                };
                let Some(host) = self.host(&id)? else { return not_found(); };
                let Some(role) = self.role(&host, account)? else { return not_found(); };
                Response::from_json(&json!({"account_id": account, "expires_at": session.expires_at, "host": host.id, "host_name": host.name, "role": role}))
            }
            "/hosts/rename" | "/hosts/remove" | "/hosts/members" | "/hosts/members/remove" => self.manage_host(path, body, account, revocations),
            "/admin/accounts" | "/admin/accounts/delete" => self.admin(path, body, account, revocations),
            "/push/subscribe" | "/push/unsubscribe" => self.push(path, body, account, hash, now),
            _ => util::json_error(404, "not_found", "unknown auth route"),
        }
    }

    fn manage_host(
        &self,
        path: &str,
        body: &Value,
        account: &str,
        revocations: &mut Vec<Revocation>,
    ) -> Result<Response> {
        let Some(mut host) = self.host(field(body, "host"))? else {
            return not_found();
        };
        let Some(role) = self.role(&host, account)? else {
            return not_found();
        };
        let member = match field(body, "account") {
            "me" => account,
            id => id,
        };
        let action = match path {
            "/hosts/rename" => Action::Rename,
            "/hosts/remove" => Action::RemoveMachine,
            "/hosts/members" => Action::ListMembers,
            _ => Action::RemoveMember {
                own: member == account,
            },
        };
        if !tenancy::allowed(role, action) {
            return forbidden();
        }
        match path {
            "/hosts/rename" => {
                let Ok(name) = HostId::new(field(body, "name")) else {
                    return util::json_error(400, "invalid_name", "invalid machine name");
                };
                if self.count("SELECT COUNT(*) AS n FROM hosts WHERE owner_id IS ? AND name = ? AND id != ?",
                    vec![host.owner_id.as_deref().into(), name.as_str().into(), host.id.as_str().into()])? > 0 {
                    return exists("machine name already exists");
                }
                self.exec("UPDATE hosts SET name = ? WHERE id = ?", vec![name.as_str().into(), host.id.as_str().into()])?;
                host.name = name.into();
                Response::from_json(&host.view(role))
            }
            "/hosts/remove" => {
                self.remove_host(&host.id, revocations)?;
                empty()
            }
            "/hosts/members" => Response::from_json(&self.rows::<anila_proto::api::Member>(
                "SELECT m.account_id, a.name, m.created_at AS since FROM members m JOIN accounts a ON a.id = m.account_id WHERE m.host_id = ? ORDER BY m.created_at, m.account_id",
                vec![host.id.into()])?),
            _ => {
                if self.count("SELECT COUNT(*) AS n FROM members WHERE host_id = ? AND account_id = ?",
                    vec![host.id.as_str().into(), member.into()])? == 0 { return not_found(); }
                self.exec("DELETE FROM members WHERE host_id = ? AND account_id = ?", vec![host.id.as_str().into(), member.into()])?;
                revocations.push(Revocation { hosts: vec![host.id], body: json!({"account": member}) });
                empty()
            }
        }
    }

    fn admin(
        &self,
        path: &str,
        body: &Value,
        account: &str,
        revocations: &mut Vec<Revocation>,
    ) -> Result<Response> {
        if !self.account(account)?.is_some_and(|a| a.is_admin != 0) {
            return forbidden();
        }
        if path == "/admin/accounts" {
            #[derive(Deserialize)]
            struct Row {
                id: String,
                name: String,
                is_admin: i64,
                created_at: i64,
                hosts: u32,
                passkeys: u32,
            }
            let accounts = self.rows::<Row>("SELECT a.id, a.name, a.is_admin, a.created_at, (SELECT COUNT(*) FROM hosts h WHERE h.owner_id = a.id) AS hosts, (SELECT COUNT(*) FROM credentials c WHERE c.account_id = a.id) AS passkeys FROM accounts a ORDER BY a.created_at, a.id", vec![])?;
            return Response::from_json(
                &accounts
                    .into_iter()
                    .map(|a| anila_proto::api::AccountView {
                        id: a.id,
                        name: a.name,
                        admin: a.is_admin != 0,
                        created_at: a.created_at,
                        hosts: a.hosts,
                        passkeys: a.passkeys,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let target = field(body, "account");
        if target == account {
            return limit("cannot delete your own account");
        }
        if self.account(target)?.is_none() {
            return not_found();
        }
        let hosts = self.accessible_hosts(target)?;
        let sessions = self.ids("SELECT s.sid_hash AS id FROM sessions s JOIN credentials c ON c.id = s.cred_id WHERE c.account_id = ?", vec![target.into()])?;
        // Collect access before removing either ownership or membership.
        revocations.push(Revocation {
            hosts: hosts.clone(),
            body: json!({"account": target}),
        });
        for sid in sessions {
            revocations.push(Revocation {
                hosts: hosts.clone(),
                body: json!({"sid_hash": sid}),
            });
        }
        // Machine removal must close service sockets as well as browsers.
        let owned = self.ids(
            "SELECT id FROM hosts WHERE owner_id = ?",
            vec![target.into()],
        )?;
        for host in owned {
            self.remove_host(&host, revocations)?;
        }
        self.exec(
            "DELETE FROM members WHERE account_id = ?",
            vec![target.into()],
        )?;
        self.exec(
            "DELETE FROM push_subs WHERE account_id = ?",
            vec![target.into()],
        )?;
        self.exec("DELETE FROM sessions WHERE cred_id IN (SELECT id FROM credentials WHERE account_id = ?)", vec![target.into()])?;
        self.exec(
            "DELETE FROM credentials WHERE account_id = ?",
            vec![target.into()],
        )?;
        self.exec(
            "DELETE FROM codes WHERE account_id = ?",
            vec![target.into()],
        )?;
        self.exec("DELETE FROM accounts WHERE id = ?", vec![target.into()])?;
        empty()
    }

    fn pair(&self, body: &Value, now: i64) -> Result<Response> {
        if self.blocked(now)? {
            return too_many();
        }
        let Some(code) = self
            .lookup_code(field(body, "code"), now)?
            .filter(|c| c.kind == "pair")
        else {
            self.failed_code(now)?;
            return not_found();
        };
        let Some(account) = self.account(code.account_id.as_deref().unwrap_or_default())? else {
            self.failed_code(now)?;
            return not_found();
        };
        let public_key = field(body, "public_key");
        if self.count(
            "SELECT COUNT(*) AS n FROM hosts WHERE public_key = ?",
            vec![public_key.into()],
        )? > 0
        {
            return exists("public key already paired");
        }
        let Ok(wanted) = HostId::new(field(body, "name")) else {
            return util::json_error(400, "invalid_name", "invalid machine name");
        };
        // Codes may have been issued before another machine filled the limit.
        if self.owned_count(&account.id)? >= MAX_HOSTS_PER_ACCOUNT {
            return limit("too many machines");
        }
        let names = self.ids(
            "SELECT name AS id FROM hosts WHERE owner_id = ?",
            vec![account.id.as_str().into()],
        )?;
        let name = tenancy::unique_host_name(
            &wanted,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let host = loop {
            let id = tenancy::host_id_from_bytes(util::random_bytes());
            if self.host(id.as_str())?.is_none() {
                break id;
            }
        };
        self.exec(
            "INSERT INTO hosts (id, owner_id, name, public_key, created_at) VALUES (?, ?, ?, ?, ?)",
            vec![
                host.as_str().into(),
                account.id.into(),
                name.as_str().into(),
                public_key.into(),
                now.into(),
            ],
        )?;
        self.exec(
            "DELETE FROM codes WHERE code_hash = ?",
            vec![code.code_hash.into()],
        )?;
        Response::from_json(&anila_proto::api::Paired {
            host,
            name,
            account_name: account.name,
        })
    }

    fn agent(
        &self,
        path: &str,
        body: &Value,
        now: i64,
        revocations: &mut Vec<Revocation>,
    ) -> Result<Response> {
        let Some(host) = self.host(field(body, "host"))? else {
            return not_found();
        };
        match path {
            "/hosts/key" => Response::from_json(
                &json!({"public_key": host.public_key, "owner_id": host.owner_id}),
            ),
            "/agent/enroll-code" => {
                if host.owner_id.is_none() && self.account_count()? != 0 {
                    return exists("machine has no owner");
                }
                self.issue_code(CodeKind::Device, host.owner_id.as_deref(), None, now)
            }
            "/agent/revoke-all" => {
                revocations.push(Revocation {
                    hosts: vec![host.id.clone()],
                    body: json!({"all": true}),
                });
                if let Some(owner) = host.owner_id {
                    let hosts = self
                        .accessible_hosts(&owner)?
                        .into_iter()
                        .filter(|id| id != &host.id)
                        .collect();
                    revocations.push(Revocation {
                        hosts,
                        body: json!({"account": owner}),
                    });
                    self.exec("DELETE FROM sessions WHERE cred_id IN (SELECT id FROM credentials WHERE account_id = ?)", vec![owner.into()])?;
                }
                empty()
            }
            "/agent/recipients" => {
                let subscriptions = self.rows::<Value>("SELECT p.endpoint, p.p256dh, p.auth FROM push_subs p WHERE p.account_id = ? OR EXISTS (SELECT 1 FROM members m WHERE m.host_id = ? AND m.account_id = p.account_id) ORDER BY p.endpoint",
                    vec![host.owner_id.as_deref().into(), host.id.into()])?;
                Response::from_json(&json!({"name": host.name, "subscriptions": subscriptions}))
            }
            _ => not_found(),
        }
    }

    fn push(
        &self,
        path: &str,
        body: &Value,
        account: &str,
        hash: &str,
        now: i64,
    ) -> Result<Response> {
        let Some(endpoint) = body["endpoint"].as_str() else {
            return util::json_error(400, "bad_frame", "missing endpoint");
        };
        if path == "/push/subscribe" {
            let (Some(p256dh), Some(auth)) = (body["p256dh"].as_str(), body["auth"].as_str())
            else {
                return util::json_error(400, "bad_frame", "missing subscription keys");
            };
            self.exec("INSERT INTO push_subs (endpoint, p256dh, auth, sid_hash, created_at, account_id) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth, sid_hash = excluded.sid_hash, account_id = excluded.account_id",
                vec![endpoint.into(), p256dh.into(), auth.into(), hash.into(), now.into(), account.into()])?;
        } else {
            self.exec(
                "DELETE FROM push_subs WHERE endpoint = ? AND account_id = ?",
                vec![endpoint.into(), account.into()],
            )?;
        }
        Response::empty().map(|response| response.with_status(204))
    }
}

fn field<'a>(body: &'a Value, name: &str) -> &'a str {
    body[name].as_str().unwrap_or_default()
}

fn decode(value: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| Error::RustError(e.to_string()))
}

fn empty() -> Result<Response> {
    Response::from_json(&json!({}))
}

fn unauthorized() -> Result<Response> {
    util::json_error(401, "unauthorized", "not signed in or ceremony expired")
}

fn step_up() -> Result<Response> {
    util::json_error(401, "step_up", "sign in again")
}

fn forbidden() -> Result<Response> {
    util::json_error(403, "forbidden", "not allowed")
}

fn not_found() -> Result<Response> {
    util::json_error(404, "not_found", "not found")
}

fn exists(message: &str) -> Result<Response> {
    util::json_error(409, "exists", message)
}

fn limit(message: &str) -> Result<Response> {
    util::json_error(409, "limit", message)
}

fn too_many() -> Result<Response> {
    util::json_error(429, "limit", "too many attempts")
}
