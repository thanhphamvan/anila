//! Minimal JSON HTTP client with a single session cookie.

use serde_json::Value;

/// HTTP requests carrying the browser origin and session cookie.
pub struct Http {
    base: String,
    origin: String,
    cookie: Option<String>,
    agent: ureq::Agent,
}

impl Http {
    /// Create a client for `base`, sending `origin` on every request.
    pub fn new(base: &str, origin: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').into(),
            origin: origin.into(),
            cookie: None,
            agent: ureq::AgentBuilder::new().redirects(0).build(),
        }
    }
    /// GET a JSON resource.
    pub fn get(&mut self, path: &str) -> (u16, Value) {
        self.request("GET", path, None)
    }
    /// POST a JSON body.
    pub fn post(&mut self, path: &str, body: Value) -> (u16, Value) {
        self.request("POST", path, Some(body))
    }
    /// PATCH with a JSON body.
    pub fn patch(&mut self, path: &str, body: Value) -> (u16, Value) {
        self.request("PATCH", path, Some(body))
    }
    /// DELETE a resource.
    pub fn delete(&mut self, path: &str) -> (u16, Value) {
        self.request("DELETE", path, None)
    }
    /// The current `__Host-term` value.
    pub fn cookie(&self) -> Option<String> {
        self.cookie.clone()
    }
    /// Replace or clear the session cookie.
    pub fn set_cookie(&mut self, value: Option<String>) {
        self.cookie = value;
    }

    fn request(&mut self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .agent
            .request(method, &format!("{}{path}", self.base))
            .set("Origin", &self.origin);
        if let Some(cookie) = &self.cookie {
            request = request.set("Cookie", &format!("__Host-term={cookie}"));
        }
        if matches!(method, "POST" | "PATCH" | "DELETE") {
            request = request.set("Content-Type", "application/json");
        }
        let result = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        let response = match result {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(_)) => return (0, Value::Null),
        };
        for header in response.all("Set-Cookie") {
            let mut parts = header.split(';');
            if let Some(("__Host-term", value)) =
                parts.next().and_then(|s| s.trim().split_once('='))
            {
                let clear = value.is_empty()
                    || parts.any(|part| {
                        part.trim().split_once('=').is_some_and(|(name, value)| {
                            name.trim().eq_ignore_ascii_case("max-age") && value.trim() == "0"
                        })
                    });
                self.cookie = (!clear).then(|| value.to_owned());
            }
        }
        (
            response.status(),
            response.into_json().unwrap_or(Value::Null),
        )
    }
}
