//! Routing advice belongs beside the configured host catalog, not inside bash.
//! Commands are read through the bash tool's own shell inspection, so advice
//! sees the same effective programs the tool's policy does.

use std::collections::HashMap;
use std::sync::Arc;

use super::HostController;
use crate::tool_services::bash_service::effective_commands;

//
// Routing guidance
//

pub(super) fn command_nudge(
    command: &str,
    hosts: &HashMap<String, Arc<HostController>>,
) -> Option<String> {
    effective_commands(command).iter().find_map(|words| {
        // A word that needs expansion cannot name a configured host, but it
        // still occupies its position (e.g. as an option's value).
        let words: Vec<&str> = words.iter().map(|w| w.as_deref().unwrap_or("")).collect();
        command_words(&words, hosts)
    })
}

fn command_words(words: &[&str], hosts: &HashMap<String, Arc<HostController>>) -> Option<String> {
    let (program, arguments) = words.split_first()?;
    let program = basename(program);
    let operands = operands(program, arguments);
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
// Option parsing
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
