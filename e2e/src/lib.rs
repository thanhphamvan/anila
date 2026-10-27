//! Harness for end-to-end and chaos tests: a real worker under
//! `wrangler dev`, the real service binary, a private tmux server, a fault
//! injecting TCP proxy, a software passkey authenticator, and a probe that
//! plays the browser with `anila_proto::client::Client`.
//!
//! Tests in `e2e/tests/` start real processes and must run one at a time:
//! `cargo test -p anila-e2e -- --test-threads=1`.

pub mod authn;
pub mod bench;
pub mod browser;
pub mod http;
pub mod phone;
pub mod probe;
pub mod proxy;
pub mod stack;
pub mod tmux;
pub mod util;
