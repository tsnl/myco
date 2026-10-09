use serde::{Deserialize, Serialize};

use crate::data::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum End {
    Answered,
    Cancelled,
    Budget,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub id: OperationId,
    pub outcome: Option<Outcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Ready,
    Generating(OperationId),
    Acting {
        generation: OperationId,
        calls: Vec<Pending>,
    },
    Failed(Failure),
    Finished(End),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub run: String,
    pub principal: Principal,
    pub context: Context,
    pub settings: Settings,
    pub phase: Phase,
    sequence: u64,
    inbox: Vec<Message>,
    stopping: Option<End>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Input {
    Advance,
    Generated {
        id: OperationId,
        result: Result<Completion, Failure>,
    },
    Observed {
        id: OperationId,
        outcome: Outcome,
    },
    Interject(Message),
    Stop(End),
    Retry,
    InstallContext(Context),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    Generate {
        id: OperationId,
        input: ModelInput,
    },
    Call {
        id: OperationId,
        principal: Principal,
        invocation: Call,
        binding: String,
    },
    Cancel(OperationId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub state: State,
    pub effects: Vec<Effect>,
    pub applied: bool,
}

impl State {
    pub fn new(run: &str, principal: Principal, context: Context, settings: Settings) -> Self {
        Self {
            run: run.into(),
            principal,
            context,
            settings,
            phase: Phase::Ready,
            sequence: 0,
            inbox: vec![],
            stopping: None,
        }
    }

    fn next_id(&mut self) -> OperationId {
        self.sequence += 1;
        OperationId {
            run: self.run.clone(),
            sequence: self.sequence,
        }
    }

    pub fn pending(&self) -> Vec<OperationId> {
        match &self.phase {
            Phase::Generating(id) => vec![id.clone()],
            Phase::Acting { calls, .. } => calls
                .iter()
                .filter(|c| c.outcome.is_none())
                .map(|c| c.id.clone())
                .collect(),
            _ => vec![],
        }
    }
}

/// Pure: the caller commits the input and effects before interpreting any effect.
pub fn advance(state: &State, input: &Input) -> Result<Transition, &'static str> {
    let mut next = state.clone();
    let mut effects = vec![];
    let mut applied = true;
    match input {
        Input::Advance if state.phase == Phase::Ready => {
            state.context.validate_ready()?;
            let id = next.next_id();
            effects.push(Effect::Generate {
                id: id.clone(),
                input: ModelInput {
                    context: state.context.clone(),
                    settings: state.settings.clone(),
                },
            });
            next.phase = Phase::Generating(id);
        }
        Input::Generated { id, result } if state.phase == Phase::Generating(id.clone()) => {
            match result {
                Err(failure) => next.phase = Phase::Failed(failure.clone()),
                Ok(completion) if completion.finish == Finish::Limit => {
                    // This controller requires an explicit continuation/retry policy.
                    // The original truncated response remains in the operation record.
                    next.phase = Phase::Failed(Failure {
                        reason: "generation reached its output limit".into(),
                        partial: completion.reply.content.clone(),
                    });
                }
                Ok(completion) => {
                    let reply = &completion.reply;
                    next.context = next.context.append([Message::Assistant {
                        generation: id.clone(),
                        reply: reply.clone(),
                    }]);
                    if reply.calls.is_empty() {
                        next.phase = Phase::Finished(End::Answered);
                    } else {
                        let calls = reply
                            .calls
                            .iter()
                            .map(|invocation| {
                                let operation = next.next_id();
                                let outcome = match state
                                    .settings
                                    .tools
                                    .iter()
                                    .find(|t| t.name == invocation.tool)
                                {
                                    Some(tool) => {
                                        effects.push(Effect::Call {
                                            id: operation.clone(),
                                            principal: state.principal.clone(),
                                            invocation: invocation.clone(),
                                            binding: tool.binding.clone(),
                                        });
                                        None
                                    }
                                    None => Some(Outcome::Failed(format!(
                                        "unknown tool {}",
                                        invocation.tool
                                    ))),
                                };
                                Pending {
                                    id: operation,
                                    outcome,
                                }
                            })
                            .collect();
                        next.phase = Phase::Acting {
                            generation: id.clone(),
                            calls,
                        };
                        settle(&mut next);
                    }
                }
            }
        }
        Input::Observed { id, outcome } => {
            applied = false;
            if let Phase::Acting { calls, .. } = &mut next.phase
                && let Some(call) = calls
                    .iter_mut()
                    .find(|c| &c.id == id && c.outcome.is_none())
            {
                call.outcome = Some(outcome.clone());
                applied = true;
                settle(&mut next);
            }
        }
        Input::Interject(message) => {
            if !matches!(message, Message::Input { .. }) {
                return Err("an interjection must be attributed input");
            }
            if next.stopping.is_some() || matches!(state.phase, Phase::Finished(_)) {
                return Err("run is stopping or finished");
            }
            effects.extend(state.pending().into_iter().map(Effect::Cancel));
            if matches!(state.phase, Phase::Acting { .. }) {
                next.inbox.push(message.clone());
            } else {
                next.context = next.context.append([message.clone()]);
                next.phase = Phase::Ready;
            }
        }
        Input::Stop(end) => {
            if *end == End::Answered {
                return Err("a stop request cannot claim a model answer");
            }
            if matches!(state.phase, Phase::Finished(_)) || next.stopping.is_some() {
                applied = false;
            } else {
                effects.extend(state.pending().into_iter().map(Effect::Cancel));
                if matches!(state.phase, Phase::Acting { .. }) {
                    next.stopping = Some(end.clone());
                } else {
                    next.phase = Phase::Finished(end.clone());
                }
            }
        }
        Input::Retry if matches!(state.phase, Phase::Failed(_)) => next.phase = Phase::Ready,
        Input::InstallContext(context) if state.phase == Phase::Ready => {
            if context.derived_from.as_ref() != Some(&state.context.at)
                || context.at.lineage == state.context.at.lineage
            {
                return Err("replacement must derive a new lineage from the current revision");
            }
            context.validate_ready()?;
            next.context = context.clone();
        }
        Input::Generated { .. } => applied = false,
        _ => return Err("input is not valid in this phase"),
    }
    Ok(Transition {
        state: next,
        effects,
        applied,
    })
}

fn settle(state: &mut State) {
    let Phase::Acting { generation, calls } = &state.phase else {
        return;
    };
    if calls.iter().any(|c| c.outcome.is_none()) {
        return;
    }
    let messages = calls
        .iter()
        .enumerate()
        .map(|(index, call)| Message::Tool {
            generation: generation.clone(),
            index,
            outcome: call.outcome.clone().unwrap(),
        })
        .collect::<Vec<_>>();
    state.context = state.context.append(messages);
    if !state.inbox.is_empty() {
        state.context = state.context.append(std::mem::take(&mut state.inbox));
    }
    state.phase = state
        .stopping
        .take()
        .map(Phase::Finished)
        .unwrap_or(Phase::Ready);
}
