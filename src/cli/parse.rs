//! CLI argv parsing, hyphen-text guardrails, and fast help/version exit path.

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};

use crate::config::Persona;

use super::args::Cli;

pub(super) fn hyphen_text_arg_ids(subcommand: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match subcommand {
        "send" => (&["subject", "body"], &[]),
        "reply" => (&[], &["body_md"]),
        "forward" => (&[], &["comment_md"]),
        "edit-draft" => (&["subject", "body"], &[]),
        "search" => (&[], &["query"]),
        "create-event" => (&["subject", "location", "body"], &[]),
        "update-event" => (&["subject"], &[]),
        "rsvp" => (&["comment"], &[]),
        "add-task" => (&["notes"], &["title"]),
        "update-task" => (&["title"], &[]),
        "contacts" => (&["query"], &[]),
        _ => (&[], &[]),
    }
}

pub(super) fn starts_with_hyphen(arg: &std::ffi::OsStr) -> bool {
    arg.as_encoded_bytes().starts_with(b"-")
}

pub(super) fn is_generic_flag_like_token(arg: &std::ffi::OsStr) -> bool {
    let Some(s) = arg.to_str() else {
        return false;
    };
    if s == "--" {
        return true;
    }
    if let Some(rest) = s.strip_prefix("--") {
        let name = rest.split_once('=').map_or(rest, |(k, _)| k);
        return !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    if let Some(rest) = s.strip_prefix('-') {
        if rest.is_empty() {
            return false;
        }
        if rest.len() == 1 && rest.as_bytes()[0].is_ascii_alphabetic() {
            return true;
        }
        if rest.chars().all(|ch| ch == 'h' || ch == 'V') {
            return true;
        }
    }
    false
}

pub(super) fn is_flag_like_token(arg: &std::ffi::OsStr, cmd: &clap::Command) -> bool {
    if is_generic_flag_like_token(arg) {
        return true;
    }
    if let Some(rest) = arg.to_str().and_then(|s| s.strip_prefix('-')) {
        if !rest.is_empty() && !rest.starts_with('-') {
            return rest.chars().all(|ch| {
                ch == 'h' || ch == 'V' || cmd.get_arguments().any(|a| a.get_short() == Some(ch))
            });
        }
    }
    false
}

pub(super) fn has_potential_hyphen_text(args: &[std::ffi::OsString]) -> bool {
    for tok in args {
        if tok == "--" {
            break;
        }
        if starts_with_hyphen(tok) && tok != "-" && !is_generic_flag_like_token(tok) {
            return true;
        }
    }
    false
}

pub(super) fn parse_with_cmd(
    mut cmd: clap::Command,
    args: &[std::ffi::OsString],
) -> std::result::Result<Cli, clap::Error> {
    let matches = cmd.try_get_matches_from_mut(args)?;
    Cli::from_arg_matches(&matches).map_err(|err| err.format(&mut cmd))
}

pub(super) fn find_long_arg<'a>(sub: &'a clap::Command, name: &str) -> Option<&'a clap::Arg> {
    sub.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

pub(super) fn is_known_subcommand_short_flag(arg: &std::ffi::OsStr, sub: &clap::Command) -> bool {
    let Some(rest) = arg.to_str().and_then(|s| s.strip_prefix('-')) else {
        return false;
    };
    !rest.is_empty()
        && !rest.starts_with('-')
        && rest
            .chars()
            .all(|ch| ch == 'h' || sub.get_arguments().any(|a| a.get_short() == Some(ch)))
}

