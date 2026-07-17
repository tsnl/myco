#![allow(dead_code, unused_imports)]
use std::sync::Once;

mod scripted_model;
mod transcript;

pub use scripted_model::ScriptedModel;
pub use transcript::format_transcript;

static INIT: Once = Once::new();

/// Load `.env` once (for live-model integration tests that need API keys).
#[allow(dead_code)]
pub fn load_dotenv() {
    INIT.call_once(|| {
        dotenvy::dotenv().ok();
    });
}
