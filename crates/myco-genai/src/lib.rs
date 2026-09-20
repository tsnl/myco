#![doc = include_str!("../README.md")]

mod anthropic;
mod http;
mod openai;
mod sse;
mod types;

pub use http::HttpModel;
pub use types::*;

/// Implemented by real providers and scripted evaluation models.
pub trait Model: Send + Sync {
    fn generate(&self, request: Request) -> GenerationStream;
}

pub type GenerationStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<Event, Error>> + Send>>;
