//! Acceptance tests for `anila_worker::auth_core` (task T10). Do not edit.

use anila_worker::auth_core::*;
use serde_json::json;

#[test]
fn session_ids() {
    assert_eq!(
        hash_sid("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let sid = sid_from_bytes([0u8; 32]);
    assert_eq!(sid.value, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(sid.hash, hash_sid(&sid.value));
    assert_ne!(sid_from_bytes([1u8; 32]).value, sid.value);
}

#[test]
fn cookies() {
    assert_eq!(
        set_cookie("xyz", 43200),
        "__Host-term=xyz; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=43200"
    );
    assert_eq!(
        clear_cookie(),
        "__Host-term=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0"
    );
    assert_eq!(
        read_cookie(Some("a=b; __Host-term=xyz; c=d")),
        Some("xyz".into())
    );
    assert_eq!(read_cookie(Some("__Host-term=xyz")), Some("xyz".into()));
    assert_eq!(read_cookie(Some("  __Host-term=xyz  ")), Some("xyz".into()));
    assert_eq!(read_cookie(Some("__Host-terminal=1; x__Host-term=2")), None);
    assert_eq!(read_cookie(Some("__Host-term=")), None);
    assert_eq!(read_cookie(None), None);
}

#[test]
fn enroll_codes() {
    assert_eq!(enroll_code_from_bytes([0; 5]), "0000-0000");
    assert_eq!(enroll_code_from_bytes([0xff; 5]), "ZZZZ-ZZZZ");
    assert_eq!(enroll_code_from_bytes([1, 2, 3, 4, 5]), "0410-6105");

    assert_eq!(
        normalize_enroll_code("0410-C205").as_deref(),
        Some("0410C205")
    );
    assert_eq!(
        normalize_enroll_code(" o41o c2o5 ").as_deref(),
        Some("0410C205")
    );
    assert_eq!(
        normalize_enroll_code("i1l1-0000").as_deref(),
        Some("11110000")
    );
    assert_eq!(
        normalize_enroll_code("UUUU-0000"),
        None,
        "U is not in the alphabet"
    );
    assert_eq!(normalize_enroll_code("0410-C20"), None);
    assert_eq!(normalize_enroll_code("0410-C2055"), None);
    assert_eq!(normalize_enroll_code(""), None);
    assert_eq!(hash_code("0410C205").len(), 64);
    assert_ne!(hash_code("0410C205"), hash_code("0410C206"));
}

#[test]
fn csrf_rule() {
    let o = "https://term.example.com";
    assert!(check_mutation("GET", None, None, o).is_ok());
    assert!(check_mutation("HEAD", None, None, o).is_ok());
    assert!(check_mutation("POST", Some("application/json"), Some(o), o).is_ok());
    assert!(
        check_mutation(
            "DELETE",
            Some("application/json; charset=utf-8"),
            Some(o),
            o
        )
        .is_ok()
    );
    assert!(check_mutation("POST", Some("text/plain"), Some(o), o).is_err());
    assert!(check_mutation("POST", None, Some(o), o).is_err());
    assert!(check_mutation("POST", Some("application/json"), None, o).is_err());
    assert!(
        check_mutation(
            "POST",
            Some("application/json"),
            Some("https://evil.example"),
            o
        )
        .is_err()
    );
}

#[test]
fn creation_options_shape() {
    let v = creation_options(
        &[1, 2, 3],
        "term.example.com",
        "anila",
        &[9; 16],
        "owner",
        &[vec![7, 7]],
    );
    assert_eq!(
        v,
        json!({
            "challenge": "AQID",
            "rp": { "id": "term.example.com", "name": "anila" },
            "user": { "id": "CQkJCQkJCQkJCQkJCQkJCQ", "name": "owner", "displayName": "owner" },
            "pubKeyCredParams": [ { "type": "public-key", "alg": -8 }, { "type": "public-key", "alg": -7 } ],
            "timeout": 60000,
            "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "required" },
            "attestation": "none",
            "excludeCredentials": [ { "type": "public-key", "id": "Bwc" } ]
        })
    );
}

#[test]
fn request_options_shape() {
    assert_eq!(
        request_options(&[1, 2, 3], "term.example.com"),
        json!({ "challenge": "AQID", "rpId": "term.example.com", "timeout": 60000, "userVerification": "required", "allowCredentials": [] })
    );
}

#[test]
fn parses_registration_json() {
    let ok = json!({
        "id": "AQID", "rawId": "AQID", "type": "public-key",
        "response": { "clientDataJSON": "e30", "attestationObject": "oA" },
        "clientExtensionResults": {}
    });
    assert_eq!(
        parse_registration(&ok).unwrap(),
        RegistrationResponse {
            id: vec![1, 2, 3],
            client_data_json: b"{}".to_vec(),
            attestation_object: vec![0xa0]
        }
    );
    let padded = json!({ "id": "AQID", "rawId": "AQID", "type": "public-key",
                         "response": { "clientDataJSON": "e30=", "attestationObject": "oA==" } });
    assert!(parse_registration(&padded).is_ok());

    let mut bad = ok.clone();
    bad["type"] = json!("password");
    assert!(parse_registration(&bad).is_err());
    let mut bad = ok.clone();
    bad["rawId"] = json!("AQIE");
    assert!(parse_registration(&bad).is_err(), "rawId must match id");
    let mut bad = ok.clone();
    bad["response"]["attestationObject"] = json!(null);
    assert!(parse_registration(&bad).is_err());
    assert!(parse_registration(&json!("nope")).is_err());
}

#[test]
fn parses_authentication_json() {
    let ok = json!({
        "id": "AQID", "rawId": "AQID", "type": "public-key",
        "response": { "clientDataJSON": "e30", "authenticatorData": "AAE", "signature": "AgM", "userHandle": "CQk" }
    });
    let r = parse_authentication(&ok).unwrap();
    assert_eq!(r.id, vec![1, 2, 3]);
    assert_eq!(r.authenticator_data, vec![0, 1]);
    assert_eq!(r.signature, vec![2, 3]);
    assert_eq!(r.user_handle, Some(vec![9, 9]));

    let mut no_handle = ok.clone();
    no_handle["response"]["userHandle"] = json!(null);
    assert_eq!(parse_authentication(&no_handle).unwrap().user_handle, None);
    let mut bad = ok.clone();
    bad["response"]["signature"] = json!("***");
    assert!(parse_authentication(&bad).is_err());
}

#[test]
fn session_times() {
    assert!(session_valid(100, 101));
    assert!(!session_valid(101, 101));
    assert!(step_up_ok(400, 100, 300));
    assert!(!step_up_ok(401, 100, 300));
}
