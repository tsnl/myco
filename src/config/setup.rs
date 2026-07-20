//! First-run guided setup: build `~/.myco/config.toml` interactively.
//!
//! Myco ships no built-in models, so a fresh install has an empty catalog and
//! nothing to run. [`run_setup`] walks the user through picking a provider, an
//! auth source, and a default model, then writes a config file the normal
//! resolver ([`crate::config::Config`]) can load. The CLI calls it on first run
//! (empty catalog) and on `myco --setup`.
//!
//! [`render_config`] is the pure core (choices → TOML text) and is what the
//! tests exercise against the real parser; the driver only sequences prompts
//! and writes the file.

use std::path::{Path, PathBuf};

use crate::harness::load_file_config;
use crate::interaction::{Prompt, PromptError, Question};

// ---------------------------------------------------------------------------
// Provider presets
// ---------------------------------------------------------------------------

/// A ready-made gateway + a few of its models, mirroring the documented
/// example catalog (`myco --help overview`).
struct Provider {
    label: &'static str,
    /// `[gateways.NAME]` and each model's `gateway = "NAME"`.
    gateway: &'static str,
    protocol: &'static str,
    base_url: &'static str,
    /// Default environment variable holding the API key.
    env_var: &'static str,
    models: &'static [ModelPreset],
}

struct ModelPreset {
    /// `[models.KEY]` and what `--model` takes.
    key: &'static str,
    context_window: u64,
    /// Wire id when it differs from `key` (e.g. `moonshotai/kimi-k3`).
    api_id: Option<&'static str>,
    /// Non-default `thinking` mode (older models reject adaptive thinking).
    thinking: Option<&'static str>,
}

static PROVIDERS: &[Provider] = &[
    Provider {
        label: "Anthropic",
        gateway: "anthropic",
        protocol: "anthropic-messages",
        base_url: "https://api.anthropic.com",
        env_var: "ANTHROPIC_API_KEY",
        models: &[
            ModelPreset {
                key: "claude-opus-4-8",
                context_window: 1_000_000,
                api_id: None,
                thinking: None,
            },
            ModelPreset {
                key: "claude-haiku-4-5",
                context_window: 200_000,
                api_id: None,
                thinking: Some("budget"),
            },
        ],
    },
    Provider {
        label: "xAI",
        gateway: "xai",
        protocol: "openai-responses",
        base_url: "https://api.x.ai/v1",
        env_var: "XAI_API_KEY",
        models: &[ModelPreset {
            key: "grok-4.5-build",
            context_window: 500_000,
            api_id: None,
            thinking: None,
        }],
    },
    Provider {
        label: "OpenRouter",
        gateway: "openrouter",
        protocol: "openai-responses",
        base_url: "https://openrouter.ai/api/v1",
        env_var: "OPENROUTER_API_KEY",
        models: &[ModelPreset {
            key: "kimi-k3",
            context_window: 1_000_000,
            api_id: Some("moonshotai/kimi-k3"),
            thinking: None,
        }],
    },
];

/// How the config file should read the API key.
enum AuthChoice {
    /// `{ source = "env", var_name = "…" }`.
    Env(String),
    /// The literal token inline.
    Inline(String),
    /// `{ source = "file", path = "…" }`.
    File(String),
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// What the wizard did.
pub enum SetupOutcome {
    /// A config file was written at this path.
    Written(PathBuf),
    /// The user chose to keep an existing config untouched.
    Declined,
}

/// Interactively build and write a config file at `path`. Returns
/// [`SetupOutcome::Declined`] if a populated config already exists and the user
/// declines to replace it.
pub fn run_setup(prompt: &dyn Prompt, path: &Path) -> Result<SetupOutcome, String> {
    if path.exists()
        && let Ok(existing) = load_file_config(path)
        && !existing.models.is_empty()
    {
        let replace = Question::choose(
            format!(
                "{} already configures {} model(s). Replace it?",
                path.display(),
                existing.models.len()
            ),
            vec!["keep it".into(), "replace it".into()],
        )
        .with_default("keep it");
        if ask_index(prompt, &replace)? == 0 {
            return Ok(SetupOutcome::Declined);
        }
    }

    println!();
    println!("Let's set up your myco model catalog ({}).", path.display());
    println!("Re-run anytime with `myco --setup`, or edit the file by hand.");
    println!();

    let provider = choose_provider(prompt)?;
    let auth = choose_auth(prompt, provider)?;
    let default_model = choose_default_model(prompt, provider)?;

    let toml = render_config(provider, &auth, default_model);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, toml).map_err(|e| format!("write {}: {e}", path.display()))?;

