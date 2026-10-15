//! Keep explicit nohup invocations out of shell command fields. This is a
//! misuse guard, not containment of arbitrary programs or their stdin.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, Parser};

const REJECTED: &str = "nohup is not supported by the bash tool: it bypasses managed job visibility. \
    Remove nohup and shell backgrounding, then run the program with background_after (milliseconds), \
    or use action=start with a session_id. Use list/read/signal/close on that host to manage it.";

//
// Shell syntax inspection
//

pub(super) fn validate(command: &str) -> Result<(), String> {
    inspect(command, 0)
}

fn inspect(command: &str, depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("Shell command nesting is too deep to inspect; simplify the command.".into());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|error| format!("Load bash command parser: {error}"))?;
    let started = Instant::now();
    let mut budget = |_: &tree_sitter::ParseState| {
        if started.elapsed() > Duration::from_millis(500) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| command.as_bytes().get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut budget)),
        )
        .ok_or("Bash command inspection timed out; simplify the command.")?;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "command" {
            inspect_command(node, command, depth)?;
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

fn inspect_command(node: Node<'_>, source: &str, depth: usize) -> Result<(), String> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let mut cursor = node.walk();
    let mut words: Vec<Option<String>> = Vec::new();
    let mut previous_end = None;
    for word in std::iter::once(name).chain(node.children_by_field_name("argument", &mut cursor)) {
        let value = literal(word, source);
        // The grammar can split a word at a line continuation; Bash joins it
        // before tokenization. Preserve whitespace and quoted literal newlines.
        let joined = previous_end.is_some_and(|end| {
            source
                .get(end..word.start_byte())
                .is_some_and(|gap| gap.contains("\\\n") && gap.replace("\\\n", "").is_empty())
        });
        if joined {
            let previous = words.last_mut().expect("previous shell word");
            *previous = previous.take().zip(value).map(|(a, b)| a + &b);
        } else {
            words.push(value);
        }
        previous_end = Some(word.end_byte());
    }
    inspect_words(&words, depth)
}

//
// Literal commands and common launch wrappers
//

fn inspect_words(mut words: &[Option<String>], depth: usize) -> Result<(), String> {
    while let Some((Some(name), arguments)) = words.split_first() {
        let program = name.rsplit('/').next().unwrap_or(name);
        match program {
            "nohup" => return Err(REJECTED.into()),
            "command" | "exec" | "env" | "nice" | "time" | "timeout" | "sudo" => {
                let Some(command) = wrapped_command(program, arguments) else {
                    return Ok(());
                };
                words = if program == "timeout" {
                    command.get(1..).unwrap_or_default()
                } else {
                    command
                };
            }
            "busybox" | "toybox" => words = arguments,
            "bash" | "sh" | "dash" | "zsh" | "ksh" => {
                return inspect_shell(arguments, depth);
            }
            "eval" => {
                if let Some(parts) = arguments.iter().cloned().collect::<Option<Vec<_>>>() {
                    inspect(&parts.join(" "), depth + 1)?;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

fn wrapped_command<'a>(
    program: &str,
    mut words: &'a [Option<String>],
) -> Option<&'a [Option<String>]> {
    while let Some(Some(word)) = words.first() {
        if program == "command"
            && word.starts_with('-')
            && word.chars().any(|c| matches!(c, 'v' | 'V'))
        {
            return None;
        }
        if word == "--" {
            return Some(&words[1..]);
        }
        if program == "env" && word.contains('=') && !word.starts_with('-') {
            words = &words[1..];
        } else if word.starts_with('-') {
            words = words.get(1 + usize::from(option_has_value(program, word))..)?;
        } else {
            return Some(words);
        }
    }
    None
}

fn option_has_value(program: &str, option: &str) -> bool {
    match program {
        "exec" => option == "-a",
        "env" => matches!(
            option,
            "-u" | "--unset" | "-C" | "--chdir" | "-a" | "--argv0"
        ),
        "nice" => matches!(option, "-n" | "--adjustment"),
        "time" => matches!(option, "-f" | "--format" | "-o" | "--output"),
        "timeout" => matches!(option, "-k" | "--kill-after" | "-s" | "--signal"),
        "sudo" => matches!(
            option,
            "-u" | "--user"
                | "-g"
                | "--group"
                | "-h"
                | "--host"
                | "-p"
                | "--prompt"
                | "-C"
                | "--close-from"
                | "-T"
                | "--command-timeout"
                | "-R"
                | "--chroot"
                | "-D"
                | "--chdir"
        ),
        _ => false,
    }
}

fn inspect_shell(mut arguments: &[Option<String>], depth: usize) -> Result<(), String> {
    while let Some((Some(option), rest)) = arguments.split_first() {
        if !option.starts_with('-') || option == "--" {
            break;
        }
        if !option.starts_with("--") && option.contains('c') {
            if let Some(Some(command)) = rest.first() {
                inspect(command, depth + 1)?;
            }
            break;
        }
        arguments = if matches!(option.as_str(), "-o" | "-O" | "--rcfile" | "--init-file") {
            rest.get(1..).unwrap_or_default()
        } else {
            rest
        };
    }
    Ok(())
}

//
// Quote removal without expansion or execution
//

fn literal(node: Node<'_>, source: &str) -> Option<String> {
    let text = node.utf8_text(source.as_bytes()).ok()?;
    match node.kind() {
        "word" => Some(unescape(text, false)),
        "string_content" => Some(unescape(text, true)),
        "raw_string" => Some(text.strip_prefix('\'')?.strip_suffix('\'')?.into()),
        "ansi_c_string" => ansi_string(text.strip_prefix("$'")?.strip_suffix('\'')?),
        "command_name" | "concatenation" | "string" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .map(|part| literal(part, source))
                .collect::<Option<String>>()
        }
        _ => None,
    }
}

fn unescape(text: &str, quoted: bool) -> String {
    let mut output = String::new();
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
        } else if let Some(next) = chars.next() {
            if quoted && !matches!(next, '$' | '`' | '\\' | '\n' | '"') {
                output.push('\\');
            }
            if next != '\n' {
                output.push(next);
            }
        }
    }
    output
}

fn ansi_string(text: &str) -> Option<String> {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let next = chars.next()?;
        let (radix, limit) = match next {
            'x' => (16, 2),
            'u' => (16, 4),
            'U' => (16, 8),
            '0'..='7' => (8, 3),
            '\\' | '\'' | '"' => {
                output.push(next);
                continue;
            }
            _ => return None,
        };
        let mut digits = if radix == 8 {
            next.to_string()
        } else {
            String::new()
        };
        while digits.len() < limit && chars.peek().is_some_and(|c| c.is_digit(radix)) {
            digits.push(chars.next()?);
        }
        output.push(char::from_u32(u32::from_str_radix(&digits, radix).ok()?)?);
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::test_support::{result_text, temp_dir};
    use serde_json::json;

    #[test]
    fn explicit_nohup_invocations_are_rejected_in_shell_syntax() {
        for command in [
            "nohup sleep 30 >output 2>&1 &",
            "/usr/bin/nohup sleep 30",
            "cd /tmp && nohup sleep 30",
            "FOO=bar nohup sleep 30",
            "(nohup sleep 30)",
            "echo hi | nohup cat",
            "echo $(nohup true)",
            "echo `nohup true`",
            "cat <(nohup true)",
            "cat <<EOF\n$(nohup true)\nEOF",
            "if true; then nohup sleep 30; fi",
            "run() { nohup sleep 30; }; run",
            "'nohup' true",
            "n\"oh\"up true",
            "n\\ohup true",
            "no\\\nhup true",
            "$'nohup' true",
            "$'no\\x68up' true",
        ] {
            assert!(validate(command).is_err(), "accepted: {command}");
        }
    }

    #[test]
    fn wrappers_do_not_hide_explicit_nohup() {
        for command in [
            "command nohup true",
            "command -p -- /usr/bin/nohup true",
            "exec nohup true",
            "exec -a worker nohup true",
            "env FOO=bar nohup true",
            "env -u FOO -- nohup true",
            "env --chdir /tmp nohup true",
            "nice -n 10 nohup true",
            "time -p nohup true",
            "timeout 30s nohup true",
            "timeout -k 1s --signal TERM 30s nohup true",
            "timeout --kill-after=1s -- 30s /usr/bin/nohup true",
            "sudo -u worker nohup true",
            "busybox nohup true",
            "bash -lc 'nohup true'",
            "sh -c 'cd /tmp && nohup true'",
            "env FOO=bar bash -c 'command nohup true'",
            "eval 'nohup true'",
        ] {
            assert!(validate(command).is_err(), "accepted: {command}");
        }
    }

    #[test]
    fn data_and_queries_that_mention_nohup_remain_usable() {
        for command in [
            "printf '%s\\n' nohup",
            "echo 'nohup sleep 30 &'",
            "rg nohup README.md",
            "cat nohup.out",
            "# nohup is discouraged\nprintf done",
            "cat <<'EOF'\nnohup sleep 30 &\nEOF",
            "cat <<'EOF'\n$(nohup true)\nEOF",
            "cat <<EOF\nnohup sleep 30 &\nEOF",
            "bash -c 'echo nohup'",
            "python -c 'print(\"nohup\")'",
            "command -v nohup",
            "command -V /usr/bin/nohup",
            "command -pv nohup",
            "env -u nohup echo okay",
            "exec -a nohup echo okay",
            "timeout -k 1s 30s printf nohup",
            "sleep 1 & wait",
            "nohup\\\ntrue",
        ] {
            assert_eq!(validate(command), Ok(()), "rejected: {command}");
        }
    }

    async fn call(
        service: &Arc<BashService>,
        owner: Uuid,
        input: serde_json::Value,
    ) -> generative_model::ToolResult {
        service
            .clone()
            .dispatch_tool_use(
                generative_model::ToolUse {
                    name: "bash".into(),
                    input,
                },
                HostDispatchContext::new(owner, crate::core::CancelToken::new()),
            )
            .await
    }

    #[tokio::test]
    async fn rejection_precedes_all_exec_and_start_effects_and_allocates_no_handle() {
        let directory = temp_dir("nohup-rejected");
        let marker = directory.path().join("should-not-exist");
        let service = Arc::new(BashService::new());
        let owner = Uuid::new_v4();
        for action in ["exec", "start"] {
            let result = call(
                &service,
                owner,
                json!({
                    "action":action, "session_id":"blocked",
                    "command":format!("touch '{}'; nohup true", marker.display()),
                }),
            )
            .await;
            assert!(result.is_error, "{}", result_text(&result));
            assert!(result_text(&result).contains("background_after"));
            assert!(result_text(&result).contains("action=start"));
            assert!(
                !marker.exists(),
                "a rejected batch must never partially execute"
            );
            assert!(service.resources(owner).is_empty());
        }
    }

    #[tokio::test]
    async fn stdin_remains_program_data_instead_of_being_misclassified_as_shell() {
        let service = Arc::new(BashService::new());
        let owner = Uuid::new_v4();
        let start = call(
            &service,
            owner,
            json!({
                "action":"start", "session_id":"data", "command":"cat",
                "stdin":"nohup is just text\n", "idle_ms":10,
            }),
        )
        .await;
        let write = call(
            &service,
            owner,
            json!({
                "action":"write", "session_id":"data", "stdin":"nohup true\n", "idle_ms":10,
            }),
        )
        .await;
        call(
            &service,
            owner,
            json!({"action":"close", "session_id":"data"}),
        )
        .await;
        assert!(!start.is_error && !write.is_error);
        assert!(result_text(&start).contains("nohup is just text"));
        assert!(result_text(&write).contains("nohup true"));
    }
}
