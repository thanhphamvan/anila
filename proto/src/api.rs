//! JSON bodies of the browser ↔ worker HTTP API and the service's signed
//! calls that v0.0.2 added (`docs/multi-tenant.md`). The worker serializes
//! these and the web app and service deserialize them, so field names live
//! in one place.

use serde::{Deserialize, Serialize};

use crate::HostId;

/// The signed-in account, as `GET /api/me` reports it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AccountInfo {
    pub id: String,
    pub name: String,
    pub admin: bool,
}

/// `GET /api/me`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Me {
    pub account: AccountInfo,
    /// Unix seconds of the passkey ceremony that created this session.
    pub auth_at: i64,
    pub expires_at: i64,
}

/// The caller's relation to a machine.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Member,
}

/// One machine in `GET /api/hosts`, and the reply to rename and redeem.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HostView {
    pub id: HostId,
    pub name: HostId,
    pub role: Role,
    pub owner_name: String,
    pub online: bool,
    /// Unix seconds of the last online/offline change, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
}

/// `GET /api/hosts/{id}/members`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Member {
    pub account_id: String,
    pub name: String,
    /// Unix seconds when they joined.
    pub since: i64,
}

/// A freshly created one-time code. The code is shown once and never stored.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CodeIssued {
    pub code: String,
    pub expires_at: i64,
}

/// What a code does when redeemed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeKind {
    /// Adds a passkey to an existing account (or bootstraps the first admin).
    Device,
    /// Creates a new account.
    Invite,
    /// Adds a machine to an account (`anila-service pair`).
    Pair,
    /// Makes the redeeming account a member of a machine.
    Share,
}

/// `POST /api/codes/check` reply. Only `device` and `invite` are reported.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CodeCheck {
    pub kind: CodeKind,
}

/// `GET /api/admin/accounts`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AccountView {
    pub id: String,
    pub name: String,
    pub admin: bool,
    pub created_at: i64,
    /// Machines this account owns.
    pub hosts: u32,
    pub passkeys: u32,
}

/// `POST /api/agent/pair` body, signed with the key in `public_key`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PairRequest {
    pub code: String,
    /// Wanted machine name; the worker makes it unique for the owner.
    pub name: HostId,
    /// The service's ed25519 public key, unpadded base64url.
    pub public_key: String,
}

/// `POST /api/agent/pair` reply.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Paired {
    /// The machine id to put in `service.toml` as `host`.
    pub host: HostId,
    pub name: HostId,
    pub account_name: String,
}

/// The `Term-Host` value of a pairing call, which has no machine id yet.
pub const PAIR_HOST: &str = "pair";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes_match_the_design_doc() {
        let host = HostView {
            id: HostId::new("h7abc").unwrap(),
            name: HostId::new("mac-mini").unwrap(),
            role: Role::Member,
            owner_name: "Alice".into(),
            online: true,
            since: None,
        };
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            json!({"id": "h7abc", "name": "mac-mini", "role": "member",
                   "owner_name": "Alice", "online": true})
        );
        assert_eq!(
            serde_json::to_value(CodeCheck {
                kind: CodeKind::Invite
            })
            .unwrap(),
            json!({"kind": "invite"})
        );
        let pair: PairRequest = serde_json::from_value(
            json!({"code": "ABCD-EFGH", "name": "mac-mini", "public_key": "k"}),
        )
        .unwrap();
        assert_eq!(pair.name.as_str(), "mac-mini");
        assert!(
            serde_json::from_value::<PairRequest>(
                json!({"code": "x", "name": "Mac Mini", "public_key": "k"})
            )
            .is_err(),
            "machine names are validated on deserialize"
        );
    }
}
