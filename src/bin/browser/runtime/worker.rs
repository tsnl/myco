//! Each session's worker loop, and admission of actions into it.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use myco::CancelToken;
use myco::session::ActiveSession;
use tokio::sync::mpsc;

use super::queue::{MAX_QUEUED_MESSAGES, QueueUpdate};
use super::{
    Action, ActionRequest, App, Block, Error, Live, Result, Status, Work, attachments, view,
};
use crate::{Args, Boot, persist_session};

pub(super) async fn worker(
    mut boot: Boot,
    app: Arc<App>,
    mut receiver: mpsc::Receiver<Work>,
    args: Arc<Args>,
) {
    let runtime = boot.runner.runtime().clone();
    let harness = boot.harness.clone();
    let resources = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            app.refresh_metadata(runtime.session());
            app.hosts(harness.host_status());
            app.resources(runtime.resources().await);
        }
    };
    let observations = async { tokio::join!(resources, app.tick_timers()) };
    tokio::pin!(observations);
    loop {
        let work = tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = &mut observations => unreachable!("resource observation loop ended"),
            work = receiver.recv() => match work { Some(work) => work, None => break },
        };
        let request_id = work.request.request_id;
        let cancel = work.cancel.clone();
        let result = {
            let operation = execute(&mut boot, &app, work, &args);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                _ = &mut observations => unreachable!("resource observation loop ended"),
            }
        };
        let settled = app.settle_events(&boot.session.snapshot(), &result, cancel.is_cancelled());
        app.sync_with(
            &boot,
            if result.is_ok() {
                Status::Ready
            } else {
                Status::Stopped
            },
            |receipts| receipts.finish(request_id, &result, cancel.is_cancelled()),
        );
        if let Err(error) = settled {
            app.notice(format!("Inbox: {error}"));
        }
        if let Err(error) = result {
            app.notice(error);
        }
    }
    if let Err(error) = persist_session(boot.runner.agent(), &boot.session, false) {
        eprintln!("browser: {error}");
    }
}

async fn execute(
    boot: &mut Boot,
    app: &Arc<App>,
    work: Work,
    args: &Args,
) -> std::result::Result<(), String> {
    match work.request.action {
        Action::UpdateQueued { .. } | Action::CancelTimer { .. } => {
            unreachable!("queue mutations do not create worker actions")
        }
        Action::Timer { message } => {
            let content = message.content(boot.catalog_model.spec.max_image_base64_bytes)?;
            submit(boot, app, content, work.accepted_at, work.cancel).await
        }
        Action::Submit { text, images } => match attachments::content(
            &text,
            &images,
            boot.catalog_model.spec.max_image_base64_bytes,
        ) {
            Err(error) => Err(error),
            Ok(content) => submit(boot, app, content, work.accepted_at, work.cancel).await,
        },
        Action::ServiceSubmit { text, images } => {
            let content = attachments::literal_content(
                &text,
                &images,
                boot.catalog_model.spec.max_image_base64_bytes,
            )?;
            submit(boot, app, content, work.accepted_at, work.cancel).await
        }
        Action::Compact => boot
            .runner
            .compact(work.cancel)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Action::SelectModel { key } => {
            let catalog = boot.app_config.models.get(&key)?.clone();
            crate::select_runner_model(
                &mut boot.runner,
                &boot.harness,
                &catalog,
                args.effort,
                args.debug_dump_api_requests,
                boot.app_config.compaction_max_requests,
            )
            .await?;
            boot.catalog_model = catalog;
            Ok(())
        }
    }
}

async fn submit(
    boot: &mut Boot,
    app: &Arc<App>,
    content: Vec<myco::generative_model::Content>,
    time: DateTime<Utc>,
    cancel: CancelToken,
) -> std::result::Result<(), String> {
    app.append_block(Block::input(&content, Some(view::timestamp(&time))));
    let followups = app.clone();
    let image_limit = boot.catalog_model.spec.max_image_base64_bytes;
    boot.runner
        .set_followup_handler(Some(Arc::new(move |agent, session| {
            followups.deliver_followups(agent, session, image_limit)
        })));
    let outcome = boot.runner.submit(content, time, cancel).await;
    boot.runner.set_followup_handler(None);
    outcome.result.map(|_| ()).map_err(|e| e.to_string())
}

