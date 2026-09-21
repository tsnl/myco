use serde_json::Value;

use crate::driver::{BoxFuture, Driver, Observer};
use crate::http::Transport;
use crate::{Error, Protocol, Request, Response};

mod request;
mod response;
mod stream;

pub(crate) use response::decode as response;

pub(crate) struct Backend {
    transport: Transport,
}

impl Backend {
    pub fn new(endpoint: &str, api_key: &str) -> Result<Self, Error> {
        Ok(Self {
            transport: Transport::new(Protocol::AnthropicMessages, endpoint, api_key)?,
        })
    }
}

impl Driver for Backend {
    fn protocol(&self) -> Protocol {
        Protocol::AnthropicMessages
    }

    fn encode(&self, request: &Request) -> Result<Value, Error> {
        request::encode(request)
    }

    fn generate<'a>(
        &'a self,
        body: Value,
        observer: &'a mut dyn Observer,
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let mut accumulator = stream::Accumulator::default();
            self.transport
                .generate(self.protocol(), body, observer, |event| {
                    accumulator.event(event)
                })
                .await
        })
    }
}
