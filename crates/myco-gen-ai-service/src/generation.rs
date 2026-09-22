use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::{Stream, stream::FusedStream};

use crate::driver::EventStream;
use crate::{Error, Event};

/// An owned attempt, borrowing its client. Polling drives I/O; no task is spawned.
/// Completion or error terminates the stream and releases its request.
pub struct Generation<'a> {
    inner: Option<EventStream<'a>>,
}

impl<'a> Generation<'a> {
    pub(crate) fn new(stream: impl Stream<Item = Result<Event, Error>> + Send + 'a) -> Self {
        Self {
            inner: Some(Box::pin(stream)),
        }
    }
}

impl Stream for Generation<'_> {
    type Item = Result<Event, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = &mut this.inner else {
            return Poll::Ready(None);
        };
        let next = inner.as_mut().poll_next(cx);
        if matches!(
            &next,
            Poll::Ready(None | Some(Err(_)) | Some(Ok(Event::Completed(_))))
        ) {
            this.inner = None;
        }
        next
    }
}

impl FusedStream for Generation<'_> {
    fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }
}
