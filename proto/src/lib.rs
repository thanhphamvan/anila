//! The wire contract between the browser, the host room and the service.
//!
//! Every message type lives here. The service, the worker and the frontend
//! all depend on this crate and none of them defines its own frames.

pub mod api;
pub mod client;
pub mod codec;
pub mod control;
pub mod data;
pub mod limits;
pub mod names;
pub mod route;
pub mod sig;

pub use control::{
    AgentStatus, Codec, Create, ErrorCode, Hop, Preset, SessionInfo, ToAgent, ToBrowser,
};
pub use data::{Data, FrameError};
pub use names::{HostId, NameError, SessionName};
pub use route::{Route, classify};

/// Protocol version. Bump on breaking changes only; additive changes keep it.
pub const PROTO: u16 = 1;

/// The room accepts a service speaking `PROTO` or `PROTO - 1`, because the
/// service on the host machine can lag a worker deploy.
pub fn service_proto_supported(proto: u16) -> bool {
    proto == PROTO || proto + 1 == PROTO
}

pub type Tab = u32;
pub type Ch = u16;
pub type Epoch = u32;
pub type Off = u64;
pub type InputId = u32;
pub type Req = u32;
