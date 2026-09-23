//! One-shot prompts keep stdout suitable for pipes.

use std::io::{IsTerminal, Read};
use std::sync::Arc;

use myco::CancelToken;
use myco::generative_model::Content;

use super::output::CliSink;
use super::{Args, boot, interrupted, report_outcome, report_session};

pub(crate) async fn run_print(args: Args) -> u8 {
    match print_turn(args).await {
        Ok(()) => 0,
        Err((code, message)) => {
            eprintln!("myco: {message}");
            code
        }
    }
}

async fn print_turn(args: Args) -> Result<(), (u8, String)> {
    let argument = args.print.as_ref().and_then(Option::as_deref);
    let prompt = read_prompt(argument).map_err(|error| (2, error))?;
    let cancel = CancelToken::new();
    let sink = Arc::new(CliSink::new(false));
    sink.begin(cancel.clone());
    let mut boot = boot(&args, sink.clone())
        .await
        .map_err(|error| (1, error))?;
    let input = print_content(
        argument,
        prompt,
        boot.catalog_model.spec.max_image_base64_bytes,
    )
    .map_err(|error| (2, error))?;
    let outcome = interrupted(
        cancel.clone(),
        boot.runner.submit(input, chrono::Utc::now(), cancel),
    )
    .await;
    let output = sink.finish();
    report_session(&boot);
    output.map_err(|error| (1, error))?;
    report_outcome(outcome)
}

fn read_prompt(argument: Option<&str>) -> Result<String, String> {
    let mut piped = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin()
            .read_to_string(&mut piped)
            .map_err(|error| format!("read prompt from stdin: {error}"))?;
    }
    assemble_prompt(argument, &piped)
}

fn assemble_prompt(argument: Option<&str>, piped: &str) -> Result<String, String> {
    match (
        argument.filter(|text| !text.trim().is_empty()),
        piped.trim().is_empty(),
    ) {
        (Some(argument), false) => Ok(format!("{}\n\n{argument}", piped.trim_end())),
        (Some(argument), true) => Ok(argument.into()),
        (None, false) => Ok(piped.into()),
        (None, true) => Err("print mode needs a prompt: myco -p \"…\" or pipe stdin".into()),
    }
}

fn print_content(
    argument: Option<&str>,
    prompt: String,
    limit: u64,
) -> Result<Vec<Content>, String> {
    // Piped text is data: a diff mentioning @file.png must not open that file.
    let mut content = myco::session::expand_image_attachments(argument.unwrap_or(""), limit)?;
    content.retain(|part| matches!(part, Content::Image { .. }));
    content.push(Content::Text { text: prompt });
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_text_precedes_the_instruction_and_is_not_expanded_as_an_attachment() {
        let prompt = assemble_prompt(Some("review"), "diff mentions @missing.png\n").unwrap();
        assert_eq!(prompt, "diff mentions @missing.png\n\nreview");
        let content = print_content(Some("review"), prompt.clone(), 1024).unwrap();
        assert_eq!(content, vec![Content::Text { text: prompt }]);
        assert!(assemble_prompt(None, " \n").is_err());
        assert_eq!(assemble_prompt(None, "piped\n").unwrap(), "piped\n");
        assert_eq!(assemble_prompt(Some("task"), "").unwrap(), "task");
    }
}
