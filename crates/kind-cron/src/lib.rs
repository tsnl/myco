//! The `cron` kind: a table of schedules, each firing one verb at one
//! instance on a fixed cadence. One instance is one table — DESIGN.md's
//! own example of an actor — so a workspace's automation is a pane you
//! can read, not a config file you must find.
//!
//! The attribution doctrine is the whole design: an entry fires **as the
//! principal who added it**. System never starts turns (the trigger
//! rule), but a scheduled message is not System speaking — it is its
//! author speaking on a timer, and the target's records say so. `fire`
//! (run now) follows the same rule from the other side: it runs as
//! whoever pulled the trigger, never as the entry's author — a verb you
//! could not call yourself does not become callable by scheduling it
//! behind someone else's name.
//!
//! Each entry's timer is a side-feed task: reads the table through
//! [`Shared`], calls the pool, records the run, bumps. An entry aimed at
//! an instance that stops existing pauses itself with the reason — a
//! schedule aimed at nothing must not grind out failures forever.
//!
//! Cadence is `every_secs`, deliberately: wall-clock schedules ("daily
//! at 9") need a timezone story this kind does not yet have, and a wrong
//! timezone fires a real verb at a wrong hour. Deferred, named here.

use std::collections::HashMap;

use myco_instance::{
    CreateCtx, Instance, Kind, KindSpec, Pool, Principal, Shared, VerbError, VerbSpec,
};
use myco_runtime::Signals;
use serde_json::{Value, json};

static CRON_SPEC: KindSpec = KindSpec {
    kind: "cron",
    version: 1,
    doc: "a table of schedules: each entry fires one verb at one instance \
          every N seconds, as the person who added it",
    verbs: &[
        VerbSpec::read("about", "the table: entries, cadences, last runs"),
        VerbSpec::read("text", "the table, plainly"),
        VerbSpec::write(
            "add",
            "schedule {target, verb, args?, every_secs} — fires as you; answers {entry}",
        ),
        VerbSpec::write("rm", "remove entry {entry}"),
        VerbSpec::write("pause", "hold entry {entry} without forgetting it"),
        VerbSpec::write("resume", "let entry {entry} fire again"),
        VerbSpec::write("fire", "run entry {entry} now, as you"),
        VerbSpec::cursored_read("runs", "the run log from {from}: {runs, next}"),
    ],
    primary_render: "about",
    recommended_context: "text",
};

/// How many finished runs the table remembers. Old runs fall off the
/// front; `runs` cursors stay honest because offsets are absolute.
const RUN_LOG_CAP: usize = 128;

pub struct CronKind {
    pool: Pool,
}

impl CronKind {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl Kind for CronKind {
    fn spec(&self) -> &'static KindSpec {
        &CRON_SPEC
    }

    fn create(
        &self,
        _ctx: &CreateCtx,
        _args: Value,
        signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        Ok(Box::new(Cron {
            pool: self.pool.clone(),
            shared: Shared::new(Table::default(), signals),
            timers: HashMap::new(),
        }))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct Entry {
    entry: u64,
    /// Who scheduled it — and therefore who every firing runs as.
    by: Principal,
    target: String,
    verb: String,
    args: Value,
    every_secs: u64,
    paused: bool,
    /// Why it paused itself, when it did (the target went away).
    #[serde(skip_serializing_if = "Option::is_none")]
    parked: Option<String>,
    fired: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Run {
    entry: u64,
    at: chrono::DateTime<chrono::Utc>,
    as_: Principal,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Default)]
struct Table {
    next: u64,
    entries: Vec<Entry>,
    /// The run log as an absolute sequence: `runs_start` is the offset of
    /// `runs[0]`, so cursors survive trimming.
    runs_start: u64,
    runs: Vec<Run>,
}

impl Table {
    fn record(&mut self, run: Run) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.entry == run.entry) {
            entry.fired += 1;
        }
        self.runs.push(run);
        if self.runs.len() > RUN_LOG_CAP {
            let cut = self.runs.len() - RUN_LOG_CAP;
            self.runs.drain(..cut);
            self.runs_start += cut as u64;
        }
    }
}

struct Cron {
    pool: Pool,
    shared: Shared<Table>,
    timers: HashMap<u64, tokio::task::JoinHandle<()>>,
}

impl Drop for Cron {
    fn drop(&mut self) {
        for (_, timer) in self.timers.drain() {
            timer.abort();
        }
    }
}

