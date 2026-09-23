//! Terminal adapters reuse the browser's session composition and durable runner.

use std::sync::Arc;

use myco::{AgentInteractionError, CancelToken, EventSink, Session};

use super::{Args, Boot, boot_session, prepare_boot};

mod interactive;
mod output;
mod print;

pub(super) use interactive::run_interactive;
pub(super) use print::run_print;

//
// Shared session lifecycle
//

async fn boot<S: EventSink + 'static>(args: &Args, sink: Arc<S>) -> Result<Boot, String> {
    let (config, mut model, preflight) = prepare_boot(args);
    let session = match args.resume.as_deref() {
        Some(id) => Session::load_by_id_or_prefix(id)?,
        None => Session::new(model.spec.key.clone()),
    };
    if args.resume.is_some() && args.model.is_none() {
        let key = myco::RuntimeRecord::latest(&session.active_thread().messages)
            .map_or_else(|| session.model.clone(), |record| record.model.key);
        model = config
            .models
            .get(&key)
            .map_err(|error| error.to_string())?
            .clone();
    }
    if preflight.has_problems() {
        eprintln!("{}", preflight.warning_body());
    }
    let (mut boot, _) =
        boot_session(args, config, model, preflight, session, |_, _, _| sink).await?;
    boot.runner.set_observer(Arc::new(|event| match event {
        myco::chat::WorkflowEvent::Compacting { .. } => eprintln!("myco: compacting…"),
        myco::chat::WorkflowEvent::Compacted(_) => eprintln!("myco: compaction complete"),
        myco::chat::WorkflowEvent::Warning(message) => eprintln!("myco: {message}"),
        myco::chat::WorkflowEvent::CompactionProgress { .. } => {}
    }));
    Ok(boot)
}

async fn interrupted<T>(cancel: CancelToken, action: impl std::future::Future<Output = T>) -> T {
    tokio::pin!(action);
    tokio::select! {
        result = &mut action => result,
        signal = tokio::signal::ctrl_c() => {
            match signal {
                Ok(()) => cancel.cancel(),
                Err(error) => eprintln!("myco: cannot listen for Ctrl-C: {error}"),
            }
            // Settle tool results and checkpoints before releasing the session.
            action.await
        }
    }
}

fn report_session(boot: &Boot) {
    if boot.session.snapshot().json_path().exists() {
        eprintln!("session={}", boot.session.id());
    }
}

fn report_outcome(outcome: myco::chat::SessionTurnOutcome) -> Result<(), (u8, String)> {
    if outcome.rewound.is_some() {
        eprintln!(
            "myco: the rejected input was removed from active context; its observations remain in the predecessor thread"
        );
    }
    outcome.result.map(|_| ()).map_err(|error| {
        let code = if matches!(error, AgentInteractionError::Cancelled) {
            130
        } else {
            1
        };
        (code, error.to_string())
    })
}
