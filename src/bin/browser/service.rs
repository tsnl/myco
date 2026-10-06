//! Native clients observe committed output by request identity, across thread replacement.

use std::collections::{HashMap, VecDeque};

use super::{Action, ActionRequest, App, Error, Result, attachments};
use crate::service_protocol::{Output, Submit};
use uuid::Uuid;

const MAX_RECEIPTS: usize = 16;
const MAX_REQUESTS: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;

struct Receipt {
    id: Uuid,
    revision: u64,
    output: String,
    exit_code: Option<u8>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct Receipts {
    entries: VecDeque<Receipt>,
    accepted: HashMap<Uuid, [u8; 32]>,
    active: Option<Uuid>,
    draft: String,
    overflow: bool,
}

impl Receipts {
    pub(super) fn finish(
        &mut self,
        id: Uuid,
        result: &std::result::Result<(), String>,
        cancelled: bool,
    ) {
        if self.active != Some(id) {
            return;
        }
        let receipt = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .unwrap();
        receipt.exit_code = Some(if receipt.error.is_some() {
            1
        } else if cancelled {
            130
        } else if result.is_err() {
            1
        } else {
            0
        });
        if receipt.error.is_none() {
            receipt.error = result.as_ref().err().cloned();
        }
        receipt.revision += 1;
        self.active = None;
        self.draft.clear();
    }

    pub(super) fn contains(&self, id: &Uuid) -> bool {
        self.accepted.contains_key(id)
    }

    fn begin(&mut self, id: Uuid) {
        assert!(
            self.active.is_none(),
            "service submissions require an idle session"
        );
        if self.entries.len() == MAX_RECEIPTS {
            self.entries.pop_front();
        }
        self.entries.push_back(Receipt {
            id,
            revision: 0,
            output: String::new(),
            exit_code: None,
            error: None,
        });
        self.active = Some(id);
        self.reset_draft();
    }

    pub(super) fn reset_draft(&mut self) {
        self.draft.clear();
        self.overflow = false;
    }

    pub(super) fn text(&mut self, text: &str) {
        if self.active.is_none() {
            return;
        }
        if self.draft.len().saturating_add(text.len()) > MAX_OUTPUT_BYTES {
            self.overflow = true;
        } else if !self.overflow {
            self.draft.push_str(text);
        }
    }

