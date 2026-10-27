use anila_worker::auth_core::store_schema_supported;

#[test]
fn accepts_passkey_stores_and_refuses_idp_or_future_stores() {
    for schema in [None, Some("1"), Some("2")] {
        assert!(store_schema_supported(schema));
    }
    for schema in [Some("3"), Some("4"), Some(""), Some("unknown")] {
        assert!(!store_schema_supported(schema));
    }
}
