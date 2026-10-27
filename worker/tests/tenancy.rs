//! Acceptance tests for `anila_worker::tenancy` (task T31). Do not edit.

use anila_proto::HostId;
use anila_proto::api::{CodeKind, Role};
use anila_proto::limits::*;
use anila_worker::tenancy::*;

fn host(s: &str) -> HostId {
    HostId::new(s).unwrap()
}

#[test]
fn host_ids() {
    assert_eq!(host_id_from_bytes([0; 10]).as_str(), "haaaaaaaaaaaaaaa");
    // RFC 4648 base32 of "foobar" is MZXW6YTBOI; padded with zero bytes.
    let id = host_id_from_bytes(*b"foobar\0\0\0\0");
    assert_eq!(id.as_str(), "hmzxw6ytboiaaaaa");
    assert_eq!(id.as_str().len(), 16);
    assert!(
        host_id_from_bytes([0xff; 10])
            .as_str()
            .chars()
            .skip(1)
            .all(|c| matches!(c, 'a'..='z' | '2'..='7'))
    );
    assert_ne!(host_id_from_bytes([1; 10]), host_id_from_bytes([2; 10]));
}

#[test]
fn account_ids() {
    assert_eq!(account_id_from_bytes([0; 16]), "AAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(account_id_from_bytes([0xff; 16]), "_____________________w");
}

#[test]
fn account_names() {
    assert_eq!(normalize_account_name("  Alice  "), Some("Alice".into()));
    assert_eq!(
        normalize_account_name("Đặng Thị Mai"),
        Some("Đặng Thị Mai".into())
    );
    assert_eq!(normalize_account_name(""), None);
    assert_eq!(normalize_account_name("   "), None);
    assert_eq!(normalize_account_name("a\u{0007}b"), None);
    assert_eq!(normalize_account_name("line\nbreak"), None);
    let max = "é".repeat(ACCOUNT_NAME_MAX);
    assert_eq!(
        normalize_account_name(&max),
        Some(max.clone()),
        "chars, not bytes"
    );
    assert_eq!(normalize_account_name(&format!("{max}x")), None);
}

#[test]
fn unique_machine_names() {
    assert_eq!(unique_host_name(&host("mac"), &[]), host("mac"));
    assert_eq!(unique_host_name(&host("mac"), &["pc"]), host("mac"));
    assert_eq!(unique_host_name(&host("mac"), &["mac"]), host("mac-2"));
    assert_eq!(
        unique_host_name(&host("mac"), &["mac", "mac-2", "mac-3"]),
        host("mac-4")
    );
    assert_eq!(
        unique_host_name(&host("mac"), &["mac", "mac-3"]),
        host("mac-2")
    );
    let long = "a".repeat(32);
    let got = unique_host_name(&host(&long), &[long.as_str()]);
    assert_eq!(got.as_str(), format!("{}-2", "a".repeat(30)));
    // A cut that would end in '-' drops it.
    let dashy = format!("{}-b", "a".repeat(29)); // 31 chars, '-' at index 29
    let got = unique_host_name(&host(&dashy), &[dashy.as_str()]);
    assert_eq!(got.as_str(), format!("{}-2", "a".repeat(29)));
}

#[test]
fn code_kinds() {
    assert_eq!(code_lifetime(CodeKind::Device), DEVICE_CODE_SECS);
    assert_eq!(code_lifetime(CodeKind::Invite), INVITE_CODE_SECS);
    assert_eq!(code_lifetime(CodeKind::Pair), PAIR_CODE_SECS);
    assert_eq!(code_lifetime(CodeKind::Share), SHARE_CODE_SECS);
    for kind in [
        CodeKind::Device,
        CodeKind::Invite,
        CodeKind::Pair,
        CodeKind::Share,
    ] {
        assert_eq!(code_kind_parse(code_kind_str(kind)), Some(kind));
    }
    assert_eq!(code_kind_str(CodeKind::Share), "share");
    assert_eq!(code_kind_parse("Share"), None);
    assert_eq!(code_kind_parse("admin"), None);
}

#[test]
fn access_decisions() {
    assert_eq!(access(Some("a"), &[], "a"), Some(Role::Owner));
    assert_eq!(access(Some("a"), &["b", "c"], "c"), Some(Role::Member));
    assert_eq!(access(Some("a"), &["b"], "z"), None);
    assert_eq!(
        access(None, &[], "a"),
        None,
        "an ownerless machine belongs to nobody"
    );
    assert_eq!(access(None, &["b"], "b"), Some(Role::Member));
}

#[test]
fn permissions() {
    use Action::*;
    for action in [
        Use,
        Rename,
        CreateShareCode,
        ListMembers,
        RemoveMember { own: false },
        RemoveMember { own: true },
        RemoveMachine,
    ] {
        assert!(allowed(Role::Owner, action), "owner may {action:?}");
    }
    assert!(allowed(Role::Member, Use));
    assert!(
        allowed(Role::Member, RemoveMember { own: true }),
        "a member may leave"
    );
    for action in [
        Rename,
        CreateShareCode,
        ListMembers,
        RemoveMember { own: false },
        RemoveMachine,
    ] {
        assert!(!allowed(Role::Member, action), "member may not {action:?}");
    }
}

#[test]
fn code_failure_limit() {
    assert_eq!(failure_minute(0), 0);
    assert_eq!(failure_minute(59), 0);
    assert_eq!(failure_minute(60), 1);
    assert_eq!(failure_minute(1_792_585_534), 29_876_425);
    assert!(!redemptions_blocked(0));
    assert!(!redemptions_blocked(CODE_FAILURES_PER_MINUTE - 1));
    assert!(redemptions_blocked(CODE_FAILURES_PER_MINUTE));
}

#[test]
fn routes() {
    use ApiRoute::*;
    let h = host("h7abc");
    assert_eq!(parse_route("GET", "/api/hosts"), Some(ListHosts));
    assert_eq!(
        parse_route("PATCH", "/api/hosts/h7abc"),
        Some(RenameHost(h.clone()))
    );
    assert_eq!(
        parse_route("DELETE", "/api/hosts/h7abc"),
        Some(RemoveHost(h.clone()))
    );
    assert_eq!(
        parse_route("GET", "/api/hosts/h7abc/members"),
        Some(ListMembers(h.clone()))
    );
    assert_eq!(
        parse_route("DELETE", "/api/hosts/h7abc/members/me"),
        Some(RemoveMember(h.clone(), "me".into()))
    );
    assert_eq!(
        parse_route("DELETE", "/api/hosts/h7abc/members/AAAAAAAAAAAAAAAAAAAAAA"),
        Some(RemoveMember(h.clone(), "AAAAAAAAAAAAAAAAAAAAAA".into()))
    );
    assert_eq!(
        parse_route("POST", "/api/hosts/h7abc/share-codes"),
        Some(CreateShareCode(h.clone()))
    );
    assert_eq!(parse_route("POST", "/api/share/redeem"), Some(RedeemShare));
    assert_eq!(parse_route("POST", "/api/pair-codes"), Some(CreatePairCode));
    assert_eq!(
        parse_route("POST", "/api/device-codes"),
        Some(CreateDeviceCode)
    );
    assert_eq!(parse_route("POST", "/api/codes/check"), Some(CheckCode));
    assert_eq!(
        parse_route("POST", "/api/admin/invites"),
        Some(CreateInvite)
    );
    assert_eq!(
        parse_route("GET", "/api/admin/accounts"),
        Some(ListAccounts)
    );
    assert_eq!(
        parse_route("DELETE", "/api/admin/accounts/abc_-1"),
        Some(DeleteAccount("abc_-1".into()))
    );

    // Wrong method, bad ids, extra or missing segments.
    assert_eq!(parse_route("POST", "/api/hosts"), None);
    assert_eq!(parse_route("GET", "/api/pair-codes"), None);
    assert_eq!(parse_route("PATCH", "/api/hosts/Bad_Id"), None);
    assert_eq!(parse_route("PATCH", "/api/hosts/"), None);
    assert_eq!(parse_route("GET", "/api/hosts/h7abc/members/"), None);
    assert_eq!(parse_route("DELETE", "/api/hosts/h7abc/members/"), None);
    assert_eq!(parse_route("DELETE", "/api/hosts/h7abc/members/me/x"), None);
    assert_eq!(parse_route("DELETE", "/api/admin/accounts/"), None);
    assert_eq!(parse_route("GET", "/api/hosts/"), None);
    assert_eq!(
        parse_route("GET", "/api/me"),
        None,
        "existing routes are not ApiRoute"
    );
}
