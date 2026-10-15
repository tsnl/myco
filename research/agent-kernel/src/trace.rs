use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::*;
use crate::run::*;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub input: Input,
    pub effects: Vec<Effect>,
    pub applied: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub version: u32,
    pub initial: State,
    pub frames: Vec<Frame>,
}

impl Trace {
    pub fn new(initial: State) -> Self {
        Self {
            version: VERSION,
            initial,
            frames: vec![],
        }
    }

    /// Recompute decisions without performing effects. Array position is the cursor.
    pub fn replay(&self) -> Result<State, &'static str> {
        if self.version != VERSION {
            return Err("unsupported trace/controller version");
        }
        let mut state = self.initial.clone();
        for frame in &self.frames {
            let step = advance(&state, &frame.input)?;
            if step.effects != frame.effects || step.applied != frame.applied {
                return Err("recorded effects disagree with controller replay");
            }
            state = step.state;
        }
        Ok(state)
    }

    /// In-memory commit seam. A persistent runner makes this append durable before
    /// handing returned effects to workers, and enforces one writer per run.
    pub fn push(&mut self, input: Input) -> Result<Vec<Effect>, &'static str> {
        let step = advance(&self.replay()?, &input)?;
        self.frames.push(Frame {
            input,
            effects: step.effects.clone(),
            applied: step.applied,
        });
        Ok(step.effects)
    }

    pub fn samples(&self) -> Result<Vec<Sample>, &'static str> {
        Ok(self
            .generations()?
            .iter()
            .filter_map(GenerationRecord::candidate)
            .collect())
    }

    pub fn generations(&self) -> Result<Vec<GenerationRecord>, &'static str> {
        self.replay()?;
        let mut records: BTreeMap<OperationId, GenerationRecord> = BTreeMap::new();
        for frame in &self.frames {
            for effect in &frame.effects {
                if let Effect::Generate { id, input } = effect {
                    records.insert(
                        id.clone(),
                        GenerationRecord {
                            generation: id.clone(),
                            input: input.clone(),
                            result: None,
                            applied: false,
                        },
                    );
                }
            }
            if let Input::Generated { id, result } = &frame.input
                && let Some(record) = records.get_mut(id)
                && record.result.is_none()
            {
                record.result = Some(result.clone());
                record.applied = frame.applied;
            }
        }
        Ok(records.into_values().collect())
    }
}
