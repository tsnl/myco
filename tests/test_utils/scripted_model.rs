#![allow(dead_code)]
//! Scripted generative model for integration tests.
//!
//! Each `generate` call consumes the next pre-baked [`GenerateOutput`] (FIFO)
//! and replays it as a stream of [`MessagePart`]s — same shape the agent sees
//! from a real provider.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream;
use myco::generative_model::{Content, GenerateOutput, GenerationEvent, GenerativeModel, Message};

/// Scripted model: each `generate` call yields the next pre-baked output.
pub struct ScriptedModel {
    scripts: Mutex<VecDeque<GenerateOutput>>,
}

impl ScriptedModel {
    pub fn new(scripts: Vec<GenerateOutput>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
        })
    }

    /// How many scripted turns remain (for assertions).
    pub fn remaining(&self) -> usize {
        self.scripts.lock().expect("scripts lock").len()
    }
}

impl GenerativeModel for ScriptedModel {
    fn generate(&self, _input: &[Message]) -> myco::core::AsyncStream<GenerationEvent> {
        let output = self
            .scripts
            .lock()
            .expect("scripts lock")
            .pop_front()
            .expect("scripted model ran out of outputs");

        Box::pin(stream::iter(
            output.into_parts().into_iter().map(GenerationEvent::Part),
        ))
    }
}
