use async_stream::try_stream;
use futures_util::StreamExt;
use serde_json::Value;

use crate::model::driver::Driver;
use crate::model::{Error, Generation, Request, anthropic, openai, request};

/// Endpoints are complete URLs. An empty key omits authentication.
/// Credentials are deliberately excluded from debug output.
pub enum Config {
    OpenAi { endpoint: String, api_key: String },
    Anthropic { endpoint: String, api_key: String },
}

/// Reusable inference client. Share it by reference or through `Arc<GenAiClient>`.
pub struct GenAiClient {
    driver: Box<dyn Driver>,
}

impl GenAiClient {
    pub fn new(config: Config) -> Result<Self, Error> {
        let driver: Box<dyn Driver> = match config {
            Config::OpenAi { endpoint, api_key } => {
                Box::new(openai::Backend::new(&endpoint, &api_key)?)
            }
            Config::Anthropic { endpoint, api_key } => {
                Box::new(anthropic::Backend::new(&endpoint, &api_key)?)
            }
        };
        Ok(Self { driver })
    }

    /// Inspect the provider payload without opening a connection.
    pub fn request_body(&self, request: &Request) -> Result<Value, Error> {
        request::validate(request, self.driver.protocol())?;
        let mut body = self.driver.encode(request)?;
        request::apply_options(&mut body, request)?;
        Ok(body)
    }

    /// One attempt, advanced by polling. Only `Event::Completed` contains the
    /// final response. Dropping the stream releases the request.
    pub fn generate(&self, request: Request) -> Generation<'_> {
        Generation::new(try_stream! {
            let body = self.request_body(&request)?;
            let mut events = self.driver.generate(body);
            while let Some(event) = events.next().await {
                yield event?;
            }
        })
    }
}