    if let AuthChoice::Env(var) = &auth {
        println!();
        println!("Remember to export ${var} (a `.env` in the working directory also works).");
    }
    Ok(SetupOutcome::Written(path.to_path_buf()))
}

fn choose_provider(prompt: &dyn Prompt) -> Result<&'static Provider, String> {
    let labels: Vec<String> = PROVIDERS
        .iter()
        .map(|p| format!("{} ({})", p.label, p.env_var))
        .collect();
    let idx = ask_index(
        prompt,
        &Question::choose("Which model provider?", labels).with_default(PROVIDERS[0].label),
    )?;
    Ok(&PROVIDERS[idx])
}

fn choose_auth(prompt: &dyn Prompt, provider: &Provider) -> Result<AuthChoice, String> {
    let labels = vec![
        format!("read ${} from the environment", provider.env_var),
        "paste the API key now".into(),
        "read the key from a file".into(),
    ];
    let idx = ask_index(
        prompt,
        &Question::choose("How should myco read your API key?", labels)
            .with_default(format!("read ${} from the environment", provider.env_var)),
    )?;
    match idx {
        0 => Ok(AuthChoice::Env(provider.env_var.to_string())),
        1 => {
            let token = prompt
                .ask(&Question::free("Paste your API key:"))
                .map_err(setup_err)?;
            let token = token.trim();
            if token.is_empty() {
                return Err("no API key entered".into());
            }
            if token.contains('"') {
                return Err("API key contains a quote; use the env or file option instead".into());
            }
            Ok(AuthChoice::Inline(token.to_string()))
        }
        _ => {
            let file = prompt
                .ask(&Question::free(
                    "Path to the key file (e.g. ~/.secrets/key):",
                ))
                .map_err(setup_err)?;
            let file = file.trim();
            if file.is_empty() {
                return Err("no path entered".into());
            }
            Ok(AuthChoice::File(file.to_string()))
        }
    }
}

fn choose_default_model(prompt: &dyn Prompt, provider: &Provider) -> Result<&'static str, String> {
    if provider.models.len() == 1 {
        return Ok(provider.models[0].key);
    }
    let labels: Vec<String> = provider.models.iter().map(|m| m.key.to_string()).collect();
    let idx = ask_index(
        prompt,
        &Question::choose("Which model should be the default?", labels)
            .with_default(provider.models[0].key),
    )?;
    Ok(provider.models[idx].key)
}

/// Ask a menu question and return the chosen option's index, re-asking until
/// the reply names a listed option.
fn ask_index(prompt: &dyn Prompt, question: &Question) -> Result<usize, String> {
    loop {
        let answer = prompt.ask(question).map_err(setup_err)?;
        if let Some(idx) = question.options.iter().position(|o| *o == answer) {
            return Ok(idx);
        }
        println!(
            "Please choose a number from 1 to {}.",
            question.options.len()
        );
    }
}

fn setup_err(e: PromptError) -> String {
    match e {
        PromptError::NotInteractive => "setup needs an interactive terminal".into(),
        PromptError::Cancelled => "setup cancelled".into(),
        PromptError::Io(m) => format!("input error: {m}"),
    }
}

// ---------------------------------------------------------------------------
// Rendering (pure)
// ---------------------------------------------------------------------------

/// Render a full `config.toml` for one provider, its models, and the chosen
/// default. Top-level keys precede the tables, as TOML requires.
fn render_config(provider: &Provider, auth: &AuthChoice, default_model: &str) -> String {
    let mut out = String::new();
    out.push_str("# Myco config (~/.myco/config.toml) — written by `myco --setup`.\n");
    out.push_str("# See `myco --help overview` for every option.\n\n");
    out.push_str(&format!("model = {}\n", quote(default_model)));
    out.push_str("enable_subagent = true\n");
    out.push_str("attach_timeout_secs = 10\n\n");

    out.push_str(&format!("[gateways.{}]\n", provider.gateway));
    out.push_str(&format!("protocol = {}\n", quote(provider.protocol)));
    out.push_str(&format!("base_url = {}\n", quote(provider.base_url)));
    out.push_str(&auth_line(auth));
    out.push('\n');

    for model in provider.models {
        out.push('\n');
        out.push_str(&format!("[models.{}]\n", table_key(model.key)));
        out.push_str(&format!("gateway = {}\n", quote(provider.gateway)));
        out.push_str(&format!("context_window = {}\n", model.context_window));
        if let Some(api_id) = model.api_id {
            out.push_str(&format!("api_id = {}\n", quote(api_id)));
        }
        if let Some(thinking) = model.thinking {
            out.push_str(&format!("thinking = {}\n", quote(thinking)));
        }
    }
    out
}

