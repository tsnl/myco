//! Submitted input has one FIFO owner. Editing holds its position; delivery
//! claims it under the same lock as edits and acknowledges it after persistence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::{Action, ActionRequest, App, Block, Error, Live, Result, Snapshot, attachments};

pub(super) const MAX_QUEUED_MESSAGES: usize = 20;

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum QueueState {
    Ready,
    Editing,
    Sending,
}

#[derive(Clone, Serialize)]
pub(super) struct QueuedMessage {
    pub(super) request_id: Uuid,
    pub(super) text: String,
    pub(super) images: Vec<String>,
    pub(super) accepted_at: DateTime<Utc>,
    pub(super) revision: u64,
    pub(super) state: QueueState,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum QueueUpdate {
    Edit,
    Save {
        #[serde(default)]
        text: String,
        #[serde(default)]
        images: Vec<String>,
    },
    Resume,
    Remove,
}

impl QueuedMessage {
    fn request(&self, session_id: &str) -> ActionRequest {
        ActionRequest {
            request_id: self.request_id,
            session_id: session_id.into(),
            action: Action::Submit {
                text: self.text.clone(),
                images: self.images.clone(),
            },
        }
    }
}

//
// Queue mutations and idle delivery
//

fn validate_message(text: &str, images: &[String], image_limit: u64) -> Result<()> {
    if text.trim().is_empty() && images.is_empty() {
        return Err(Error::Invalid("Enter a message or attach an image.".into()));
    }
    if !images.is_empty() {
        attachments::content(text, images, image_limit).map_err(Error::Invalid)?;
    }
    Ok(())
}

impl App {
    pub(super) fn enqueue(
        &self,
        live: &mut Live,
        request: &ActionRequest,
        accepted_at: DateTime<Utc>,
    ) -> Result<()> {
        if live.snapshot.queued.len() >= MAX_QUEUED_MESSAGES {
            return Err(Error::Conflict(
                "The message queue is full (20 messages).".into(),
            ));
        }
        let Action::Submit { text, images } = &request.action else {
            unreachable!()
        };
        validate_message(
            text,
            images,
            live.snapshot.attachment_limits.max_image_base64_bytes,
        )?;
        live.snapshot.queued.push_back(QueuedMessage {
            request_id: request.request_id,
            text: text.clone(),
            images: images.clone(),
            accepted_at,
            revision: 0,
            state: QueueState::Ready,
        });
        Ok(())
    }

    pub(super) fn update_queued(
        &self,
        live: &mut Live,
        id: Uuid,
        revision: u64,
        update: &QueueUpdate,
    ) -> Result<()> {
        let index = live
            .snapshot
            .queued
            .iter()
            .position(|message| message.request_id == id)
            .ok_or_else(|| {
                Error::Conflict("This message has already been sent or unqueued.".into())
            })?;
        let message = &mut live.snapshot.queued[index];
        if message.state == QueueState::Sending {
            return Err(Error::Conflict(
                "This message is already being sent.".into(),
            ));
        }
        if message.revision != revision {
            return Err(Error::Conflict(
                "This queued message changed in another tab. Review its latest version.".into(),
            ));
        }
        match update {
            QueueUpdate::Edit => message.state = QueueState::Editing,
            QueueUpdate::Save { text, images } => {
                validate_message(
                    text,
                    images,
                    live.snapshot.attachment_limits.max_image_base64_bytes,
                )?;
                message.text = text.clone();
                message.images = images.clone();
                message.state = QueueState::Ready;
            }
            QueueUpdate::Resume => message.state = QueueState::Ready,
            QueueUpdate::Remove => {
                live.snapshot.queued.remove(index);
                return Ok(());
            }
        }
        message.revision += 1;
        Ok(())
    }

    pub(super) fn drain_queue(&self, live: &mut Live) -> Result<()> {
        if live.snapshot.busy || self.shutdown.is_cancelled() {
            return Ok(());
        }
        if let Some(next) = live
            .snapshot
            .queued
            .front()
            .filter(|message| message.state == QueueState::Ready)
        {
            self.start_work(
                live,
                next.request(&live.snapshot.session_id),
                next.accepted_at,
            )?;
            live.snapshot.queued.pop_front();
        }
        Ok(())
    }

    pub(super) fn start_next(&self, live: &mut Live) -> Result<()> {
        live.cancel = None;
        live.snapshot.busy = false;
        self.drain_queue(live)
    }

    //
    // Delivery at a settled tool/model boundary
    //

    pub(super) fn claim_followup(&self) -> Option<QueuedMessage> {
        let mut live = self.live.lock().unwrap();
        if self.shutdown.is_cancelled() || live.snapshot.status == "Cancelling" {
            return None;
        }
        let next = live
            .snapshot
            .queued
            .front_mut()
            .filter(|message| message.state == QueueState::Ready)?;
        next.state = QueueState::Sending;
        let next = next.clone();
        self.publish_queue(&mut live.snapshot);
        Some(next)
    }

    pub(super) fn restore_followup(&self, id: Uuid) {
        let mut live = self.live.lock().unwrap();
        let next = live
            .snapshot
            .queued
            .front_mut()
            .expect("claimed queue entry");
        assert_eq!(next.request_id, id);
        next.state = QueueState::Ready;
        self.publish_queue(&mut live.snapshot);
    }

    pub(super) fn finish_followup(&self, id: Uuid, block: Option<Block>) {
        let mut live = self.live.lock().unwrap();
        let snapshot = &mut live.snapshot;
        let next = snapshot.queued.pop_front().expect("claimed queue entry");
        assert_eq!(next.request_id, id);
        if let Some(block) = block {
            let index = snapshot.blocks.len();
            snapshot.blocks.push(block.clone());
            self.publish(
                snapshot,
                json!({"kind":"block", "index":index, "block":block}),
            );
        }
        self.publish_queue(snapshot);
    }

    pub(super) fn publish_queue(&self, snapshot: &mut Snapshot) {
        self.publish(snapshot, json!({"kind":"meta", "meta":snapshot.metadata()}));
    }
}