#[async_trait::async_trait]
impl Instance for Cron {
    async fn verb(
        &mut self,
        caller: &Principal,
        verb: &str,
        args: Value,
        _signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "about" => Ok(self.shared.read(|t| {
                json!({
                    "entries": t.entries,
                    "runs": t.runs_start + t.runs.len() as u64,
                })
            })),
            "text" => Ok(Value::String(self.shared.read(|t| {
                if t.entries.is_empty() {
                    return "an empty cron table".to_string();
                }
                t.entries
                    .iter()
                    .map(|e| {
                        let state = match (&e.parked, e.paused) {
                            (Some(why), _) => format!("parked: {why}"),
                            (None, true) => "paused".into(),
                            (None, false) => format!("every {}s", e.every_secs),
                        };
                        format!(
                            "#{} {} {} on {} — {} ({} fired, by {})",
                            e.entry, e.verb, e.args, e.target, state, e.fired, e.by
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }))),
            "add" => {
                let target = args
                    .get("target")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "add needs {target} — an instance id".into(),
                    })?
                    .to_string();
                let target_verb = args
                    .get("verb")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "add needs {verb}".into(),
                    })?
                    .to_string();
                let every_secs = args
                    .get("every_secs")
                    .and_then(Value::as_u64)
                    .filter(|s| *s >= 1)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "add needs {every_secs} >= 1".into(),
                    })?;
                let call_args = args.get("args").cloned().unwrap_or(Value::Null);
                let entry = self.shared.with(|t| {
                    t.next += 1;
                    let entry = Entry {
                        entry: t.next,
                        by: caller.clone(),
                        target,
                        verb: target_verb,
                        args: call_args,
                        every_secs,
                        paused: false,
                        parked: None,
                        fired: 0,
                    };
                    t.entries.push(entry.clone());
                    entry
                });
                self.timers.insert(
                    entry.entry,
                    tokio::spawn(tick(
                        self.pool.clone(),
                        self.shared.clone(),
                        entry.entry,
                        every_secs,
                    )),
                );
                Ok(json!({ "entry": entry.entry }))
            }
            "rm" => {
                let id = entry_arg(&args)?;
                let removed = self.shared.with(|t| {
                    let before = t.entries.len();
                    t.entries.retain(|e| e.entry != id);
                    t.entries.len() < before
                });
                if !removed {
                    return Err(unknown_entry(id));
                }
                if let Some(timer) = self.timers.remove(&id) {
                    timer.abort();
                }
                Ok(Value::Null)
            }
            "pause" | "resume" => {
                let id = entry_arg(&args)?;
                let paused = verb == "pause";
                let found = self.shared.with(|t| {
                    t.entries.iter_mut().find(|e| e.entry == id).map(|e| {
                        e.paused = paused;
                        // Resuming clears a parking: the person saying "go
                        // again" is saying the target is back.
                        if !paused {
                            e.parked = None;
                        }
                    })
                });
                found.map(|()| Value::Null).ok_or_else(|| unknown_entry(id))
            }
            "fire" => {
                let id = entry_arg(&args)?;
                let entry = self
                    .shared
                    .read(|t| t.entries.iter().find(|e| e.entry == id).cloned())
                    .ok_or_else(|| unknown_entry(id))?;
                // As the firer, not the author — see the module doc.
                let result = self
                    .pool
                    .call(caller, &entry.target, &entry.verb, entry.args.clone())
                    .await;
                let ok = result.is_ok();
                self.shared.with(|t| {
                    t.record(Run {
                        entry: id,
                        at: chrono::Utc::now(),
                        as_: caller.clone(),
                        ok,
                        error: result.as_ref().err().map(|e| e.to_string()),
                    })
                });
                result
            }
            "runs" => {
                let from = args.get("from").and_then(Value::as_u64).unwrap_or(0);
                Ok(self.shared.read(|t| {
                    let end = t.runs_start + t.runs.len() as u64;
                    let from = from.clamp(t.runs_start, end);
                    let slice = &t.runs[(from - t.runs_start) as usize..];
                    json!({ "runs": slice, "next": end })
                }))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

fn entry_arg(args: &Value) -> Result<u64, VerbError> {
    args.get("entry")
        .and_then(Value::as_u64)
        .ok_or_else(|| VerbError::BadArgs {
            why: "needs {entry} — a number from add's answer".into(),
        })
}

fn unknown_entry(id: u64) -> VerbError {
    VerbError::BadArgs {
        why: format!("no entry #{id} in this table"),
    }
}

/// One entry's clock. Reads the entry fresh each tick — pause, removal,
/// and edits all take effect at the next beat — and parks the entry when
/// its target stops existing.
async fn tick(pool: Pool, shared: Shared<Table>, id: u64, every_secs: u64) {
    let period = std::time::Duration::from_secs(every_secs);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(entry) = shared.read(|t| t.entries.iter().find(|e| e.entry == id).cloned())
        else {
            return;
        };
        if entry.paused {
            continue;
        }
        let result = pool
            .call(&entry.by, &entry.target, &entry.verb, entry.args.clone())
            .await;
        let gone = matches!(
            result,
            Err(VerbError::UnknownInstance { .. }) | Err(VerbError::Gone)
        );
        let ok = result.is_ok();
        let error = result.err().map(|e| e.to_string());
        shared.with(|t| {
            t.record(Run {
                entry: id,
                at: chrono::Utc::now(),
                as_: entry.by.clone(),
                ok,
                error: error.clone(),
            });
            if gone && let Some(e) = t.entries.iter_mut().find(|e| e.entry == id) {
                e.paused = true;
                e.parked = Some(format!("target {} is gone", entry.target));
            }
        });
    }
}

#[cfg(test)]
mod tests;
