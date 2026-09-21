use clap::{Args, Parser, Subcommand};
use myco::eval::{CreateOptions, RunOptions};
use myco::generative_model::Effort;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Create private task evals from Myco sessions and run repeatable comparisons"
)]
struct Cli {
    /// Source session profile (create/from-session only).
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Freeze a new task, workspace recipe, and independent Python grader.
    Create {
        output: PathBuf,
        #[arg(long)]
        task_file: PathBuf,
        #[command(flatten)]
        case: CaseArgs,
    },
    /// Freeze a session prefix ending at the chosen human input; omit its answer.
    FromSession {
        session: String,
        output: PathBuf,
        #[arg(long)]
        user_message: usize,
        #[arg(long)]
        thread: Option<String>,
        #[command(flatten)]
        case: CaseArgs,
    },
    /// Run one case or a directory of cases; finished matching results are reused.
    Run {
        cases: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long, required = true, action = clap::ArgAction::Append)]
        model: Vec<String>,
        #[arg(long)]
        output: PathBuf,
        /// Entire candidate prelude; the user's global prelude is never inherited.
        #[arg(long)]
        prelude: Option<PathBuf>,
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        #[arg(long, default_value_t = 600)]
        timeout_secs: u64,
        #[arg(long, default_value_t = 40)]
        max_requests: u64,
        #[arg(long, default_value_t = 60)]
        grader_timeout_secs: u64,
        /// Require an explicit OpenRouter :free model; no paid/router fallback.
        #[arg(long)]
        free_only: bool,
        #[arg(long, default_value = "high", value_parser = parse_effort)]
        effort: Effort,
        #[arg(long, value_parser = ["train", "validation", "test"])]
        split: Option<String>,
    },
    /// Aggregate finished results as JSON. Optionally enforce a success-rate floor.
    Report {
        output: PathBuf,
        #[arg(long)]
        min_success_rate: Option<f64>,
        /// JSON map of model keys to input/cached_input/output_per_million USD.
        #[arg(long)]
        prices: Option<PathBuf>,
    },
    #[command(hide = true)]
    Execute { job: PathBuf },
}

#[derive(Args)]
struct CaseArgs {
    /// Copy a small fixture tree; excludes .git, .myco, target and __pycache__.
    #[arg(long, conflicts_with = "repo")]
    workspace: Option<PathBuf>,
    /// Existing local Git repository, checked out at --revision for each run.
    #[arg(long, requires = "revision", conflicts_with = "workspace")]
    repo: Option<PathBuf>,
    #[arg(long, requires = "repo")]
    revision: Option<String>,
    /// Python script returning JSON {"score": 0..1, "feedback": "..."}.
    #[arg(long)]
    grader: PathBuf,
    #[arg(long, default_value = "train", value_parser = ["train", "validation", "test"])]
    split: String,
}

fn parse_effort(value: &str) -> Result<Effort, String> {
    value.parse()
}

fn create_options(output: PathBuf, case: CaseArgs) -> CreateOptions {
    CreateOptions {
        output,
        task: None,
        session: None,
        thread: None,
        user_message: None,
        fixture: case.workspace,
        repo: case.repo,
        revision: case.revision,
        grader: case.grader,
        split: case.split,
    }
}

fn main() {
    let cli = Cli::parse();
    if let Some(profile) = &cli.profile {
        let profile = myco::core::validate_profile(profile).unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2)
        });
        // Before the runtime or any other thread reads profile selection.
        unsafe {
            std::env::set_var("MYCO_PROFILE", profile);
        }
    }
    let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
        match cli.command {
            Command::Create {
                output,
                task_file,
                case,
            } => {
                let mut options = create_options(output, case);
                options.task =
                    Some(std::fs::read_to_string(task_file).map_err(|error| error.to_string())?);
                let path = myco::eval::create_case(options)?;
                Ok(serde_json::json!({"case":path}))
            }
            Command::FromSession {
                session,
                output,
                user_message,
                thread,
                case,
            } => {
                let mut options = create_options(output, case);
                options.session = Some(session);
                options.thread = thread;
                options.user_message = Some(user_message);
                let path = myco::eval::create_case(options)?;
                Ok(serde_json::json!({"case":path}))
            }
            Command::Run {
                cases,
                config,
                model,
                output,
                prelude,
                repeat,
                jobs,
                timeout_secs,
                max_requests,
                grader_timeout_secs,
                free_only,
                effort,
                split,
            } => {
                myco::eval::run(RunOptions {
                    cases,
                    config,
                    models: model,
                    output,
                    prelude,
                    repeat,
                    jobs,
                    timeout_secs,
                    max_requests,
                    grader_timeout_secs,
                    free_only,
                    effort,
                    split,
                })
                .await
            }
            Command::Report {
                output,
                min_success_rate,
                prices,
            } => {
                let report = myco::eval::report(&output, prices.as_deref())?;
                if let Some(floor) = min_success_rate {
                    if !floor.is_finite() || !(0.0..=1.0).contains(&floor) {
                        return Err("success-rate floor must be 0..1".into());
                    }
                    let groups = report["groups"].as_array().unwrap();
                    if groups.is_empty()
                        || groups.iter().any(|group| {
                            group["success_rate"].as_f64().unwrap() < floor
                                || group["infrastructure_errors"].as_u64().unwrap() > 0
                        })
                    {
                        println!("{}", serde_json::to_string_pretty(&report).unwrap());
                        return Err(
                            "eval success-rate threshold failed or infrastructure errors occurred"
                                .into(),
                        );
                    }
                }
                Ok(report)
            }
            Command::Execute { job } => {
                myco::eval::execute_job(&job).await?;
                Ok(serde_json::json!({"finished":true}))
            }
        }
    });
    match result {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap()),
        Err(error) => {
            eprintln!("myco-eval: {error}");
            std::process::exit(1);
        }
    }
}
