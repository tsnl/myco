use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::agent::Agent;
use crate::chat::{CompactWorkerError, Compactor, SessionRunner, WorkflowEvent};
use crate::core::{
    Async, CancelToken, ModelInfo,
    image_store::{ImageStore, sha256, with_images},
};
use crate::external_command::GIT;
use crate::generative_model::{
    self, BackendConfig, CatalogModel, Content, Effort, GenerativeModelConfig, Message,
    TurnEndReason,
};
use crate::session::{ActiveSession, CompactOutcome, Session, Thread};
use crate::{Config, ConfigUserSettings, Harness, SessionRuntime};

use super::case::{Workspace, copy_tree, git};
use super::metrics::{Metrics, Recorder};
use super::{discover_cases, load_case, read_json, write_json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOptions {
    pub cases: PathBuf,
    pub config: PathBuf,
    pub models: Vec<String>,
    pub output: PathBuf,
    pub prelude: Option<PathBuf>,
    pub repeat: u32,
    pub jobs: usize,
    pub timeout_secs: u64,
    pub max_requests: u64,
    pub grader_timeout_secs: u64,
    pub free_only: bool,
    pub effort: Effort,
    pub split: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    case: PathBuf,
    config: PathBuf,
    model: String,
    model_hash: String,
    cohort_hash: String,
    prelude: String,
    output: PathBuf,
    timeout_secs: u64,
    max_requests: u64,
    free_only: bool,
    effort: Effort,
    case_hash: String,
    fingerprint: String,
    repetition: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentResult {
    status: String,
    error: Option<String>,
    answer: String,
    session_id: String,
    elapsed_ms: u128,
    metrics: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grade {
    score: f64,
    feedback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResultRecord {
    version: u32,
    fingerprint: String,
    case_hash: String,
    case: String,
    split: String,
    model: String,
    model_hash: String,
    cohort_hash: String,
    free_only: bool,
    repetition: u32,
    prelude_hash: String,
    myco_version: String,
    myco_commit: String,
    status: String,
    score: Option<f64>,
    feedback: String,
    agent: Option<AgentResult>,
}

fn config(path: &Path, model: &str) -> Result<(Config, CatalogModel), String> {
    let config = Config::resolve(ConfigUserSettings {
        config_path: Some(path.to_path_buf()),
        model: Some(model.into()),
        ..Default::default()
    })?;
    let model = config.models.get(model)?.clone();
    Ok((config, model))
}

fn enforce_free(model: &CatalogModel) -> Result<(), String> {
    let base_url = match &model.backend {
        BackendConfig::OpenAICompletions(config) | BackendConfig::OpenAIResponses(config) => {
            &config.base_url
        }
        _ => return Err("--free-only requires an explicit OpenRouter :free model".into()),
    };
    if !model.spec.api_id.ends_with(":free")
        || base_url.trim_end_matches('/') != "https://openrouter.ai/api/v1"
    {
        return Err("--free-only requires https://openrouter.ai/api/v1 and an explicit model ID ending in :free; routers and paid fallback IDs are excluded".into());
    }
    Ok(())
}

fn model_hash(model: &CatalogModel) -> String {
    let mut backend = model.backend.clone();
    match &mut backend {
        BackendConfig::Anthropic(c) => {
            c.anthropic_auth_token.clear();
            c.debug_dump_api_requests = false;
        }
        BackendConfig::OpenAIResponses(c) | BackendConfig::OpenAICompletions(c) => {
            c.auth_token.clear();
            c.debug_dump_api_requests = false;
        }
    }
    sha256(format!("{:?}|{:?}", model.spec, backend).as_bytes())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prices {
    input_per_million: f64,
    cached_input_per_million: f64,
    output_per_million: f64,
}
impl Prices {
    fn validate(&self) -> Result<(), String> {
        if [
            self.input_per_million,
            self.cached_input_per_million,
            self.output_per_million,
        ]
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err("prices must be finite nonnegative USD per million tokens".into());
        }
        Ok(())
    }
}
fn cost(result: &ResultRecord, prices: Option<&Prices>) -> Option<f64> {
    if result.free_only {
        return Some(0.0);
    }
    let usage = &result.agent.as_ref()?.metrics;
    let prices = prices?;
    if usage.requests_with_usage != usage.requests {
        return None;
    }
    Some(
        (usage.input_tokens.saturating_sub(usage.cached_input_tokens) as f64
            * prices.input_per_million
            + usage.cached_input_tokens as f64 * prices.cached_input_per_million
            + usage.output_tokens as f64 * prices.output_per_million)
            / 1_000_000.0,
    )
}

/// Fingerprints include input bytes and modes. Credentials are never serialized.
fn tree_hash(root: &Path) -> Result<String, String> {
    fn collect(root: &Path, dir: &Path, entries: &mut Vec<(String, String)>) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.file_name() == "__pycache__" {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                collect(root, &path, entries)?;
            } else if kind.is_file() {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry
                    .metadata()
                    .map_err(|error| error.to_string())?
                    .permissions()
                    .mode();
                entries.push((
                    format!("{}:{mode}", path.strip_prefix(root).unwrap().display()),
                    sha256(&std::fs::read(&path).map_err(|error| error.to_string())?),
                ));
            } else {
                return Err(format!(
                    "case contains unsupported entry {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
    let mut entries = vec![];
    collect(root, root, &mut entries)?;
    entries.sort();
    Ok(sha256(&serde_json::to_vec(&entries).unwrap()))
}

pub async fn run(mut options: RunOptions) -> Result<serde_json::Value, String> {
    if options.models.is_empty()
        || options.repeat == 0
        || options.jobs == 0
        || options.max_requests == 0
        || options.timeout_secs == 0
        || options.grader_timeout_secs == 0
    {
        return Err("models, repeat, jobs, request limit, and deadlines must be nonzero".into());
    }
    options.config = options
        .config
        .canonicalize()
        .map_err(|error| error.to_string())?;
    options.output = std::path::absolute(&options.output).map_err(|error| error.to_string())?;
    options.models.sort();
    options.models.dedup();
    let cases = discover_cases(&options.cases)?;
    let binary_hash = sha256(
        &std::fs::read(std::env::current_exe().map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
    );
    let cohort_hash = sha256(
        format!(
            "{:?}|{}|{}|{}|{}|{:?}|{}|{binary_hash}",
            cases
                .iter()
                .map(|path| tree_hash(path))
                .collect::<Result<Vec<_>, _>>()?,
            options.repeat,
            options.timeout_secs,
            options.max_requests,
            options.grader_timeout_secs,
            options.effort,
            options.free_only
        )
        .as_bytes(),
    );
    let prelude = options
        .prelude
        .as_ref()
        .map(|path| std::fs::read_to_string(path).map_err(|error| error.to_string()))
        .transpose()?
        .unwrap_or_default();
    let mut jobs = vec![];
    for case_path in cases {
        if options.output.starts_with(&case_path) {
            return Err("run output must be outside the frozen case directory".into());
        }
        let case = load_case(&case_path)?;
        if options
            .split
            .as_ref()
            .is_some_and(|split| *split != case.split)
        {
            continue;
        }
        let case_hash = tree_hash(&case_path)?;
        for model_key in &options.models {
            let (_, model) = config(&options.config, model_key)?;
            if options.free_only {
                enforce_free(&model)?;
            }
            let model_hash = model_hash(&model);
            for repetition in 0..options.repeat {
                let fingerprint = sha256(
                    format!(
                        "{cohort_hash}|{case_hash}|{model_key}|{model_hash}|{prelude}|{repetition}|{}|{}|{}|{:?}|{}|{}",
                        options.timeout_secs,
                        options.max_requests,
                        options.grader_timeout_secs,
                        options.effort,
                        crate::manual::VERSION,
                        crate::manual::GIT_COMMIT
                    )
                    .as_bytes(),
                );
                let output = options.output.join(&fingerprint[..24]);
                jobs.push(Job {
                    case: case_path.clone(),
                    config: options.config.clone(),
                    model: model_key.clone(),
                    model_hash: model_hash.clone(),
                    cohort_hash: cohort_hash.clone(),
                    prelude: prelude.clone(),
                    output,
                    timeout_secs: options.timeout_secs,
                    max_requests: options.max_requests,
                    free_only: options.free_only,
                    effort: options.effort,
                    case_hash: case_hash.clone(),
                    fingerprint,
                    repetition,
                });
            }
        }
    }
    if jobs.is_empty() {
        return Err("no cases match the selected split".into());
    }
    std::fs::create_dir_all(&options.output).map_err(|error| error.to_string())?;
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(options.output.join(".lock"))
        .map_err(|error| error.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another eval runner owns this output directory".into());
    }
    let results: Vec<_> = stream::iter(jobs)
        .map(|job| run_one(job, options.grader_timeout_secs))
        .buffer_unordered(options.jobs)
        .collect()
        .await;
    for result in results {
        result?;
    }
    report(&options.output, None)
}

async fn run_one(job: Job, grader_timeout: u64) -> Result<(), String> {
    if !job.output.join("result.json").exists()
        && let Ok(pid) = std::fs::read_to_string(job.output.join("worker.pid"))
        && let Ok(pid) = pid.trim().parse::<i32>()
        && pid > 0
        && unsafe { libc::kill(pid, 0) } == 0
    {
        return Err(format!(
            "previous worker {pid} is still running in {}; wait for its deadline before resuming",
            job.output.display()
        ));
    }
    if let Err(error) = run_one_inner(job.clone(), grader_timeout).await {
        if job.output.join("result.json").exists() {
            return Err(error);
        }
        std::fs::create_dir_all(&job.output).map_err(|error| error.to_string())?;
        let case = load_case(&job.case)?;
        let result = ResultRecord {
            version: 1,
            fingerprint: job.fingerprint,
            case_hash: job.case_hash,
            case: case.name,
            split: case.split,
            model: job.model,
            model_hash: job.model_hash,
            cohort_hash: job.cohort_hash,
            free_only: job.free_only,
            repetition: job.repetition,
            prelude_hash: sha256(job.prelude.as_bytes()),
            myco_version: crate::manual::VERSION.into(),
            myco_commit: crate::manual::GIT_COMMIT.into(),
            status: "setup_error".into(),
            score: None,
            feedback: error,
            agent: None,
        };
        write_json(&job.output.join("result.json"), &result)?;
    }
    Ok(())
}

async fn run_one_inner(mut job: Job, grader_timeout: u64) -> Result<(), String> {
    let result_path = job.output.join("result.json");
    if result_path.exists() {
        let result: ResultRecord = read_json(&result_path)?;
        if result.fingerprint != job.fingerprint {
            return Err("result fingerprint mismatch".into());
        }
        eprintln!("cached: {} repetition {}", job.model, job.repetition + 1);
        return Ok(());
    }
    // Existing unfinished attempts stay inspectable. Resume starts a fresh attempt;
    // it never replays an uncertain tool effect in an old process.
    if job.output.exists() {
        let previous = job.output.with_extension(format!(
            "interrupted-{}",
            crate::core::uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::rename(&job.output, previous).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir(&job.output).map_err(|error| error.to_string())?;
    let case = load_case(&job.case)?;
    eprintln!(
        "running: {} / {} repetition {}",
        case.name,
        job.model,
        job.repetition + 1
    );
    let frozen = job.output.join("case");
    copy_tree(&job.case, &frozen)?;
    if tree_hash(&frozen)? != job.case_hash {
        return Err("case changed while taking its snapshot".into());
    }
    job.case = frozen;
    let workspace = job.output.join("workspace");
    match &case.workspace {
        Workspace::Fixture => {
            copy_tree(&job.case.join("workspace"), &workspace)?;
            git(&workspace, &["init", "--quiet"])?;
        }
        Workspace::Git { repo, revision } => {
            let output = GIT
                .tokio_command()
                .args(["clone", "--shared", "--no-checkout", "--"])
                .arg(repo)
                .arg(&workspace)
                .output()
                .await
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "prepare git workspace: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            git(&workspace, &["checkout", "--detach", revision])?;
        }
    }
    write_json(&job.output.join("job.json"), &job)?;
    let stdout = std::fs::File::create(job.output.join("worker.stdout"))
        .map_err(|error| error.to_string())?;
    let stderr = std::fs::File::create(job.output.join("worker.stderr"))
        .map_err(|error| error.to_string())?;
    let mut child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("execute")
        .arg(job.output.join("job.json"))
        .current_dir(&workspace)
        .env("MYCO_HOME", job.output.join("home"))
        .env("MYCO_PROFILE", "eval")
        .env_remove("MYCO_CONFIG")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| error.to_string())?;
    std::fs::write(
        job.output.join("worker.pid"),
        child.id().unwrap().to_string(),
    )
    .map_err(|error| error.to_string())?;
    let status = match tokio::time::timeout(
        Duration::from_secs(job.timeout_secs.saturating_add(30)),
        child.wait(),
    )
    .await
    {
        Ok(status) => status.map_err(|error| error.to_string())?,
        Err(_) => {
            let _ = child.kill().await;
            return Err("worker exceeded its deadline and cancellation grace".into());
        }
    };
    std::fs::remove_file(job.output.join("worker.pid")).map_err(|error| error.to_string())?;
    let agent: Option<AgentResult> = read_json(&job.output.join("agent.json")).ok();
    let mut result = ResultRecord {
        version: 1,
        fingerprint: job.fingerprint.clone(),
        case_hash: job.case_hash.clone(),
        case: case.name,
        split: case.split,
        model: job.model,
        model_hash: job.model_hash,
        cohort_hash: job.cohort_hash,
        free_only: job.free_only,
        repetition: job.repetition,
        prelude_hash: sha256(job.prelude.as_bytes()),
        myco_version: crate::manual::VERSION.into(),
        myco_commit: crate::manual::GIT_COMMIT.into(),
        status: "worker_error".into(),
        score: None,
        feedback: String::new(),
        agent,
    };
    if !status.success() || result.agent.is_none() {
        result.feedback = "worker failed; inspect worker.stderr".into();
    } else if tree_hash(&job.case)? != job.case_hash {
        result.status = "case_modified".into();
        result.feedback = "frozen case or grader changed during execution".into();
    } else {
        let stdout = std::fs::File::create(job.output.join("grader.stdout"))
            .map_err(|error| error.to_string())?;
        let stderr = std::fs::File::create(job.output.join("grader.stderr"))
            .map_err(|error| error.to_string())?;
        let arguments: Vec<_> = case
            .grader
            .iter()
            .map(|arg| arg.replace("{case}", &job.case.to_string_lossy()))
            .collect();
        let mut child = Command::new(&arguments[0])
            .args(&arguments[1..])
            .current_dir(&workspace)
            .env("MYCO_EVAL_WORKSPACE", &workspace)
            .env("MYCO_EVAL_RUN", &job.output)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|error| format!("start grader: {error}"))?;
        let grade_status =
            tokio::time::timeout(Duration::from_secs(grader_timeout), child.wait()).await;
        if grade_status.is_err() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            result.status = "grader_error".into();
            result.feedback = "grader deadline exceeded".into();
        } else {
            let grade: Result<Grade, _> = read_json(&job.output.join("grader.stdout"));
            match (grade_status, grade) {
                (Ok(Ok(status)), Ok(grade))
                    if status.success()
                        && grade.score.is_finite()
                        && (0.0..=1.0).contains(&grade.score) =>
                {
                    result.status = result
                        .agent
                        .as_ref()
                        .map_or_else(|| "worker_error".into(), |agent| agent.status.clone());
                    result.score = Some(grade.score);
                    result.feedback = grade.feedback;
                }
                _ => {
                    result.status = "grader_error".into();
                    result.feedback = "grader must exit 0 and return JSON {score: 0..1, feedback: string}; inspect grader logs".into();
                }
            }
        }
        if tree_hash(&job.case)? != job.case_hash {
            result.status = "case_modified".into();
            result.score = None;
            result.feedback = "grader or frozen inputs changed".into();
        }
    }
    write_json(&result_path, &result)?;
    eprintln!(
        "finished: {} / {}: {} score={:?}",
        result.case, result.model, result.status, result.score
    );
    Ok(())
}

struct EvalCompactor {
    model: CatalogModel,
    recorder: Arc<Recorder>,
}
impl Compactor for EvalCompactor {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
        Box::pin(async move {
            crate::chat::run_compact_worker_with_model(&predecessor, &self.model, cancel, |model| {
                self.recorder.wrap(model, true)
            })
            .await
        })
    }
}

pub async fn execute_job(path: &Path) -> Result<(), String> {
    let job: Job = read_json(path)?;
    let (config, mut catalog) = config(&job.config, &job.model)?;
    if job.free_only {
        enforce_free(&catalog)?;
    }
    if model_hash(&catalog) != job.model_hash {
        return Err("model configuration changed after planning the run".into());
    }
    let home = crate::core::myco_home()?;
    let expected = job.output.join("home/profiles/eval");
    if home != expected
        || std::env::current_dir().map_err(|error| error.to_string())?
            != job.output.join("workspace")
    {
        return Err("execute requires the fresh profile and workspace prepared by run".into());
    }
    if job.prelude.len() > config.max_prelude_bytes {
        return Err("candidate prelude exceeds max_prelude_bytes".into());
    }
    std::fs::create_dir_all(home.join("workspace/prelude")).map_err(|error| error.to_string())?;
    std::fs::write(home.join("workspace/prelude/candidate.md"), &job.prelude)
        .map_err(|error| error.to_string())?;
    crate::manual::export(&home)?;
    let context: Vec<Message> = read_json(&job.case.join("context.json"))?;
    let source_store = ImageStore::new(job.case.join("images"));
    let image_store = ImageStore::for_profile()?;
    for part in context.iter().flat_map(Message::content) {
        if let Content::Image { source } = part {
            source_store.copy_reference(source, &image_store)?;
        }
    }
    let mut context = context;
    let Message::UserMessage { content: input } = context.pop().ok_or("empty eval context")? else {
        return Err("eval context must end at user input".into());
    };
    let mut session = Session::new(&job.model);
    session.replace_context(context, None);
    let session_id = session.id.clone();
    let session = ActiveSession::new(session);
    let harness = Harness::local_with_services(vec![
        Arc::new(crate::SessionMetaTool::new(session.clone())),
        Arc::new(crate::SessionHistoryTool::new()),
        Arc::new(crate::ListRecentService::new()),
        Arc::new(crate::PreludeTool::new(config.max_prelude_bytes)),
    ]);
    let runtime = SessionRuntime::new(harness.clone(), session);
    runtime.set_max_image_base64_bytes(catalog.spec.max_image_base64_bytes);
    match &mut catalog.backend {
        BackendConfig::Anthropic(c) => {
            c.effort = Some(job.effort);
            c.debug_dump_api_requests = false;
        }
        BackendConfig::OpenAIResponses(c) | BackendConfig::OpenAICompletions(c) => {
            c.effort = Some(job.effort);
            c.debug_dump_api_requests = false;
        }
    }
    let model = generative_model::new(GenerativeModelConfig {
        model: catalog.spec.clone(), tools: harness.tool_specs(), backend_config: catalog.backend.clone(),
        system_prompt: format!("You are a helpful assistant running in an agentic harness with unfettered computer access.\n{}\n{}\n{}\nThe current task workspace is {}. This is already an isolated task checkout; make changes directly here. Historical paths and tool handles are observations from an earlier run; use the current workspace. Only the local host is configured. Complete this task in this agent; nested model runs are not configured in this evaluation.", crate::prompts::agent_prompt_epilogue(), crate::prompts::model_stamp(&job.model), crate::prompts::auto_compact_notice(catalog.spec.auto_compact_at_tokens, catalog.spec.context_window_tokens), job.output.join("workspace").display()),
    }).map_err(|error| error.to_string())?;
    let recorder = Recorder::new(
        job.max_requests,
        std::fs::File::create(job.output.join("events.jsonl"))
            .map_err(|error| error.to_string())?,
    );
    let model = with_images(recorder.wrap(model, false), image_store);
    let mut agent = Agent::new(model.clone(), runtime.clone(), recorder.clone());
    agent.set_retry_policy(catalog.backend.retry_policy());
    agent.set_context_window_tokens(catalog.spec.context_window_tokens);
    agent.set_max_truncated_resumes(catalog.spec.max_truncated_resumes);
    let mut runner = SessionRunner::new(agent, runtime)
        .await
        .map_err(|error| error.to_string())?;
    runner
        .set_model(model, ModelInfo::from_spec(&catalog.spec, Some(job.effort)))
        .await
        .map_err(|error| error.to_string())?;
    runner.set_compactor(
        Arc::new(EvalCompactor {
            model: catalog.clone(),
            recorder: recorder.clone(),
        }),
        catalog.spec.auto_compact_at_tokens,
    );
    let observer = recorder.clone();
    runner.set_observer(Arc::new(move |event| match event {
        WorkflowEvent::Compacted(_) => observer.metrics.lock().unwrap().compactions += 1,
        WorkflowEvent::Warning(message) => {
            observer.event(serde_json::json!({"event":"warning", "message":message}))
        }
        _ => {}
    }));
    let cancel = CancelToken::new();
    let user_cancel = cancel.clone();
    let sigint = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            user_cancel.cancel();
        }
    });
    let started = Instant::now();
    let mut work = std::pin::pin!(runner.submit(input, chrono::Utc::now(), cancel.clone()));
    let (outcome, timed_out) = tokio::select! {
        biased;
        outcome = &mut work => (outcome, false),
        _ = tokio::time::sleep(Duration::from_secs(job.timeout_secs)) => { cancel.cancel(); (work.await, true) },
    };
    sigint.abort();
    let metrics = recorder.metrics.lock().unwrap().clone();
    let (status, error, answer) = match outcome.result {
        Ok(outcome) => (
            if outcome.reason == TurnEndReason::EndTurn {
                "completed"
            } else {
                "incomplete"
            },
            None,
            outcome
                .answer
                .iter()
                .filter_map(|part| match part {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Err(error) => ("agent_error", Some(error.to_string()), String::new()),
    };
    let status = if timed_out {
        "timeout"
    } else if metrics.request_limit_reached {
        "request_limit"
    } else if cancel.is_cancelled() {
        "cancelled"
    } else {
        status
    };
    let result = AgentResult {
        status: status.into(),
        error,
        answer,
        session_id,
        elapsed_ms: started.elapsed().as_millis(),
        metrics,
    };
    write_json(&job.output.join("agent.json"), &result)
}

pub fn report(output: &Path, prices: Option<&Path>) -> Result<serde_json::Value, String> {
    let prices: BTreeMap<String, Prices> = prices.map(read_json).transpose()?.unwrap_or_default();
    for price in prices.values() {
        price.validate()?;
    }
    let mut groups: BTreeMap<(String, String, String, String, String), Vec<ResultRecord>> =
        BTreeMap::new();
    for entry in std::fs::read_dir(output).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .file_name()
            .to_string_lossy()
            .contains(".interrupted-")
        {
            continue;
        }
        let path = entry.path().join("result.json");
        if !path.exists() {
            continue;
        }
        let result: ResultRecord = read_json(&path)?;
        groups
            .entry((
                result.cohort_hash.clone(),
                result.model_hash.clone(),
                result.model.clone(),
                result.prelude_hash.clone(),
                result.split.clone(),
            ))
            .or_default()
            .push(result);
    }
    let groups: Vec<_> = groups.into_iter().map(|((cohort, model_hash, model, prelude, split), results)| {
        let count = results.len();
        let successes = results.iter().filter(|result| result.status == "completed" && result.score == Some(1.0)).count();
        let infra = results.iter().filter(|result| ["setup_error", "worker_error", "grader_error", "case_modified"].contains(&result.status.as_str())).count();
        let metrics: Vec<_> = results.iter().filter_map(|result| result.agent.as_ref()).collect();
        serde_json::json!({
            "cohort_hash": cohort,
            "model_hash": model_hash,
            "model": model,
            "prelude_hash": prelude,
            "split": split,
            "runs": count,
            "successes": successes,
            "success_rate": successes as f64 / count as f64,
            "mean_score": results.iter().map(|result| result.score.unwrap_or(0.0)).sum::<f64>() / count as f64,
            "infrastructure_errors": infra,
            "requests": metrics.iter().map(|result| result.metrics.requests).sum::<u64>(),
            "compaction_requests": metrics.iter().map(|result| result.metrics.compaction_requests).sum::<u64>(),
            "compactions": metrics.iter().map(|result| result.metrics.compactions).sum::<u64>(),
            "tool_calls": metrics.iter().map(|result| result.metrics.tool_calls).sum::<u64>(),
            "tool_errors": metrics.iter().map(|result| result.metrics.tool_errors).sum::<u64>(),
            "input_tokens": metrics.iter().map(|result| result.metrics.input_tokens).sum::<u64>(),
            "cached_input_tokens": metrics.iter().map(|result| result.metrics.cached_input_tokens).sum::<u64>(),
            "output_tokens": metrics.iter().map(|result| result.metrics.output_tokens).sum::<u64>(),
            "requests_without_usage": metrics.iter().map(|result| result.metrics.requests - result.metrics.requests_with_usage).sum::<u64>(),
            "elapsed_ms": metrics.iter().map(|result| result.elapsed_ms).sum::<u128>(),
            "estimated_cost_usd": results.iter().map(|result| cost(result, prices.get(&model))).collect::<Option<Vec<_>>>().map(|costs| costs.iter().sum::<f64>())
        })
    }).collect();
    Ok(serde_json::json!({"version":1, "groups":groups}))
}