/// The `auth = …` line for the chosen key source.
fn auth_line(auth: &AuthChoice) -> String {
    match auth {
        AuthChoice::Env(var) => {
            format!("auth = {{ source = \"env\", var_name = {} }}", quote(var))
        }
        AuthChoice::File(path) => {
            format!("auth = {{ source = \"file\", path = {} }}", quote(path))
        }
        AuthChoice::Inline(token) => format!("auth = {}", quote(token)),
    }
}

/// A TOML basic string. Callers restrict inputs to values without control
/// characters, so escaping quotes and backslashes is sufficient.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A model table key: bare when it is a bare-key (`A-Za-z0-9_-`), otherwise a
/// quoted key so dotted names like `grok-4.5-build` stay one key.
fn table_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        key.to_string()
    } else {
        quote(key)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::parse_file_config_str;
    use crate::interaction::ScriptedPrompt;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "myco-setup-{tag}-{}",
            crate::session::uuid_simple_hex(uuid::Uuid::new_v4())
        ))
    }

    #[test]
    fn every_preset_renders_parseable_toml() {
        for provider in PROVIDERS {
            let toml = render_config(
                provider,
                &AuthChoice::Env(provider.env_var.to_string()),
                provider.models[0].key,
            );
            let parsed = parse_file_config_str(&toml)
                .unwrap_or_else(|e| panic!("{} config did not parse: {e}\n{toml}", provider.label));
            assert_eq!(parsed.model.as_deref(), Some(provider.models[0].key));
            assert!(parsed.gateways.contains_key(provider.gateway));
            for model in provider.models {
                assert!(
                    parsed.models.contains_key(model.key),
                    "{} missing model {}",
                    provider.label,
                    model.key
                );
            }
        }
    }

    #[test]
    fn inline_and_file_auth_render_parseable() {
        let provider = &PROVIDERS[0];
        for auth in [
            AuthChoice::Inline("sk-test-123".into()),
            AuthChoice::File("~/.secrets/key".into()),
        ] {
            let toml = render_config(provider, &auth, provider.models[0].key);
            parse_file_config_str(&toml).expect("auth variant parses");
        }
    }

    #[test]
    fn wizard_writes_selected_config() {
        // provider #2 (xAI), env auth (#1); single model so no model prompt.
        let prompt = ScriptedPrompt::new(["2", "1"]);
        let path = temp_path("xai");
        let outcome = run_setup(&prompt, &path).expect("setup ran");
        assert!(matches!(outcome, SetupOutcome::Written(_)));

        let file = load_file_config(&path).expect("written config loads");
        assert_eq!(file.model.as_deref(), Some("grok-4.5-build"));
        assert!(file.models.contains_key("grok-4.5-build"));
        assert!(file.gateways.contains_key("xai"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wizard_picks_non_default_model() {
        // Anthropic (#1), env auth (#1), second model (#2 → claude-haiku-4-5).
        let prompt = ScriptedPrompt::new(["1", "1", "2"]);
        let path = temp_path("anthropic");
        run_setup(&prompt, &path).expect("setup ran");
        let file = load_file_config(&path).expect("config loads");
        assert_eq!(file.model.as_deref(), Some("claude-haiku-4-5"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn declines_to_overwrite_populated_config() {
        let path = temp_path("keep");
        // Seed a config that already has a model.
        std::fs::write(
            &path,
            render_config(
                &PROVIDERS[0],
                &AuthChoice::Env("ANTHROPIC_API_KEY".into()),
                "claude-opus-4-8",
            ),
        )
        .unwrap();
        // Answer the replace prompt with "keep it" (option 1).
        let prompt = ScriptedPrompt::new(["1"]);
        let outcome = run_setup(&prompt, &path).expect("setup ran");
        assert!(matches!(outcome, SetupOutcome::Declined));
        let _ = std::fs::remove_file(&path);
    }
}
