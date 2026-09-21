use std::future::Future;

use serde_json::Value;

use crate::driver::Driver;
use crate::{Error, Event, ObserverError, Request, Response, anthropic, openai, request};

/// Endpoints are complete URLs. An empty key omits authentication.
/// Credentials are deliberately excluded from debug output.
pub enum Config {
    OpenAi { endpoint: String, api_key: String },
    Anthropic { endpoint: String, api_key: String },
}

/// Reusable inference client. Share it by reference or through `Arc<Client>`.
pub struct Client {
    driver: Box<dyn Driver>,
}

impl Client {
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

    /// One attempt. Observations are awaited in order; only the return value is
    /// final. Dropping this future releases the request without a background task.
    pub async fn generate<F>(
        &self,
        request: Request,
        mut observe: impl FnMut(Event) -> F + Send,
    ) -> Result<Response, Error>
    where
        F: Future<Output = Result<(), ObserverError>> + Send,
    {
        let body = self.request_body(&request)?;
        self.driver.generate(body, &mut observe).await
    }
}