impl App {
    fn deliver_followups(
        &self,
        agent: &mut myco::agent::Agent,
        session: &ActiveSession,
        image_limit: u64,
    ) -> std::result::Result<bool, myco::agent::AgentInteractionError> {
        let mut delivered = false;
        for _ in 0..MAX_QUEUED_MESSAGES {
            let Some(next) = self
                .claim_followup()
                .map_err(|e| myco::agent::AgentInteractionError::Checkpoint(e.to_string()))?
            else {
                break;
            };
            let content = match next.content(image_limit) {
                Ok(content) => content,
                Err(error) => {
                    self.notice(format!("Queued message could not be sent: {error}"));
                    self.finish_followup(next.request_id, None);
                    continue;
                }
            };
            if let Err(error) =
                myco::chat::append_followup(agent, session, content.clone(), next.accepted_at)
            {
                self.restore_followup(next.request_id)
                    .map_err(|e| myco::agent::AgentInteractionError::Checkpoint(e.to_string()))?;
                return Err(error);
            }
            let block = Block::input(&content, Some(view::timestamp(&next.accepted_at)));
            self.finish_followup(next.request_id, Some(block));
            delivered = true;
        }
        Ok(delivered)
    }

    pub(crate) fn accept(&self, mut request: ActionRequest) -> Result<()> {
        if let Action::Submit { images, .. }
        | Action::UpdateQueued {
            update: QueueUpdate::Save { images, .. },
            ..
        } = &mut request.action
        {
            attachments::externalize(images).map_err(Error::Invalid)?;
        }
        let mut live = self.live.lock().unwrap();
        if self.shutdown.is_cancelled() {
            return Err(Error::Unavailable("The server is stopping.".into()));
        }
        if live.service.contains(&request.request_id) {
            return Err(Error::Conflict(
                "Request id already used for a native service turn.".into(),
            ));
        }
        if let Some(accepted) = live.accepted.get(&request.request_id) {
            return if accepted == &request {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "Request id already used for another action.".into(),
                ))
            };
        }
        if live.inbox.get(request.request_id).is_some() {
            return Err(Error::Conflict(
                "Request ID already belongs to an inbox event.".into(),
            ));
        }
        if request.session_id != live.snapshot.meta.session_id {
            return Err(Error::Conflict(
                "The request belongs to a different session.".into(),
            ));
        }
        let queue_update = matches!(
            request.action,
            Action::UpdateQueued { .. } | Action::CancelTimer { .. }
        );
        if live.snapshot.meta.busy()
            && !matches!(
                request.action,
                Action::Submit { .. } | Action::UpdateQueued { .. } | Action::CancelTimer { .. }
            )
        {
            return Err(Error::Conflict(
                "Wait for the current run or cancel it first.".into(),
            ));
        }
        if live.snapshot.meta.cancelling() && !queue_update {
            return Err(Error::Conflict("Wait for cancellation to finish.".into()));
        }
        let accepted_at = Utc::now();
        if self.work.is_closed() {
            return Err(Error::Unavailable(
                "The session worker is unavailable.".into(),
            ));
        }
        match &request.action {
            Action::Submit { .. } => self.enqueue(&mut live, &request, accepted_at)?,
            Action::UpdateQueued {
                message_id,
                revision,
                update,
            } => self.update_queued(&mut live, *message_id, *revision, update)?,
            Action::CancelTimer { timer_id } => self.remove_timer(&mut live, *timer_id)?,
            _ => self.start_work(&mut live, request.clone(), accepted_at)?,
        }
        self.refresh_inbox(&mut live);
        live.accepted.insert(request.request_id, request);
        if let Err(error) = self.drain_queue(&mut live) {
            let block = Block::Notice {
                text: error.to_string(),
            };
            self.push_block(&mut live.snapshot, block);
        }
        self.publish_metadata(&mut live.snapshot);
        Ok(())
    }

    pub(super) fn start_work(
        &self,
        live: &mut Live,
        request: ActionRequest,
        accepted_at: DateTime<Utc>,
    ) -> Result<()> {
        let cancel = CancelToken::new();
        self.work
            .try_send(Work {
                request,
                cancel: cancel.clone(),
                accepted_at,
            })
            .map_err(|_| Error::Unavailable("The session worker is unavailable.".into()))?;
        live.snapshot.meta.cancel = Some(cancel);
        live.snapshot.meta.status = Status::Running;
        Ok(())
    }

    pub(crate) fn cancel(&self, session_id: &str) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if session_id != live.snapshot.meta.session_id {
            return Err(Error::Conflict(
                "The request belongs to a different session.".into(),
            ));
        }
        self.cancel_live(&mut live);
        Ok(())
    }

    pub(super) fn cancel_live(&self, live: &mut Live) {
        if let Some(cancel) = &live.snapshot.meta.cancel {
            cancel.cancel();
            live.snapshot.meta.status = Status::Cancelling;
        }
        self.publish_metadata(&mut live.snapshot);
    }
}
