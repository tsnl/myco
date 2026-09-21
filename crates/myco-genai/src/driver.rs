use std::{future::Future, pin::Pin};

use serde_json::Value;

use crate::{Delta, Error, Event, ObserverError, Protocol, Request, Response};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait Driver: Send + Sync {
    fn protocol(&self) -> Protocol;
    fn encode(&self, request: &Request) -> Result<Value, Error>;
    fn generate<'a>(
        &'a self,
        body: Value,
        observer: &'a mut dyn Observer,
    ) -> BoxFuture<'a, Result<Response, Error>>;
}

pub(crate) trait Observer: Send {
    fn observe(&mut self, event: Event) -> BoxFuture<'_, Result<(), Error>>;
}

impl<F, Fut> Observer for F
where
    F: FnMut(Event) -> Fut + Send,
    Fut: Future<Output = Result<(), ObserverError>> + Send,
{
    fn observe(&mut self, event: Event) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { self(event).await.map_err(Error::Observer) })
    }
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
