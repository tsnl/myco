//! The agent's model for a catalog entry, and the runner settings that must
//! change with it. Boot, model selection and evaluation share both, so a
//! runner never keeps one model's limits while running another.

use std::sync::Arc;

use crate::agent::AgentInteractionError;
use crate::core::ModelInfo;
use crate::core::image_store::{ImageStore, with_images};
use crate::generative_model::{self, CatalogModel, Effort, GenerativeModel, GenerativeModelConfig};
use crate::harness::Harness;
use crate::prelude::PreludeEntry;
use crate::prompts;
use crate::skills::Reported;

use super::{Compactor, SessionRunner};

pub struct AgentModel {
    /// The catalog entry with the selected effort applied to its backend.
    pub catalog: CatalogModel,
    pub effort: Effort,
    /// The prelude the system prompt was built from; later edits arrive as
    /// notices measured against it.
    prelude: Vec<PreludeEntry>,
    /// The skills the system prompt listed; a thread's local skill notices
    /// start from them.
    skills: Reported,
}

impl AgentModel {
    /// The system prompt joins `prologue`, the agent epilogue with its
    /// prelude, the model stamp, the auto-compaction notice, and `suffix`, one
    /// per line. Tools are the harness's at the model's image limit. The
    /// returned configuration creates the model with [`Self::build`].
    pub fn new(
        catalog: &CatalogModel,
        harness: &Harness,
        effort: Effort,
        debug_dump_api_requests: bool,
        prologue: &str,
        suffix: Option<&str>,
    ) -> (Self, GenerativeModelConfig) {
        let mut catalog = catalog.clone();
        catalog.backend.effort = effort;
        catalog.backend.debug_dump_api_requests = debug_dump_api_requests;
        let spec = &catalog.spec;
        let (epilogue, prelude, skills) = prompts::agent_prompt_epilogue();
        let mut prompt = vec![
            prologue.to_string(),
            epilogue,
            prompts::model_stamp(&spec.key),
            prompts::auto_compact_notice(spec.auto_compact_at_tokens, spec.context_window_tokens),
        ];
        prompt.extend(suffix.map(str::to_string));
        let config = GenerativeModelConfig {
            model: spec.clone(),
            tools: harness.tool_specs_with_image_limit(spec.max_image_base64_bytes),
            system_prompt: prompt.join("\n"),
            backend_config: catalog.backend.clone(),
        };
        let selected = Self {
            catalog,
            effort,
            prelude,
            skills,
        };
        (selected, config)
    }

    /// Create the model. `wrap` instruments provider requests before stored
    /// image references are resolved.
    pub fn build(
        &self,
        config: GenerativeModelConfig,
        wrap: impl FnOnce(Arc<dyn GenerativeModel>) -> Arc<dyn GenerativeModel>,
    ) -> Result<Arc<dyn GenerativeModel>, String> {
        let model = generative_model::new(config)
            .map_err(|error| format!("could not create model: {error}"))?;
        Ok(with_images(
            wrap(model),
            ImageStore::for_profile()?,
            self.catalog.spec.max_image_base64_bytes,
        ))
    }
}

impl SessionRunner {
    /// Switch to `model`, built from `selected`, then apply every setting that
    /// follows the model: prelude tracking, retry policy, truncation cap,
    /// compaction, the image limit and the prompt's skill catalog. A failed switch leaves
    /// the previous model and its settings in place.
    pub async fn select_model(
        &mut self,
        model: Arc<dyn GenerativeModel>,
        selected: AgentModel,
        compactor: Arc<dyn Compactor>,
    ) -> Result<(), AgentInteractionError> {
        let spec = &selected.catalog.spec;
        self.set_model(model, ModelInfo::from_spec(spec, Some(selected.effort)))
            .await?;
        let agent = self.agent_mut();
        agent.set_before_generation_notice(Some(crate::session_runtime::prelude_change_notices(
            selected.prelude,
        )));
        agent.set_retry_policy(selected.catalog.backend.retry);
        agent.set_max_truncated_resumes(spec.max_truncated_resumes);
        self.set_compactor(compactor, spec.auto_compact_at_tokens);
        self.runtime()
            .set_max_image_base64_bytes(spec.max_image_base64_bytes);
        self.runtime().set_prompt_skills(selected.skills);
        Ok(())
    }
}
