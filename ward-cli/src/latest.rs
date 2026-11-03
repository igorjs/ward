// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Podman's `-l/--latest`: the newest sandbox stands in for the name. The
//! flag is replaced by that sandbox's ID before the arguments are parsed, so
//! each command keeps its own definition.

use anyhow::{Result, bail};

use crate::{client, ps_filter, socket, subcommand_index};

const LATEST_FLAGS: [&str; 2] = ["-l", "--latest"];
const SOCKET_FLAG: &str = "--socket";
const SOCKET_EQUALS: &str = "--socket=";
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
        at += if with_value.contains(&arg.as_str()) || arg == SOCKET_FLAG {
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

/// The socket the arguments name, as `--socket PATH` or `--socket=PATH`
/// anywhere before `--`. The last one wins, as in clap.
fn socket_in(args: &[String]) -> Option<String> {
    let command = subcommand_index(args).unwrap_or(args.len());
    let with_value = COMMANDS
        .iter()
        .find(|(name, _)| args.get(command).is_some_and(|arg| arg == name))
        .map_or(&[][..], |(_, flags)| *flags);
    let mut found = None;
    let mut at = 1;
    while let Some(arg) = args.get(at) {
        if at > command && (arg == END_OF_OPTIONS || !arg.starts_with('-')) {
            break;
        }
        if arg == SOCKET_FLAG {
            found = args.get(at + 1).cloned();
            at += 1;
        } else if let Some(path) = arg.strip_prefix(SOCKET_EQUALS) {
            found = Some(path.to_string());
        } else if at > command && with_value.contains(&arg.as_str()) {
            at += 1;
        }
        at += 1;
    }
    found
}

/// The socket the command will use: the flag, then `WARD_SOCKET`, then the
/// default, which is the order clap and `socket::default_socket` apply.
fn socket_of(args: &[String]) -> String {
    socket_in(args).unwrap_or_else(socket::default_socket)
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
    #[case::separate("ward --socket /a logs -l", Some("/a"))]
    #[case::equals("ward --socket=/b logs -l", Some("/b"))]
    #[case::after_the_command("ward logs -l --socket /c", Some("/c"))]
    #[case::last_wins("ward --socket /a --socket=/d logs -l", Some("/d"))]
    #[case::after_end_of_options("ward exec -- --socket /e", None)]
    #[case::absent("ward logs -l", None)]
    #[case::in_the_exec_command("ward exec -l sh -c x --socket=/other.sock", None)]
    #[case::in_the_exec_command_separate("ward exec -l sh --socket /other.sock", None)]
    #[case::before_the_exec_command("ward --socket /a exec -l sh --socket=/other.sock", Some("/a"))]
    #[case::option_value_is_not_a_flag("ward exec -e --socket=/x -l sh", None)]
    fn given_arguments_when_the_socket_is_read_then_both_flag_forms_count(
        #[case] line: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(socket_in(&words(line)).as_deref(), expected);
    }

    #[test]
    fn given_a_socket_flag_after_the_command_when_spliced_then_its_value_is_not_the_name() {
        let args = words("ward logs --socket /s -l");

        let spliced =
            flag_position(&args).map(|(command, flag)| splice(&args, command, flag, "ID"));

        assert_eq!(spliced, Some(words("ward logs ID --socket /s")));
    }

    #[test]
    fn given_the_env_socket_when_resolved_then_it_is_the_default() {
        assert_eq!(
            socket::resolve(Some("/env.sock"), None, None, None),
            "/env.sock"
        );
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