/// Parses `outlook` CLI arguments while allowing separate hyphen-prefixed text literals
/// (such as `"- list item"` or `"- first\n- second"`) for free-text options and positionals,
/// without swallowing real flags, `--help`/`-h`, `--`, or flag-like tokens such as `--dry-run`.
pub fn parse_cli_from<I, S>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let raw: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    if raw.len() <= 2 || !has_potential_hyphen_text(&raw[2..]) {
        return Cli::try_parse_from(&raw);
    }

    let Some(first_sub) = raw[1].to_str() else {
        return Cli::try_parse_from(&raw);
    };

    let mut cmd = <Cli as CommandFactory>::command();
    let rewrite = {
        cmd.get_subcommands()
            .find(|sc| sc.get_name() == first_sub || sc.get_all_aliases().any(|a| a == first_sub))
            .and_then(|sub| {
                let (affected_opts, affected_pos) = hyphen_text_arg_ids(sub.get_name());
                if affected_opts.is_empty() && affected_pos.is_empty() {
                    return None;
                }

                let mut rewritten = Vec::with_capacity(raw.len());
                rewritten.push(raw[0].clone());
                rewritten.push(raw[1].clone());

                let mut enable_pos_hyphen: Vec<&'static str> = Vec::new();
                let mut block_pos_hyphen = false;
                let mut pos_index: usize = 1;
                let mut i = 2usize;

                while i < raw.len() {
                    let tok = &raw[i];
                    if tok == "--" {
                        rewritten.extend_from_slice(&raw[i..]);
                        break;
                    }

                    if let Some(long_rest) = tok.to_str().and_then(|s| s.strip_prefix("--")) {
                        if let Some((name, _)) = long_rest.split_once('=') {
                            if find_long_arg(sub, name).is_some() {
                                rewritten.push(tok.clone());
                                i += 1;
                                continue;
                            }
                            if is_flag_like_token(tok, sub) {
                                block_pos_hyphen = true;
                                rewritten.push(tok.clone());
                                i += 1;
                                continue;
                            }
                        } else if long_rest == "help" {
                            rewritten.push(tok.clone());
                            i += 1;
                            continue;
                        } else if let Some(arg) = find_long_arg(sub, long_rest) {
                            if arg.get_action().takes_values() {
                                if let Some(next) = raw.get(i + 1) {
                                    let arg_id = arg.get_id().as_str();
                                    if affected_opts.contains(&arg_id) {
                                        if starts_with_hyphen(next)
                                            && !is_flag_like_token(next, sub)
                                        {
                                            let mut attached = std::ffi::OsString::with_capacity(
                                                tok.len() + 1 + next.len(),
                                            );
                                            attached.push(tok);
                                            attached.push("=");
                                            attached.push(next);
                                            rewritten.push(attached);
                                            i += 2;
                                            continue;
                                        }
                                        if starts_with_hyphen(next) {
                                            block_pos_hyphen = true;
                                            rewritten.push(tok.clone());
                                            i += 1;
                                            continue;
                                        }
                                        rewritten.push(tok.clone());
                                        rewritten.push(next.clone());
                                        i += 2;
                                        continue;
                                    }
                                    if starts_with_hyphen(next) {
                                        block_pos_hyphen = true;
                                        rewritten.push(tok.clone());
                                        i += 1;
                                        continue;
                                    }
                                    rewritten.push(tok.clone());
                                    rewritten.push(next.clone());
                                    i += 2;
                                    continue;
                                }
                                block_pos_hyphen = true;
                                rewritten.push(tok.clone());
                                i += 1;
                                continue;
                            }
                            rewritten.push(tok.clone());
                            i += 1;
                            continue;
                        } else if is_flag_like_token(tok, sub) {
                            block_pos_hyphen = true;
                            rewritten.push(tok.clone());
                            i += 1;
                            continue;
                        }
                    } else if starts_with_hyphen(tok) && tok != "-" && is_flag_like_token(tok, sub)
                    {
                        if !is_known_subcommand_short_flag(tok, sub) {
                            block_pos_hyphen = true;
                        }
                        rewritten.push(tok.clone());
                        i += 1;
                        continue;
                    }

                    if starts_with_hyphen(tok) && tok != "-" && !block_pos_hyphen {
                        if let Some(pos_arg) = sub.get_positionals().nth(pos_index - 1) {
                            if let Some(&id) = affected_pos
                                .iter()
                                .find(|&&id| id == pos_arg.get_id().as_str())
                            {
                                if !enable_pos_hyphen.contains(&id) {
                                    enable_pos_hyphen.push(id);
                                }
                            }
                        }
                    }
                    rewritten.push(tok.clone());
                    pos_index += 1;
                    i += 1;
                }

                Some((sub.get_name().to_string(), rewritten, enable_pos_hyphen))
            })
    };

    let Some((sub_name, rewritten, enable_pos_hyphen)) = rewrite else {
        return parse_with_cmd(cmd, &raw);
    };

    if !enable_pos_hyphen.is_empty() {
        cmd = cmd.mut_subcommand(&sub_name, |mut sc| {
            for arg_id in &enable_pos_hyphen {
                sc = sc.mut_arg(*arg_id, |arg| arg.allow_hyphen_values(true));
            }
            sc
        });
    }

    parse_with_cmd(cmd, &rewritten)
}

/// Synchronously handles `--help`, `-h`, `--version`, `-V`, and `help` before Tokio/rustls/Keyring/SQLite init.
pub fn try_fast_help_or_version() -> Option<Result<()>> {
    try_fast_help_or_version_from(std::env::args_os(), Persona::detect())
}

pub(super) fn try_fast_help_or_version_from<I, S>(args: I, persona: Persona) -> Option<Result<()>>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let raw: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let mut after_bin = raw.iter().skip(1);
    let first = after_bin.next()?;
    let has_help_or_version = first.to_str() == Some("help")
        || std::iter::once(first)
            .chain(after_bin)
            .take_while(|arg| arg.to_str() != Some("--"))
            .any(|arg| matches!(arg.to_str(), Some("--help" | "-h" | "--version" | "-V")));

    if !has_help_or_version {
        return None;
    }

    let err = match persona {
        Persona::Outlook => parse_cli_from(&raw).err()?,
    };
    if matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    ) {
        let _ = err.print();
        return Some(Ok(()));
    }
    None
}
