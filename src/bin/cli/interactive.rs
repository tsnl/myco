//! A scrolling line editor over the same durable turns as the browser.

use std::sync::Arc;

use myco::CancelToken;
use myco::session::expand_image_attachments;
use rustyline::{DefaultEditor, error::ReadlineError};

use super::output::CliSink;
use super::{Args, Boot, boot, interrupted, report_outcome, report_session};

pub(crate) async fn run_interactive(args: Args) -> u8 {
    match chat(args).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("myco: {error}");
            1
        }
    }
}

async fn chat(args: Args) -> Result<(), String> {
    let sink = Arc::new(CliSink::new(true));
    let mut boot = boot(&args, sink.clone()).await?;
    let mut editor = DefaultEditor::new().map_err(|error| error.to_string())?;
    editor.bind_sequence(rustyline::KeyEvent::alt('\r'), rustyline::Cmd::Newline);
    eprintln!(
        "Myco · {} · {}",
        boot.catalog_model.spec.key,
        boot.session.id()
    );
    eprintln!("/help for commands · Alt-Enter for a newline · Ctrl-D to exit\n");
    let result = read_turns(&mut editor, &mut boot, &sink).await;
    report_session(&boot);
    result
}

async fn read_turns(
    editor: &mut DefaultEditor,
    boot: &mut Boot,
    sink: &CliSink,
) -> Result<(), String> {
    loop {
        let line = match editor.readline("you › ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(error) => return Err(format!("read prompt: {error}")),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        editor
            .add_history_entry(line)
            .map_err(|error| error.to_string())?;
        match line {
            "/quit" | "/exit" => return Ok(()),
            "/help" => help(),
            "/session" => eprintln!(
                "session={}\nmodel={}",
                boot.session.id(),
                boot.catalog_model.spec.key
            ),
            "/compact" => compact(boot).await,
            command if command.starts_with('/') && !command.contains(char::is_whitespace) => {
                eprintln!("myco: unknown command {command}; use /help");
            }
            _ => turn(boot, sink, line).await?,
        }
    }
}

async fn turn(boot: &mut Boot, sink: &CliSink, prompt: &str) -> Result<(), String> {
    let input =
        match expand_image_attachments(prompt, boot.catalog_model.spec.max_image_base64_bytes) {
            Ok(input) => input,
            Err(error) => {
                eprintln!("myco: {error}");
                return Ok(());
            }
        };
    let cancel = CancelToken::new();
    sink.begin(cancel.clone());
    eprintln!("\n── assistant");
    let outcome = interrupted(
        cancel.clone(),
        boot.runner.submit(input, chrono::Utc::now(), cancel),
    )
    .await;
    sink.finish()?;
    if let Err((_, message)) = report_outcome(outcome) {
        eprintln!("myco: {message}");
    }
    eprintln!();
    Ok(())
}

async fn compact(boot: &mut Boot) {
    let cancel = CancelToken::new();
    match interrupted(cancel.clone(), boot.runner.compact(cancel)).await {
        Ok(outcome) => eprintln!("myco: compacted into thread {}", outcome.successor_id),
        Err(error) => eprintln!("myco: {error}"),
    }
}

fn help() {
    eprintln!(
        "Enter a prompt to start a turn. @./image.png attaches an image.\n\
        /compact  Summarize into a new thread in this session\n\
        /session  Show the session id and model\n\
        /quit     Exit (also /exit or Ctrl-D)\n\
        Ctrl-C cancels a running turn or clears the current input.\n\
        Resume later with myco --mode cli --resume ID; select a model with --model KEY."
    );
}
