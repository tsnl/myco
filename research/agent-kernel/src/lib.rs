//! Executable design model: immutable data, a pure run controller, and control grants.
//! No transport, task scheduler, model client, resource implementation, or filesystem.

pub mod control;
pub mod data;
pub mod run;
pub mod trace;
