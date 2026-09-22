use std::pin::Pin;

use futures_core::Stream;
use serde_json::Value;

use crate::{Delta, Error, Event, Protocol, Request};

pub(crate) type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<Event, Error>> + Send + 'a>>;

pub(crate) trait Driver: Send + Sync {
    fn protocol(&self) -> Protocol;
    fn encode(&self, request: &Request) -> Result<Value, Error>;
    fn generate(&self, body: Value) -> EventStream<'_>;
}

pub(crate) enum Decoded {
    Progress(Option<Delta>),
    Completed(Value),
}

impl Decoded {
    pub fn delta(&self) -> Option<Delta> {
        match self {
            Self::Progress(delta) => delta.clone(),
            Self::Completed(_) => None,
        }
    }
}
