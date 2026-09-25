//! Project retained shell processes onto their original tool cards. Resource
//! identities, rather than display text or reused handle names, bind a card to
//! a process. These live observations never rewrite recorded tool results.

use std::time::{Duration, Instant};

use chrono::Utc;
use myco::core::{HostResources, ToolResource};
use myco::generative_model::{ToolResourceRef, ToolUse};
use serde::Deserialize;
use serde_json::json;

use super::{Block, ToolTimer};

#[derive(Deserialize)]
struct Process {
    instance_id: String,
    command: String,
    started_at: i64,
    output_closed: bool,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    duration_ms: Option<u64>,
}

fn host(tool: &ToolUse) -> &str {
    tool.input["host"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("local")
}

fn process(resource: &ToolResource) -> Option<Process> {
    (resource.tool == "bash")
        .then(|| serde_json::from_value(resource.details.clone()).ok())
        .flatten()
}

/// Preserve ongoing cards through compaction, including when their original
/// messages no longer belong to the current thread.
pub(crate) fn retain_processes(blocks: &mut Vec<Block>, previous: &[Block]) {
    for previous in previous {
        let Block::Tool {
            tool,
            resource: Some(resource),
            running,
            ..
        } = previous
        else {
            continue;
        };
        let found = blocks.iter_mut().find(|block| matches!(block,
            Block::Tool {tool: other, resource: Some(id), ..} if id == resource && host(other) == host(tool)));
        if let Some(Block::Tool {
            status,
            error,
            running,
            timer,
            ..
        }) = found
        {
            if let Block::Tool {
                status: old_status,
                error: old_error,
                running: old_running,
                timer: old_timer,
                ..
            } = previous
            {
                *status = old_status.clone();
                *error = *old_error;
                *running = *old_running;
                *timer = old_timer.clone();
            }
        } else if *running {
            blocks.push(previous.clone());
        }
    }
}

pub(crate) fn refresh_processes(blocks: &mut Vec<Block>, hosts: &[HostResources]) -> Vec<usize> {
    let mut changed = Vec::new();
    for (index, block) in blocks.iter_mut().enumerate() {
        let Block::Tool {
            tool,
            resource: Some(id),
            waiting: false,
            ..
        } = block
        else {
            continue;
        };
        let observed = hosts.iter().find(|entry| entry.host == host(tool));
        let resource = observed
            .filter(|entry| entry.error.is_none())
            .and_then(|entry| entry.resources.as_ref())
            .and_then(|resources| {
                resources.iter().find(|resource| {
                    resource.id == id.id && resource.details["instance_id"] == id.instance_id
                })
            });
        let process = resource.and_then(process);
        let missing = if observed.is_some_and(|entry| entry.error.is_none()) {
            "not running"
        } else {
            "state unknown"
        };
        if update(block, process.as_ref(), missing) {
            changed.push(index);
        }
    }
    for entry in hosts.iter().filter(|entry| entry.error.is_none()) {
        for resource in entry.resources.iter().flatten() {
            let Some(process) = process(resource).filter(|process| !process.output_closed) else {
                continue;
            };
            if blocks
                .iter()
                .any(|block| owns(block, &entry.host, resource, &process))
            {
                continue;
            }
            let mut block = Block::tool(
                ToolUse {
                    name: "bash".into(),
                    input: json!({
                        "host": entry.host, "session_id": resource.id, "command": process.command,
                    }),
                },
                None,
            );
            if let Block::Tool {
                resource: handle,
                waiting,
                text,
                ..
            } = &mut block
            {
                *handle = Some(ToolResourceRef {
                    id: resource.id.clone(),
                    instance_id: process.instance_id.clone(),
                });
                *waiting = false;
                *text = "Live process retained outside this thread. Use bash read with this session_id and host to collect output.".into();
            }
            update(&mut block, Some(&process), "");
            changed.push(blocks.len());
            blocks.push(block);
        }
    }
    changed
}

fn owns(block: &Block, name: &str, resource: &ToolResource, process: &Process) -> bool {
    let Block::Tool {
        tool,
        resource: handle,
        waiting,
        ..
    } = block
    else {
        return false;
    };
    if host(tool) != name {
        return false;
    }
    if handle
        .as_ref()
        .is_some_and(|handle| handle.instance_id == process.instance_id)
    {
        return true;
    }
    // A start or promoted exec may still be returning its first snapshot.
    // Wait for that result instead of briefly creating a duplicate card.
    *waiting
        && tool.name == "bash"
        && (tool.input["session_id"] == resource.id || tool.input["command"] == process.command)
}

fn update(block: &mut Block, process: Option<&Process>, missing: &str) -> bool {
    let Block::Tool {
        status,
        error,
        running,
        timer,
        ..
    } = block
    else {
        return false;
    };
    // Removing a retained handle must not erase an exit already observed by
    // this server. Missing handles from disk still get an honest unknown state.
    if process.is_none()
        && !*running
        && (status.starts_with("exit ") || status.starts_with("signal ") || status == "finished")
    {
        return false;
    }
    let next_running = process.is_some_and(|p| !p.output_closed);
    let next_status = match process {
        Some(_) if next_running => "running".into(),
        Some(p) => p
            .exit_signal
            .map(|signal| format!("signal {signal}"))
            .or_else(|| p.exit_code.map(|code| format!("exit {code}")))
            .unwrap_or_else(|| "finished".into()),
        None => missing.into(),
    };
    let next_error = process.is_some_and(|p| {
        p.output_closed && (p.exit_signal.is_some() || p.exit_code.is_some_and(|code| code != 0))
    });
    let changed = *status != next_status || *running != next_running || *error != next_error;
    if !changed && timer.is_some() {
        return false;
    }
    *status = next_status;
    *running = next_running;
    *error = next_error;
    if let Some(p) = process {
        let elapsed = (Utc::now().timestamp_millis() - p.started_at).max(0) as u64;
        let clock = timer.get_or_insert_with(|| ToolTimer {
            started: Instant::now()
                .checked_sub(Duration::from_millis(elapsed))
                .unwrap_or_else(Instant::now),
            finished: None,
        });
        clock.finished = if next_running {
            None
        } else {
            p.duration_ms.map(Duration::from_millis)
        };
    }
    if !next_running && let Some(timer) = timer {
        timer.finish();
    }
    changed || process.is_some()
}
