//! Contract numbers. Each one has a single owner that enforces it, named
//! in the comment; other services may rely on it holding.

/// Browser and service: ping interval, and silence before a link is dead.
pub const PING_INTERVAL_SECS: u64 = 10;
pub const DEAD_AFTER_SECS: u64 = 25;

/// Service: how long a PTY stays attached after `tab_gone`.
pub const GRACE_SECS: u64 = 600;
/// Service: replay buffer per channel.
pub const REPLAY_BUFFER_BYTES: usize = 4 << 20;
/// Service: unacked output before PTY reads pause.
pub const FLOW_WINDOW_BYTES: usize = 512 << 10;

/// Browser: ack cadence.
pub const ACK_INTERVAL_MS: u64 = 200;
pub const ACK_EVERY_BYTES: usize = 64 << 10;
/// Service: `in_ack` batching.
pub const IN_ACK_INTERVAL_MS: u64 = 100;

/// Room: frame sizes, enforced with close code 1009.
pub const MAX_OUT_PAYLOAD: usize = 32 << 10;
pub const MAX_IN_PAYLOAD: usize = 4 << 10;
pub const MAX_TEXT_FRAME: usize = 64 << 10;

/// Room and service: fan-out limits.
pub const MAX_TABS_PER_ROOM: usize = 16;
pub const MAX_CHANNELS_PER_TAB: usize = 8;

/// Room: hung-service check. Alarm interval; the service counts as hung
/// after `DEAD_AFTER_SECS` without a heartbeat.
pub const HUNG_AGENT_CHECK_SECS: u64 = 15;
/// Room: after `watch { on: true }`, how long a watched service has to send
/// its first heartbeat before it counts as hung.
pub const WATCH_PROBE_SECS: u64 = 5;

/// Auth store.
pub const SESSION_SECS: u64 = 12 * 60 * 60;
pub const STEP_UP_SECS: u64 = 5 * 60;
pub const CHALLENGE_SECS: u64 = 60;
pub const ENROLL_CODE_SECS: u64 = 10 * 60;

/// Auth store: one-time code lifetimes, by kind (`docs/multi-tenant.md`).
pub const DEVICE_CODE_SECS: u64 = ENROLL_CODE_SECS;
pub const PAIR_CODE_SECS: u64 = 10 * 60;
pub const SHARE_CODE_SECS: u64 = 24 * 60 * 60;
pub const INVITE_CODE_SECS: u64 = 7 * 24 * 60 * 60;
/// Auth store: failed code lookups per minute before all redemptions fail.
pub const CODE_FAILURES_PER_MINUTE: u32 = 30;

/// Auth store: tenancy limits.
pub const MAX_ACCOUNTS: usize = 200;
pub const MAX_HOSTS_PER_ACCOUNT: usize = 20;
pub const MAX_MEMBERS_PER_HOST: usize = 20;
pub const MAX_PASSKEYS_PER_ACCOUNT: usize = 20;
pub const ACCOUNT_NAME_MAX: usize = 40;

/// Room and worker: allowed clock skew for service signatures.
pub const AGENT_SKEW_SECS: i64 = 60;

/// Websocket close codes.
pub mod close {
    pub const GOING_AWAY: u16 = 1001;
    pub const TOO_BIG: u16 = 1009;
    pub const BAD_FRAME: u16 = 4400;
    pub const UNAUTHORIZED: u16 = 4401;
    pub const FORBIDDEN: u16 = 4403;
    /// The room heard no heartbeat from the service for `DEAD_AFTER_SECS`.
    pub const TIMEOUT: u16 = 4408;
    pub const REPLACED: u16 = 4409;
}
