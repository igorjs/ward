// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Podman's `-l/--latest`: the newest sandbox stands in for the name. The
//! flag is replaced by that sandbox's ID before the arguments are parsed, so
//! each command keeps its own definition.

use anyhow::{Result, bail};

use crate::{client, ps_filter, socket, subcommand_index};

const LATEST_FLAGS: [&str; 2] = ["-l", "--latest"];
const SOCKET_FLAG: &str = "--socket";
const END_OF_OPTIONS: &str = "--";
const TIME_OPTIONS: [&str; 3] = ["-t", "--time", "--timeout"];

/// The commands that take the flag and the options of each that read a value,
/// which tells a value apart from the name the flag replaces.
const COMMANDS: [(&str, &[&str]); 14] = [
    ("logs", &["-n", "--tail", "--since", "--until"]),
    ("inspect", &["-f", "--format", "--type"]),
    ("stop", &TIME_OPTIONS),
    ("start", &[]),
    ("restart", &TIME_OPTIONS),
    ("rm", &[]),
    ("remove", &[]),
    ("kill", &["-s", "--signal", "--pid"]),
    ("top", &[]),
    ("wait", &[]),
    ("port", &[]),
    ("diff", &[]),
    ("stats", &["--format"]),
    (
        "exec",
        &["-w", "--workdir", "-u", "--user", "-e", "--env", "--secret"],
    ),
];

/// The command and flag positions when `args` hold the flag for a command that
/// takes it. Options of the
/// command are skipped; the first name or value ends the search.
fn flag_position(args: &[String]) -> Option<(usize, usize)> {
    let command = subcommand_index(args)?;
    let (_, with_value) = COMMANDS.iter().find(|(name, _)| *name == args[command])?;
    let mut at = command + 1;
    while let Some(arg) = args.get(at) {
        if LATEST_FLAGS.contains(&arg.as_str()) {
            return Some((command, at));
        }
        if arg == END_OF_OPTIONS || !arg.starts_with('-') {
            return None;
        }
        at += if with_value.contains(&arg.as_str()) {
            2
        } else {
            1
        };
    }
    None
}

/// `args` with the flag removed and `id` put where the name goes.
fn splice(args: &[String], command: usize, flag: usize, id: &str) -> Vec<String> {
    let mut out = args.to_vec();
    out.remove(flag);
    out.insert(command + 1, id.to_string());
    out
}

fn socket_of(args: &[String]) -> String {
    args.iter()
        .position(|arg| arg == SOCKET_FLAG)
        .and_then(|at| args.get(at + 1))
        .cloned()
        .unwrap_or_else(socket::default_socket)
}

/// Replaces `-l/--latest` with the ID of the newest sandbox, in any state.
///
/// # Errors
///
/// Fails when the daemon cannot be reached or has no sandbox.
pub async fn expand(args: Vec<String>) -> Result<Vec<String>> {
    let Some((command, flag)) = flag_position(&args) else {
        return Ok(args);
    };
    let mut c = client::connect(&socket_of(&args)).await?;
    let listed = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let newest = ps_filter::select(&listed, true, &ps_filter::Filters::default(), Some(1));
    let Some(newest) = newest.first() else {
        bail!("no sandbox to use as the latest");
    };
    Ok(splice(&args, command, flag, &newest.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[rstest::rstest]
    #[case::logs("ward logs -l", Some("ward logs ID"))]
    #[case::long_flag("ward logs --latest -f", Some("ward logs ID -f"))]
    #[case::after_flags("ward logs -f --tail 5 -l", Some("ward logs ID -f --tail 5"))]
    #[case::stop_time("ward stop -t 3 -l", Some("ward stop ID -t 3"))]
    #[case::kill_signal("ward kill -s KILL -l", Some("ward kill ID -s KILL"))]
    #[case::exec_command_flag("ward exec -l ls -l", Some("ward exec ID ls -l"))]
    #[case::global_socket("ward --socket /s rm -l -f", Some("ward --socket /s rm ID -f"))]
    #[case::named("ward logs web -l", None)]
    #[case::value_is_not_the_flag("ward logs --since -l", None)]
    #[case::after_end_of_options("ward exec -- -l", None)]
    #[case::ps_has_its_own("ward ps -l", None)]
    #[case::no_flag("ward stop web", None)]
    fn given_arguments_when_the_flag_is_spliced_then_the_id_takes_the_name_place(
        #[case] line: &str,
        #[case] expected: Option<&str>,
    ) {
        let args = words(line);

        let spliced =
            flag_position(&args).map(|(command, flag)| splice(&args, command, flag, "ID"));

        assert_eq!(spliced, expected.map(words));
    }
}
