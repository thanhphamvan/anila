//! The public edition exposes no identity-provider routes, even if an old
//! environment still contains identity-provider settings.

mod common;

use anila_e2e::http::Http;
use common::*;
use serde_json::json;

#[test]
fn removed_login_routes_stay_unavailable() {
    let worker = start_worker_with(
        false,
        vec![
            ("IDP_ISSUER".into(), "https://provider.example.com".into()),
            ("IDP_CLIENT_ID".into(), "obsolete-client".into()),
        ],
    );
    let mut http = Http::new(&worker.http_base(), &worker.origin());
    for path in ["/api/idp", "/api/idp/callback?code=unused&state=unused"] {
        assert_eq!(http.get(path).0, 404);
    }
    assert_eq!(
        http.post("/api/idp/start", json!({"intent": "login"})).0,
        404
    );
    assert_eq!(http.get("/api/me").0, 401);
    assert_eq!(http.post("/api/passkeys/login/options", json!({})).0, 200);
}
