//! Pure rules for accounts, machines, sharing and codes
//! (`docs/multi-tenant.md`). No I/O and no Workers types, so everything here
//! tests natively; randomness is passed in as bytes.

use anila_proto::api::{CodeKind, Role};
use anila_proto::{HostId, limits};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

/// `h` followed by 15 characters from `a-z2-7`: the RFC 4648 base32
/// encoding (lowercase, no padding) of `bytes`, cut to 15 characters.
pub fn host_id_from_bytes(bytes: [u8; 10]) -> HostId {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut id = String::from("h");
    let mut buffer = 0_u16;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            id.push(ALPHABET[usize::from((buffer >> bits) & 31)] as char);
        }
    }
    id.truncate(16);
    HostId::new(id).expect("h plus 15 lowercase base32 characters is a valid host id")
}

/// Unpadded base64url of 16 random bytes.
pub fn account_id_from_bytes(bytes: [u8; 16]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// An account name as stored: trimmed, 1 to `ACCOUNT_NAME_MAX` characters
/// (chars, not bytes), no control characters. `None` otherwise.
pub fn normalize_account_name(input: &str) -> Option<String> {
    let name = input.trim();
    if name.is_empty()
        || name.chars().count() > limits::ACCOUNT_NAME_MAX
        || name.chars().any(char::is_control)
    {
        return None;
    }
    Some(name.to_owned())
}

/// `wanted` if no name in `taken` equals it, else `wanted-2`, `wanted-3`, …
/// (the first free one). When the suffix would exceed 32 characters, the
/// base is cut (and any `-` left at its end removed) so the result fits.
pub fn unique_host_name(wanted: &HostId, taken: &[&str]) -> HostId {
    if !taken.contains(&wanted.as_str()) {
        return wanted.clone();
    }
    let mut number = 2_u64;
    loop {
        let suffix = format!("-{number}");
        let base = wanted.as_str();
        let base = &base[..base.len().min(32 - suffix.len())];
        let candidate = format!("{}{suffix}", base.trim_end_matches('-'));
        if !taken.contains(&candidate.as_str()) {
            return HostId::new(candidate)
                .expect("a truncated host name plus a numeric suffix is a valid host id");
        }
        number += 1;
    }
}

/// How long a code of `kind` stays redeemable, in seconds (`limits`).
pub fn code_lifetime(kind: CodeKind) -> u64 {
    match kind {
        CodeKind::Device => limits::DEVICE_CODE_SECS,
        CodeKind::Invite => limits::INVITE_CODE_SECS,
        CodeKind::Pair => limits::PAIR_CODE_SECS,
        CodeKind::Share => limits::SHARE_CODE_SECS,
    }
}

/// Stored code kind ↔ text (`"device"`, `"invite"`, `"pair"`, `"share"`).
pub fn code_kind_str(kind: CodeKind) -> &'static str {
    match kind {
        CodeKind::Device => "device",
        CodeKind::Invite => "invite",
        CodeKind::Pair => "pair",
        CodeKind::Share => "share",
    }
}

/// The inverse of `code_kind_str`.
pub fn code_kind_parse(text: &str) -> Option<CodeKind> {
    match text {
        "device" => Some(CodeKind::Device),
        "invite" => Some(CodeKind::Invite),
        "pair" => Some(CodeKind::Pair),
        "share" => Some(CodeKind::Share),
        _ => None,
    }
}

/// `account`'s relation to a machine owned by `owner_id` with these members.
pub fn access(owner_id: Option<&str>, members: &[&str], account: &str) -> Option<Role> {
    if owner_id == Some(account) {
        Some(Role::Owner)
    } else if members.contains(&account) {
        Some(Role::Member)
    } else {
        None
    }
}

/// Something an account wants to do with a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Open terminals, list sessions, receive its notifications.
    Use,
    Rename,
    CreateShareCode,
    ListMembers,
    /// Remove a member; `own` is true when they remove themselves (leave).
    RemoveMember {
        own: bool,
    },
    RemoveMachine,
}

/// Owners may do everything; members may only `Use` and remove themselves.
pub fn allowed(role: Role, action: Action) -> bool {
    match role {
        Role::Owner => true,
        Role::Member => matches!(action, Action::Use | Action::RemoveMember { own: true }),
    }
}

/// The bucket a failed code lookup counts toward: `now / 60`.
pub fn failure_minute(now: i64) -> i64 {
    now / 60
}

/// Whether redemptions are blocked after `failures` failed lookups this
/// minute (`failures >= CODE_FAILURES_PER_MINUTE`).
pub fn redemptions_blocked(failures: u32) -> bool {
    failures >= limits::CODE_FAILURES_PER_MINUTE
}

/// The browser API routes v0.0.2 adds, parsed from method and path.
/// Ids are validated: a bad machine id or an empty account id is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiRoute {
    /// `GET /api/hosts`
    ListHosts,
    /// `PATCH /api/hosts/{id}`
    RenameHost(HostId),
    /// `DELETE /api/hosts/{id}`
    RemoveHost(HostId),
    /// `GET /api/hosts/{id}/members`
    ListMembers(HostId),
    /// `DELETE /api/hosts/{id}/members/{account}`; `me` is kept as `"me"`.
    RemoveMember(HostId, String),
    /// `POST /api/hosts/{id}/share-codes`
    CreateShareCode(HostId),
    /// `POST /api/share/redeem`
    RedeemShare,
    /// `POST /api/pair-codes`
    CreatePairCode,
    /// `POST /api/device-codes`
    CreateDeviceCode,
    /// `POST /api/codes/check`
    CheckCode,
    /// `POST /api/admin/invites`
    CreateInvite,
    /// `GET /api/admin/accounts`
    ListAccounts,
    /// `DELETE /api/admin/accounts/{id}`
    DeleteAccount(String),
}

/// Parses `method` (uppercase) and `path` (no query string). Anything else,
/// including a trailing slash or extra segments, is `None`.
pub fn parse_route(method: &str, path: &str) -> Option<ApiRoute> {
    let segments: Vec<_> = path.strip_prefix("/api/")?.split('/').collect();
    let valid_account = |id: &str| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    Some(match (method, segments.as_slice()) {
        ("GET", ["hosts"]) => ApiRoute::ListHosts,
        ("PATCH", ["hosts", id]) => ApiRoute::RenameHost(HostId::new(*id).ok()?),
        ("DELETE", ["hosts", id]) => ApiRoute::RemoveHost(HostId::new(*id).ok()?),
        ("GET", ["hosts", id, "members"]) => ApiRoute::ListMembers(HostId::new(*id).ok()?),
        ("DELETE", ["hosts", id, "members", account]) if valid_account(account) => {
            ApiRoute::RemoveMember(HostId::new(*id).ok()?, (*account).to_owned())
        }
        ("POST", ["hosts", id, "share-codes"]) => ApiRoute::CreateShareCode(HostId::new(*id).ok()?),
        ("POST", ["share", "redeem"]) => ApiRoute::RedeemShare,
        ("POST", ["pair-codes"]) => ApiRoute::CreatePairCode,
        ("POST", ["device-codes"]) => ApiRoute::CreateDeviceCode,
        ("POST", ["codes", "check"]) => ApiRoute::CheckCode,
        ("POST", ["admin", "invites"]) => ApiRoute::CreateInvite,
        ("GET", ["admin", "accounts"]) => ApiRoute::ListAccounts,
        ("DELETE", ["admin", "accounts", account]) if valid_account(account) => {
            ApiRoute::DeleteAccount((*account).to_owned())
        }
        _ => return None,
    })
}
