//! The myco web client — Rust compiled to wasm, driving the DOM directly.
//! The architecture is the fold ([`core`]): one state, one action stream,
//! one reducer, with this crate's wasm half doing exactly two jobs —
//! render the state, run the effects. Both halves stay dumb on purpose;
//! everything that *decides* lives in [`core::reduce`], which compiles and
//! tests natively and is what a native client (DP‑1) would reuse.

pub mod core;
#[cfg(any(target_arch = "wasm32", test))]
mod html;

#[cfg(target_arch = "wasm32")]
mod app;
