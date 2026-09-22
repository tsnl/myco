#![doc = include_str!("../README.md")]

mod anthropic;
mod client;
mod driver;
mod generation;
mod http;
mod openai;
mod request;
mod sse;
mod types;

pub use client::{Config, GenAiClient};
pub use generation::Generation;
pub use types::*;
