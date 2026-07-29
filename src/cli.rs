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
    lockstep play --sink <index|id> [--sink <index|id>] [--source <index|id>]
                  [--duration <secs>] [--status-interval <secs>] [--no-correction]
    lockstep help

COMMANDS:
    list      Report every render endpoint with its ID and mix format (default)
    play      Loopback-capture the source endpoint and render it to the sinks

OPTIONS:
    --sink <index|id>          Output endpoint. Required. Repeat once for a
                               second simultaneous output (two is the maximum).
    --source <index|id>        Endpoint to capture. Defaults to the current
                               default Console render endpoint.
    --duration <secs>          Stop after this long. Without it, runs until Enter.
    --status-interval <secs>   Seconds between status lines. Default 1.
    --no-correction            Disable drift correction. The ring is left to
                               fill or drain on its own, as in milestone 3 —
                               this is how uncorrected drift gets measured.

Indices are the bracketed numbers from `lockstep list`; IDs are the verbatim
device ID strings from the same report. IDs are stable across reboots and
renames, indices are not — prefer IDs for anything scripted.";

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
    /// One or two output endpoints, in the order given.
    pub sinks: Vec<String>,
    pub duration_secs: Option<f64>,
    /// `None` means the default interval.
    pub status_interval_secs: Option<f64>,
    /// Set by `--no-correction`. Correction is on by default, so this is the
    /// opt-*out*; `PlayArgs::default()` therefore means "corrected".
    pub no_correction: bool,
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

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--source" => {
                parsed.source = Some(take_value(&mut args, "--source")?);
            }
            "--sink" => {
                parsed.sinks.push(take_value(&mut args, "--sink")?);
            }
            "--duration" => {
                parsed.duration_secs = Some(take_positive(&mut args, "--duration")?);
            }
            "--status-interval" => {
                parsed.status_interval_secs = Some(take_positive(&mut args, "--status-interval")?);
            }
            "--no-correction" => {
                parsed.no_correction = true;
            }
            other => bail!("unknown option `{other}` for `play`\n\n{USAGE}"),
        }
    }

    if parsed.sinks.is_empty() {
        bail!("`play` requires at least one --sink <index|id>\n\n{USAGE}");
    }
    // Two simultaneous outputs is the whole point of the project and also its
    // ceiling; CLAUDE.md lists more than two as an explicit non-goal.
    if parsed.sinks.len() > crate::audio::MAX_SINKS {
        bail!(
            "at most {} --sink options are supported, got {}",
            crate::audio::MAX_SINKS,
            parsed.sinks.len()
        );
    }

    Ok(parsed)
}

fn take_positive<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<f64> {
    let raw = take_value(args, flag)?;
    let value: f64 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("`{flag}` expects a number, got `{raw}`"))?;
    if !(value.is_finite() && value > 0.0) {
        bail!("`{flag}` must be a positive number of seconds, got `{raw}`");
    }
    Ok(value)
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
            sinks: vec!["{0.0.0.00000000}.{abc}".into()],
            duration_secs: Some(5.0),
            status_interval_secs: Some(2.5),
            no_correction: false,
        });
        let parsed = parse_str(&[
            "play",
            "--source",
            "0",
            "--sink",
            "{0.0.0.00000000}.{abc}",
            "--duration",
            "5",
            "--status-interval",
            "2.5",
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
        assert_eq!(args.status_interval_secs, None);
    }

    #[test]
    fn sink_is_repeatable_and_ordered() {
        let Command::Play(args) =
            parse_str(&["play", "--sink", "6", "--sink", "4", "--duration", "5"]).unwrap()
        else {
            panic!("expected a play command");
        };
        assert_eq!(args.sinks, vec!["6".to_string(), "4".to_string()]);
    }

    #[test]
    fn rejects_more_than_two_sinks() {
        assert!(parse_str(&["play", "--sink", "1", "--sink", "2", "--sink", "3"]).is_err());
    }

    #[test]
    fn rejects_bad_duration() {
        assert!(parse_str(&["play", "--sink", "4", "--duration", "0"]).is_err());
        assert!(parse_str(&["play", "--sink", "4", "--duration", "-2"]).is_err());
        assert!(parse_str(&["play", "--sink", "4", "--duration", "soon"]).is_err());
    }

    #[test]
    fn correction_is_on_unless_opted_out() {
        let Command::Play(on) = parse_str(&["play", "--sink", "4"]).unwrap() else {
            panic!("expected a play command");
        };
        assert!(!on.no_correction, "correction must default to enabled");

        let Command::Play(off) = parse_str(&["play", "--sink", "4", "--no-correction"]).unwrap()
        else {
            panic!("expected a play command");
        };
        assert!(off.no_correction);
    }

    #[test]
    fn no_correction_takes_no_value() {
        // It is a bare flag, so the next token must still parse as a flag.
        let Command::Play(args) =
            parse_str(&["play", "--no-correction", "--sink", "4", "--duration", "10"]).unwrap()
        else {
            panic!("expected a play command");
        };
        assert!(args.no_correction);
        assert_eq!(args.sinks, vec!["4".to_string()]);
        assert_eq!(args.duration_secs, Some(10.0));
    }

    #[test]
    fn rejects_bad_status_interval() {
        assert!(parse_str(&["play", "--sink", "4", "--status-interval", "0"]).is_err());
        assert!(parse_str(&["play", "--sink", "4", "--status-interval", "nope"]).is_err());
    }

    #[test]
    fn rejects_missing_value() {
        assert!(parse_str(&["play", "--sink"]).is_err());
        assert!(parse_str(&["play", "--sink", "--duration", "5"]).is_err());
    }
}
