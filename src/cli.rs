//! Hand-rolled argument parsing.
//!
//! No `clap`: the end state of this project is a single-window GUI, and the CLI
//! exists only to drive the audio engine during development. A parser this
//! small is not worth a dependency that would outlive its usefulness.

use anyhow::{Result, bail};

pub const USAGE: &str = "\
Lockstep — Windows dual-output audio router

USAGE:
    lockstep [list]
    lockstep play --sink <index|id> [--source <index|id>] [--duration <secs>]
    lockstep help

COMMANDS:
    list      Report every render endpoint with its ID and mix format (default)
    play      Loopback-capture the source endpoint and render it to the sink

OPTIONS:
    --sink <index|id>      Output endpoint. Required.
    --source <index|id>    Endpoint to capture. Defaults to the current default
                           Console render endpoint.
    --duration <secs>      Stop after this long. Without it, runs until Enter.

Indices are the bracketed numbers from `lockstep list`; IDs are the verbatim
device ID strings from the same report. IDs are stable across reboots and
renames, indices are not.";

#[derive(Debug, PartialEq)]
pub enum Command {
    List,
    Play(PlayArgs),
    Help,
}

#[derive(Debug, Default, PartialEq)]
pub struct PlayArgs {
    /// `None` means "the default Console render endpoint".
    pub source: Option<String>,
    pub sink: String,
    pub duration_secs: Option<f64>,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut args = args.into_iter();

    let Some(first) = args.next() else {
        return Ok(Command::List);
    };

    match first.as_str() {
        "list" => {
            if let Some(extra) = args.next() {
                bail!("`list` takes no arguments, got `{extra}`");
            }
            Ok(Command::List)
        }
        "help" | "--help" | "-h" => Ok(Command::Help),
        "play" => parse_play(args).map(Command::Play),
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

fn parse_play<I: Iterator<Item = String>>(mut args: I) -> Result<PlayArgs> {
    let mut parsed = PlayArgs::default();
    let mut sink: Option<String> = None;

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--source" => {
                parsed.source = Some(take_value(&mut args, "--source")?);
            }
            "--sink" => {
                sink = Some(take_value(&mut args, "--sink")?);
            }
            "--duration" => {
                let raw = take_value(&mut args, "--duration")?;
                let secs: f64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("`--duration` expects a number, got `{raw}`"))?;
                if !(secs.is_finite() && secs > 0.0) {
                    bail!("`--duration` must be a positive number of seconds, got `{raw}`");
                }
                parsed.duration_secs = Some(secs);
            }
            other => bail!("unknown option `{other}` for `play`\n\n{USAGE}"),
        }
    }

    parsed.sink = match sink {
        Some(sink) => sink,
        None => bail!("`play` requires --sink <index|id>\n\n{USAGE}"),
    };

    Ok(parsed)
}

fn take_value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String> {
    match args.next() {
        Some(value) if !value.starts_with("--") => Ok(value),
        Some(value) => bail!("`{flag}` expects a value, got the flag `{value}`"),
        None => bail!("`{flag}` expects a value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Command> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_lists() {
        assert_eq!(parse_str(&[]).unwrap(), Command::List);
        assert_eq!(parse_str(&["list"]).unwrap(), Command::List);
    }

    #[test]
    fn play_requires_sink() {
        assert!(parse_str(&["play"]).is_err());
        assert!(parse_str(&["play", "--source", "0"]).is_err());
    }

    #[test]
    fn play_parses_all_options() {
        let expected = Command::Play(PlayArgs {
            source: Some("0".into()),
            sink: "{0.0.0.00000000}.{abc}".into(),
            duration_secs: Some(5.0),
        });
        let parsed = parse_str(&[
            "play",
            "--source",
            "0",
            "--sink",
            "{0.0.0.00000000}.{abc}",
            "--duration",
            "5",
        ])
        .unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn source_defaults_to_none() {
        let Command::Play(args) = parse_str(&["play", "--sink", "4"]).unwrap() else {
            panic!("expected a play command");
        };
        assert_eq!(args.source, None);
    }

    #[test]
    fn rejects_bad_duration() {
        assert!(parse_str(&["play", "--sink", "4", "--duration", "0"]).is_err());
        assert!(parse_str(&["play", "--sink", "4", "--duration", "-2"]).is_err());
        assert!(parse_str(&["play", "--sink", "4", "--duration", "soon"]).is_err());
    }

    #[test]
    fn rejects_missing_value() {
        assert!(parse_str(&["play", "--sink"]).is_err());
        assert!(parse_str(&["play", "--sink", "--duration", "5"]).is_err());
    }
}