    /// Return true when the caller must cancel the run because output storage is full.
    pub(super) fn commit(&mut self) -> bool {
        let Some(id) = self.active else {
            return false;
        };
        let receipt = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .unwrap();
        if self.overflow || receipt.output.len().saturating_add(self.draft.len()) > MAX_OUTPUT_BYTES
        {
            receipt.error = Some("Service stdout exceeded 4 MiB; the run was cancelled. Inspect the saved session transcript before continuing.".into());
            self.draft.clear();
            return true;
        }
        receipt.output.push_str(&std::mem::take(&mut self.draft));
        receipt.revision += 1;
        false
    }
}

impl App {
    pub(crate) fn accept_service(&self, mut input: Submit, session_id: String) -> Result<()> {
        attachments::externalize(&mut input.images).map_err(Error::Invalid)?;
        let fingerprint: [u8; 32] = ring::digest::digest(
            &ring::digest::SHA256,
            &serde_json::to_vec(&(&input.text, &input.images)).expect("serialize service input"),
        )
        .as_ref()
        .try_into()
        .unwrap();
        let request = ActionRequest {
            request_id: input.request_id,
            session_id,
            action: Action::ServiceSubmit {
                text: input.text,
                images: input.images,
            },
        };
        let mut live = self.live.lock().unwrap();
        if live.snapshot.session_id != request.session_id {
            return Err(Error::Conflict(
                "The request belongs to a different session.".into(),
            ));
        }
        if let Some(accepted) = live.service.accepted.get(&request.request_id) {
            if accepted != &fingerprint {
                return Err(Error::Conflict(
                    "Request id already used for another action.".into(),
                ));
            }
            return if live
                .service
                .entries
                .iter()
                .any(|entry| entry.id == request.request_id)
            {
                Ok(())
            } else {
                Err(expired())
            };
        }
        if live.accepted.contains_key(&request.request_id) {
            return Err(Error::Conflict(
                "Request id already used for a browser action.".into(),
            ));
        }
        if live.service.accepted.len() == MAX_REQUESTS {
            return Err(Error::Unavailable("This session reached the 4096-request service identity limit. Finish active work and restart the service before submitting more native turns; saved history is retained.".into()));
        }
        if self.shutdown.is_cancelled() || self.work.is_closed() {
            return Err(Error::Unavailable(
                "The session worker is unavailable.".into(),
            ));
        }
        if live.snapshot.busy || live.service.active.is_some() || !live.snapshot.queued.is_empty() {
            return Err(Error::Conflict("The session has active or queued work. Observe it or wait before submitting a service turn.".into()));
        }
        let Action::ServiceSubmit { text, images } = &request.action else {
            unreachable!()
        };
        if text.trim().is_empty() && images.is_empty() {
            return Err(Error::Invalid("Enter a prompt or attach an image.".into()));
        }
        attachments::literal_content(
            text,
            images,
            live.snapshot.attachment_limits.max_image_base64_bytes,
        )
        .map_err(Error::Invalid)?;
        self.start_work(&mut live, request.clone(), chrono::Utc::now())?;
        live.service.begin(request.request_id);
        live.service
            .accepted
            .insert(request.request_id, fingerprint);
        let change = serde_json::json!({"kind":"meta", "meta":live.snapshot.metadata()});
        self.publish(&mut live.snapshot, change);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn finish_service(
        &self,
        id: Uuid,
        result: &std::result::Result<(), String>,
        cancelled: bool,
    ) {
        self.live
            .lock()
            .unwrap()
            .service
            .finish(id, result, cancelled);
    }

    pub(crate) fn service_output(&self, instance: Uuid, id: Uuid, offset: usize) -> Result<Output> {
        let live = self.live.lock().unwrap();
        let receipt = live.service.entries.iter().find(|entry| entry.id == id).ok_or_else(|| {
            if live.service.contains(&id) { expired() }
            else { Error::NotFound("No receipt for this request in this service instance. Its outcome is unknown; inspect the saved session before submitting new work.".into()) }
        })?;
        if offset > receipt.output.len() || !receipt.output.is_char_boundary(offset) {
            return Err(Error::Invalid("Invalid output byte offset.".into()));
        }
        let mut end = (offset + CHUNK_BYTES).min(receipt.output.len());
        while !receipt.output.is_char_boundary(end) {
            end -= 1;
        }
        Ok(Output {
            instance,
            request_id: id,
            revision: receipt.revision,
            offset,
            next_offset: end,
            output: receipt.output[offset..end].into(),
            exit_code: (end == receipt.output.len())
                .then_some(receipt.exit_code)
                .flatten(),
            error: receipt.error.clone(),
        })
    }

    pub(crate) fn cancel_service(&self, id: Uuid) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if !live.service.entries.iter().any(|entry| entry.id == id) {
            return Err(expired());
        }
        if live.service.active == Some(id) {
            self.cancel_live(&mut live);
        }
        Ok(())
    }
}

fn expired() -> Error {
    Error::Gone("This service output receipt expired (16 recent turns are retained). The request will not be replayed; inspect the saved session transcript.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use myco::{AgentEvent, EventSink, TraceContext};

    fn input() -> Submit {
        Submit {
            instance: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            text: "run".into(),
            images: vec![],
        }
    }

    fn complete(app: &App, id: Uuid, text: &str) {
        let context = TraceContext::root();
        app.emit(AgentEvent::GenerationStarted {
            context: context.clone(),
        });
        app.emit(AgentEvent::TextDelta {
            text: text.into(),
            context: context.clone(),
        });
        app.emit(AgentEvent::GenerationFinished {
            context: context.clone(),
        });
        app.emit(AgentEvent::GenerationCommitted { context });
        app.finish_service(id, &Ok(()), false);
        let mut live = app.live.lock().unwrap();
        live.snapshot.busy = false;
        live.cancel = None;
    }

    #[test]
    fn a_lost_acceptance_response_can_be_retried_without_repeating_work() {
        let (app, mut receiver) = super::super::tests::app();
        let input = input();
        app.accept_service(input.clone(), "session".into()).unwrap();
        app.accept_service(input.clone(), "session".into()).unwrap();
        assert_eq!(
            receiver.try_recv().unwrap().request.request_id,
            input.request_id
        );
        assert!(receiver.try_recv().is_err());
        let mut conflicting = input.clone();
        conflicting.text = "different".into();
        assert!(matches!(
            app.accept_service(conflicting, "session".into()),
            Err(Error::Conflict(_))
        ));
        complete(&app, input.request_id, "committed");
        app.accept_service(input.clone(), "session".into()).unwrap();
        assert!(receiver.try_recv().is_err());
        let output = app
            .service_output(input.instance, input.request_id, 0)
            .unwrap();
        assert_eq!(output.output, "committed");
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn output_excludes_discarded_drafts_and_survives_transcript_replacement() {
        let (app, _receiver) = super::super::tests::app();
        let input = input();
        app.accept_service(input.clone(), "session".into()).unwrap();
        app.live.lock().unwrap().service.text("discarded");
        assert!(
            app.service_output(input.instance, input.request_id, 0)
                .unwrap()
                .output
                .is_empty()
        );
        app.live.lock().unwrap().service.reset_draft();
        complete(&app, input.request_id, "kept 🦊");
        app.live.lock().unwrap().snapshot.blocks.clear();
        let output = app
            .service_output(input.instance, input.request_id, 0)
            .unwrap();
        assert_eq!(output.output, "kept 🦊");
        assert!(matches!(
            app.service_output(input.instance, input.request_id, 6),
            Err(Error::Invalid(_))
        ));
        assert!(output.revision > 0);
    }

    #[test]
    fn automatic_retry_keeps_validated_output_for_checkpoint_repair_only() {
        for validated in [false, true] {
            let (app, _receiver) = super::super::tests::app();
            let input = input();
            app.accept_service(input.clone(), "session".into()).unwrap();
            let context = TraceContext::root();
            app.emit(AgentEvent::TextDelta {
                text: "prior answer".into(),
                context: context.clone(),
            });
            app.emit(AgentEvent::GenerationFinished {
                context: context.clone(),
            });
            app.emit(AgentEvent::GenerationCommitted {
                context: context.clone(),
            });
            app.emit(AgentEvent::GenerationStarted {
                context: context.clone(),
            });
            app.emit(AgentEvent::TextDelta {
                text: "pending answer".into(),
                context: context.clone(),
            });
            if validated {
                app.emit(AgentEvent::GenerationFinished {
                    context: context.clone(),
                });
            }
            app.retrying(
                "checkpoint or stream failed".into(),
                std::time::Duration::from_secs(1),
            );
            let pending = app
                .service_output(input.instance, input.request_id, 0)
                .unwrap();
            assert_eq!(pending.output, "prior answer");
            assert!(pending.exit_code.is_none());
            assert_eq!(app.snapshot().change["snapshot"]["status"], "Retrying");
            app.emit(AgentEvent::GenerationCommitted { context });
            let output = app
                .service_output(input.instance, input.request_id, 0)
                .unwrap();
            assert_eq!(
                output.output,
                if validated {
                    "prior answerpending answer"
                } else {
                    "prior answer"
                }
            );
        }
    }

    #[test]
    fn expired_receipts_never_replay_and_cancelling_an_old_turn_leaves_current_work_running() {
        let (app, mut receiver) = super::super::tests::app();
        let old = input();
        app.accept_service(old.clone(), "session".into()).unwrap();
        receiver.try_recv().unwrap();
        complete(&app, old.request_id, "first");
        let next = input();
        app.accept_service(next.clone(), "session".into()).unwrap();
        let work = receiver.try_recv().unwrap();
        app.cancel_service(old.request_id).unwrap();
        assert!(!work.cancel.is_cancelled());
        app.cancel_service(next.request_id).unwrap();
        assert!(work.cancel.is_cancelled());
        complete(&app, next.request_id, "second");
        for _ in 0..MAX_RECEIPTS {
            let next = input();
            app.accept_service(next.clone(), "session".into()).unwrap();
            receiver.try_recv().unwrap();
            complete(&app, next.request_id, "later");
        }
        assert!(matches!(
            app.service_output(old.instance, old.request_id, 0),
            Err(Error::Gone(_))
        ));
        assert!(matches!(
            app.accept_service(old, "session".into()),
            Err(Error::Gone(_))
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(app.live.lock().unwrap().service.entries.len(), MAX_RECEIPTS);
    }

    #[test]
    fn bounded_output_cancels_with_an_explicit_error_and_no_truncated_success() {
        let (app, mut receiver) = super::super::tests::app();
        let input = input();
        app.accept_service(input.clone(), "session".into()).unwrap();
        let work = receiver.try_recv().unwrap();
        let context = TraceContext::root();
        app.emit(AgentEvent::TextDelta {
            text: "x".repeat(MAX_OUTPUT_BYTES + 1),
            context: context.clone(),
        });
        assert!(app.live.lock().unwrap().service.draft.len() <= MAX_OUTPUT_BYTES);
        app.emit(AgentEvent::GenerationFinished {
            context: context.clone(),
        });
        assert!(
            !work.cancel.is_cancelled(),
            "validation alone has not committed output"
        );
        app.emit(AgentEvent::GenerationCommitted { context });
        assert!(work.cancel.is_cancelled());
        app.finish_service(input.request_id, &Ok(()), true);
        let output = app
            .service_output(input.instance, input.request_id, 0)
            .unwrap();
        assert_eq!(output.exit_code, Some(1));
        assert!(output.error.unwrap().contains("4 MiB"));
        assert!(output.output.is_empty());
    }

    #[test]
    fn validated_output_waits_for_its_checkpoint_and_later_save_failure_keeps_prior_chunks() {
        let (app, mut receiver) = super::super::tests::app();
        let input = input();
        app.accept_service(input.clone(), "session".into()).unwrap();
        let _work = receiver.try_recv().unwrap();
        let context = TraceContext::root();
        app.emit(AgentEvent::TextDelta {
            text: "saved".into(),
            context: context.clone(),
        });
        app.emit(AgentEvent::GenerationFinished {
            context: context.clone(),
        });
        assert!(
            app.service_output(input.instance, input.request_id, 0)
                .unwrap()
                .output
                .is_empty()
        );
        app.emit(AgentEvent::GenerationCommitted {
            context: context.clone(),
        });
        app.emit(AgentEvent::GenerationStarted {
            context: context.clone(),
        });
        app.emit(AgentEvent::TextDelta {
            text: "not saved".into(),
            context: context.clone(),
        });
        app.emit(AgentEvent::GenerationFinished { context });
        app.finish_service(input.request_id, &Err("checkpoint failed".into()), false);
        let output = app
            .service_output(input.instance, input.request_id, 0)
            .unwrap();
        assert_eq!(output.output, "saved");
        assert_eq!(output.exit_code, Some(1));
        assert_eq!(output.error.as_deref(), Some("checkpoint failed"));
    }

    #[test]
    fn output_chunks_preserve_unicode_and_finish_only_after_all_committed_bytes() {
        let (app, mut receiver) = super::super::tests::app();
        let input = input();
        app.accept_service(input.clone(), "session".into()).unwrap();
        receiver.try_recv().unwrap();
        let text = format!("{}🦊done", "x".repeat(CHUNK_BYTES - 1));
        complete(&app, input.request_id, &text);
        let first = app
            .service_output(input.instance, input.request_id, 0)
            .unwrap();
        assert_eq!(first.exit_code, None);
        assert_eq!(first.next_offset, CHUNK_BYTES - 1);
        let second = app
            .service_output(input.instance, input.request_id, first.next_offset)
            .unwrap();
        assert_eq!(second.exit_code, Some(0));
        assert_eq!(first.output + &second.output, text);
    }

    #[test]
    fn identity_capacity_rejects_new_work_without_evicting_recent_completion() {
        let (app, mut receiver) = super::super::tests::app();
        let accepted = input();
        app.accept_service(accepted.clone(), "session".into())
            .unwrap();
        receiver.try_recv().unwrap();
        complete(&app, accepted.request_id, "kept");
        {
            let mut live = app.live.lock().unwrap();
            while live.service.accepted.len() < MAX_REQUESTS {
                live.service.accepted.insert(Uuid::new_v4(), [0; 32]);
            }
        }
        assert!(matches!(
            app.accept_service(input(), "session".into()),
            Err(Error::Unavailable(_))
        ));
        app.accept_service(accepted.clone(), "session".into())
            .unwrap();
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            app.service_output(accepted.instance, accepted.request_id, 0)
                .unwrap()
                .output,
            "kept"
        );
    }
}
