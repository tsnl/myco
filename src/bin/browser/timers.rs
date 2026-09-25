//! One-shot wakeups owned by a live server session. Expiry transfers a message
//! into the existing FIFO; model execution still waits for a settled boundary.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use myco::core::Async;
use myco::generative_model::{ToolResult, ToolSpec, ToolUse};
use myco::tool_services::{HostDispatchContext, ToolService, tool_input_schema};
use serde::{Deserialize, Serialize};
use tokio::time::Instant as Deadline;
use uuid::Uuid;

use super::queue::{MAX_QUEUED_MESSAGES, QueueState, QueuedMessage, TimerOrigin};
use super::{App, Error, Live, Result};

//
// Scheduled wakeups
//

const MAX_TIMERS: usize = 20;
const MAX_DELAY: f64 = 30.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone, Serialize)]
pub(super) struct Timer {
    pub(super) id: Uuid,
    pub(super) message: String,
    pub(super) due_at: DateTime<Utc>,
    #[serde(rename = "remaining_ms", serialize_with = "remaining_ms")]
    deadline: Deadline,
}

fn remaining_ms<S: serde::Serializer>(
    deadline: &Deadline,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_u64(
        deadline
            .saturating_duration_since(Deadline::now())
            .as_millis() as u64,
    )
}

//
// Model-facing tool
//

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TimerAction {
    Set,
    List,
    Cancel,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Input {
    action: TimerAction,
    /// For set: delay in seconds, from 0.1 seconds to 30 days. Use this OR at.
    after_seconds: Option<f64>,
    /// For set: future RFC 3339 timestamp with timezone, e.g. 2026-09-25T09:00:00-07:00.
    at: Option<String>,
    /// For set: follow-up to deliver to this session when the timer fires.
    message: Option<String>,
    /// For cancel: ID returned by set or list.
    #[schemars(with = "Option<String>")]
    timer_id: Option<Uuid>,
}

impl ToolService for App {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "timer".into(),
            description: "Schedule a one-shot follow-up in this server session without blocking. \
                set requires message and either after_seconds or at (RFC 3339 with timezone). \
                Returns a timer ID and due time. list shows scheduled and queued timers; cancel \
                requires timer_id and works until delivery starts. When due, the message joins \
                the session queue and wakes an idle assistant, or arrives at the next settled \
                tool/model boundary. You can finish your turn; no shell sleep or polling is needed. \
                Timers survive tab closes, model changes and compaction, including in archived \
                sessions, while this server runs. Restarting the server clears them. Cancel run \
                stops the current turn; cancel timers separately. Up to 20 timers, each within \
                30 days and with at most 8000 message characters. No recurring schedules.".into(),
            input_schema: tool_input_schema::<Input>(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool: ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            if ctx.cancel.is_cancelled() {
                return ToolResult::err("cancelled");
            }
            let result = serde_json::from_value::<Input>(tool.input)
                .map_err(|e| Error::Invalid(format!("Invalid timer input: {e}")))
                .and_then(|input| self.timer_action(input));
            match result {
                Ok(text) => ToolResult::text(text),
                Err(error) => ToolResult::err(error.to_string()),
            }
        })
    }
}

impl App {
    fn timer_action(&self, input: Input) -> Result<String> {
        match input.action {
            TimerAction::Set => {
                if input.timer_id.is_some() {
                    return Err(Error::Invalid("set does not accept timer_id.".into()));
                }
                let timer = self.set_timer(
                    input.after_seconds,
                    input.at.as_deref(),
                    input
                        .message
                        .ok_or_else(|| Error::Invalid("set requires message.".into()))?,
                )?;
                Ok(format!(
                    "Timer {} scheduled for {}.\n{}\nThis session will resume automatically. The server must remain running.",
                    timer.id,
                    timer.due_at.to_rfc3339(),
                    timer.message
                ))
            }
            TimerAction::List | TimerAction::Cancel => {
                if input.after_seconds.is_some() || input.at.is_some() || input.message.is_some() {
                    return Err(Error::Invalid(
                        "Only set accepts after_seconds, at and message.".into(),
                    ));
                }
                if matches!(input.action, TimerAction::List) {
                    if input.timer_id.is_some() {
                        return Err(Error::Invalid("list does not accept timer_id.".into()));
                    }
                    return Ok(self.list_timers());
                }
                let id = input
                    .timer_id
                    .ok_or_else(|| Error::Invalid("cancel requires timer_id.".into()))?;
                self.cancel_timer(id)?;
                Ok(format!("Timer {id} cancelled."))
            }
        }
    }
}

//
// Scheduling and queue delivery
//

