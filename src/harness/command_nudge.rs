//! Routing advice belongs beside the configured host catalog, not inside bash.

use std::collections::HashMap;
use std::sync::Arc;

use super::HostController;

//
// Routing guidance
//

pub(super) fn command_nudge(
    command: &str,
    hosts: &HashMap<String, Arc<HostController>>,
) -> Option<String> {
    let tokens = shell_tokens(command);
    for words in tokens.split(|word| word == ";") {
        let words: Vec<_> = words.iter().map(String::as_str).collect();
        if let Some(message) = command_words(&words, hosts) {
            return Some(message);
        }
    }
    None
}

fn command_words(words: &[&str], hosts: &HashMap<String, Arc<HostController>>) -> Option<String> {
    let first = words.iter().position(|word| !word.contains('='))?;
    let program = basename(words[first]);
    let start = if matches!(
        program,
        "timeout" | "env" | "sudo" | "command" | "exec" | "nohup"
    ) {
        words
            .iter()
            .enumerate()
            .skip(first + 1)
            .find(|(_, word)| matches!(basename(word), "ssh" | "scp" | "rsync"))?
            .0
    } else {
        first
    };
    let program = basename(words[start]);
    let operands = operands(program, &words[start + 1..]);
    if program == "ssh" {
        let target = operands.first()?;
        let alias = target.rsplit('@').next()?;
        if configured(alias, hosts) {
            return Some(format!(
                "use bash's host={alias:?} field for this configured host. Connections retry lazily after failures; retry host= once before falling back to ssh. Direct SSH is useful for setup and diagnosis."
            ));
        }
    } else if matches!(program, "scp" | "rsync") {
        for target in operands {
            let Some((destination, _)) = target.split_once(':') else {
                continue;
            };
            let alias = destination.rsplit('@').next()?;
            if configured(alias, hosts) {
                return Some(format!(
                    "scp/rsync copies between hosts are fine. For commands on {alias:?}, use bash's host={alias:?} field."
                ));
            }
        }
    }
    None
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn configured(alias: &str, hosts: &HashMap<String, Arc<HostController>>) -> bool {
    alias != "local" && hosts.contains_key(alias)
}

//
// Shell parsing
//

fn operands<'a>(program: &str, words: &'a [&'a str]) -> Vec<&'a str> {
    let mut result = Vec::new();
    let mut skip = false;
    let mut options = true;
    for word in words {
        if skip {
            skip = false;
            continue;
        }
        if options && *word == "--" {
            options = false;
            continue;
        }
        if options && word.starts_with('-') {
            skip = match program {
                "ssh" => matches!(
                    *word,
                    "-B" | "-b"
                        | "-c"
                        | "-D"
                        | "-E"
                        | "-e"
                        | "-F"
                        | "-I"
                        | "-i"
                        | "-J"
                        | "-L"
                        | "-l"
                        | "-m"
                        | "-O"
                        | "-o"
                        | "-P"
                        | "-p"
                        | "-Q"
                        | "-R"
                        | "-S"
                        | "-W"
                        | "-w"
                ),
                "scp" => matches!(
                    *word,
                    "-c" | "-D" | "-F" | "-i" | "-J" | "-l" | "-o" | "-P" | "-S" | "-X"
                ),
                "rsync" => matches!(
                    *word,
                    "-e" | "--rsh"
                        | "--exclude"
                        | "--include"
                        | "--filter"
                        | "--files-from"
                        | "--log-file"
                ),
                _ => false,
            };
            continue;
        }
        result.push(*word);
        if program == "ssh" {
            break;
        }
    }
    result
}

/// A deliberately small lexer: quoted strings stay one word, shell control
/// operators start a new command, and comments do not supply executable words.
/// This is advisory only; it never changes or authorizes shell execution.
fn shell_tokens(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' && quote != Some('\'') {
            if let Some(next) = chars.next() {
                word.push(next);
            }
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_some() {
            word.push(ch);
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == '#' && word.is_empty() {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            words.push(";".into());
        } else if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')') {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if ch == '\n' || !ch.is_whitespace() {
                words.push(";".into());
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advice(command: &str) -> Option<String> {
        let host = HostController::local_in_process(5 * 1024 * 1024);
        command_nudge(command, &HashMap::from([("R".into(), host)]))
    }

    #[test]
    fn configured_ssh_targets_are_found_in_wrapped_and_chained_commands() {
        for command in [
            "ssh R pwd",
            "timeout 120 ssh R 'uname -a'",
            "cd x && ssh R pwd",
            "scp a R:b; ssh R pwd",
            "env FOO=x /usr/bin/ssh -o BatchMode=yes user@R pwd",
            "ssh -tt 'R'",
            "ssh -p 22 R pwd",
            "(ssh R pwd)",
        ] {
            assert!(advice(command).is_some(), "{command}");
        }
    }

    #[test]
    fn copies_are_acknowledged_without_treating_them_as_shell_routing() {
        for command in [
            "scp a R:b",
            "rsync -av ./ R:dir/",
            "rsync -e 'ssh -p 22' R:dir ./",
        ] {
            assert!(
                advice(command)
                    .unwrap()
                    .contains("copies between hosts are fine"),
                "{command}"
            );
        }
    }

    #[test]
    fn unconfigured_or_quoted_ssh_does_not_nudge() {
        for command in [
            "ssh unknown pwd",
            "ssh -V",
            "ssh-add -l",
            "echo ssh R",
            "echo 'ssh R'",
            "printf '%s' 'timeout 1 ssh R'",
            "# ssh R\necho ok",
        ] {
            assert!(advice(command).is_none(), "{command}");
        }
    }
}
