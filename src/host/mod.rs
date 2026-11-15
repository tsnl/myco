//! Host controller ↔ worker (in-process local or NDJSON remote).
//!
//! ```text
//! host_controller.rs  — HostController: in-process local OR lazy remote subprocess
//! transport.rs        — subprocess spawn, handshake, NDJSON writer/reader tasks
//! host_worker.rs      — HostWorker: tool registry + serve on AsyncRead/AsyncWrite
//! protocol.rs         — Request / Response
//! ```
//!
//! Tool service implementations live in [`crate::tool_services`].

mod host_controller;
pub use host_controller::{HostConfig, HostController};

mod transport;

mod host_worker;
pub use host_worker::HostWorker;

pub mod protocol;
pub use protocol::{Request, Response};