impl App {
    pub(super) fn set_timer(
        &self,
        after: Option<f64>,
        at: Option<&str>,
        message: String,
    ) -> Result<Timer> {
        let message = message.trim().to_owned();
        if message.is_empty() || message.chars().count() > 8000 {
            return Err(Error::Invalid(
                "Timer message must contain 1–8000 characters.".into(),
            ));
        }
        let (delay, due_at) = timer_deadline(after, at)?;
        let timer = Timer {
            id: Uuid::new_v4(),
            message,
            due_at,
            deadline: Deadline::now() + delay,
        };
        let mut live = self.live.lock().unwrap();
        if self.shutdown.is_cancelled() || self.work.is_closed() {
            return Err(Error::Unavailable(
                "The session worker is unavailable.".into(),
            ));
        }
        let queued = live
            .snapshot
            .queued
            .iter()
            .filter(|message| message.timer.is_some())
            .count();
        if live.snapshot.timers.len() + queued >= MAX_TIMERS {
            return Err(Error::Conflict(
                "This session already has 20 pending timers.".into(),
            ));
        }
        live.snapshot.timers.push(timer.clone());
        live.snapshot.timers.sort_by_key(|timer| timer.deadline);
        self.publish_queue(&mut live.snapshot);
        Ok(timer)
    }

    fn list_timers(&self) -> String {
        let live = self.live.lock().unwrap();
        let mut lines: Vec<_> = live
            .snapshot
            .timers
            .iter()
            .map(|timer| {
                format!(
                    "{} · {} · {}",
                    timer.id,
                    timer.due_at.to_rfc3339(),
                    timer.message
                )
            })
            .collect();
        lines.extend(live.snapshot.queued.iter().filter_map(|message| {
            message.timer.as_ref().map(|timer| {
                format!(
                    "{} · {} · {}",
                    timer.id,
                    if message.state == QueueState::Sending {
                        "sending"
                    } else {
                        "queued"
                    },
                    message.text
                )
            })
        }));
        if lines.is_empty() {
            "No pending timers.".into()
        } else {
            lines.join("\n")
        }
    }

    pub(super) fn cancel_timer(&self, id: Uuid) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        self.remove_timer(&mut live, id)?;
        self.drain_queue(&mut live)?;
        self.publish_queue(&mut live.snapshot);
        Ok(())
    }

    pub(super) fn remove_timer(&self, live: &mut Live, id: Uuid) -> Result<()> {
        if let Some(index) = live.snapshot.timers.iter().position(|timer| timer.id == id) {
            live.snapshot.timers.remove(index);
            return Ok(());
        }
        let index = live
            .snapshot
            .queued
            .iter()
            .position(|message| message.timer.as_ref().is_some_and(|timer| timer.id == id))
            .ok_or_else(|| {
                Error::Conflict(
                    "Timer already delivered, cancelled, or belongs to another session.".into(),
                )
            })?;
        if live.snapshot.queued[index].state == QueueState::Sending {
            return Err(Error::Conflict(
                "This timer is already being delivered.".into(),
            ));
        }
        live.snapshot.queued.remove(index);
        Ok(())
    }

    pub(super) fn fire_timers(&self) {
        let mut live = self.live.lock().unwrap();
        if self.shutdown.is_cancelled()
            || live.snapshot.status == "Cancelling"
            || self.work.is_closed()
        {
            return;
        }
        let mut changed = false;
        while live.snapshot.queued.len() < MAX_QUEUED_MESSAGES
            && live
                .snapshot
                .timers
                .first()
                .is_some_and(|timer| timer.deadline <= Deadline::now())
        {
            let timer = live.snapshot.timers.remove(0);
            live.snapshot.queued.push_back(QueuedMessage {
                request_id: timer.id,
                text: timer.message,
                images: vec![],
                accepted_at: Utc::now(),
                revision: 0,
                state: QueueState::Ready,
                timer: Some(TimerOrigin {
                    id: timer.id,
                    scheduled_for: timer.due_at,
                }),
            });
            changed = true;
        }
        if !changed {
            return;
        }
        let result = self.drain_queue(&mut live);
        self.publish_queue(&mut live.snapshot);
        drop(live);
        if let Err(error) = result {
            self.notice(format!("Timer delivery: {error}"));
        }
    }

    pub(super) async fn tick_timers(&self) {
        // Resource polling can await remote hosts; it must not delay wakeups.
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            self.fire_timers();
        }
    }
}

fn timer_deadline(after: Option<f64>, at: Option<&str>) -> Result<(Duration, DateTime<Utc>)> {
    let now = Utc::now();
    let seconds = match (after, at) {
        (Some(seconds), None) => seconds,
        (None, Some(at)) => {
            DateTime::parse_from_rfc3339(at)
                .map_err(|_| {
                    Error::Invalid("at must be an RFC 3339 timestamp including a timezone.".into())
                })?
                .signed_duration_since(now)
                .num_milliseconds() as f64
                / 1000.0
        }
        _ => {
            return Err(Error::Invalid(
                "set requires exactly one of after_seconds or at.".into(),
            ));
        }
    };
    if !seconds.is_finite() || !(0.1..=MAX_DELAY).contains(&seconds) {
        return Err(Error::Invalid(
            "Timer must be between 0.1 seconds and 30 days in the future.".into(),
        ));
    }
    let delay = Duration::from_secs_f64(seconds);
    Ok((
        delay,
        now + chrono::Duration::from_std(delay).expect("bounded timer duration"),
    ))
}
