//! Hand-rolled argument parsing.
//!
//! No `clap`: the end state of this project is a single-window GUI, and the CLI
//! exists only to drive the audio engine during development. A parser this
//! small is not worth a dependency that would outlive its usefulness.

use anyhow::{Result, bail};

pub const USAGE: &str = "\
Lockstep — Windows dual-output audio router

USAGE:
    lockstep                       Open the window
    lockstep list
    lockstep play --sink <index|id> [--sink <index|id>] [--source <index|id>]
                  [--delay <ms>]... [--duration <secs>] [--status-interval <secs>]
                  [--no-correction]
    lockstep help

COMMANDS:
    gui       Open the window (the default when no command is given)
    list      Report every render endpoint with its ID and mix format
    play      Loopback-capture the source endpoint and render it to the sinks

OPTIONS:
    --sink <index|id>          Output endpoint. Required. Repeat once for a
                               second simultaneous output (two is the maximum).
    --source <index|id>        Endpoint to capture. Defaults to the current
                               default Console render endpoint.
    --delay <ms>               Output delay, 0-250 ms. Repeatable: the first
                               --delay pairs with the first --sink, the second
                               with the second. Sinks with no --delay get 0.
    --duration <secs>          Stop after this long. Without it, runs until Enter.
    --status-interval <secs>   Seconds between status lines. Default 1.
    --no-correction            Disable drift correction. The ring is left to
                               fill or drain on its own, as in milestone 3 —
                               this is how uncorrected drift gets measured.

Indices are the bracketed numbers from `lockstep list`; IDs are the verbatim
device ID strings from the same report. IDs are stable across reboots and
renames, indices are not — prefer IDs for anything scripted.

While `play` is running, type commands on stdin:
    delay <sink-index> <ms>    Change a sink's delay, crossfaded over 10 ms
    quit                       Stop (a bare Enter does the same)";

#[derive(Debug, PartialEq)]
pub enum Command {
    Gui,
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
    /// Startup delays in milliseconds, paired positionally with `sinks`.
    pub delays_ms: Vec<f64>,
    pub duration_secs: Option<f64>,
    /// `None` means the default interval.
    pub status_interval_secs: Option<f64>,
    /// Set by `--no-correction`. Correction is on by default, so this is the
    /// opt-*out*; `PlayArgs::default()` therefore means "corrected".
    pub no_correction: bool,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut args = args.into_iter();

    // Bare `lockstep` opens the window: the GUI is the product, the CLI is the
    // instrument. `list` still has to be asked for by name.
    let Some(first) = args.next() else {
        return Ok(Command::Gui);
    };

    match first.as_str() {
        "gui" => {
            if let Some(extra) = args.next() {
                bail!("`gui` takes no arguments, got `{extra}`");
            }
            Ok(Command::Gui)
        }
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
            "--delay" => {
                let raw = take_value(&mut args, "--delay")?;
                let ms: f64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("`--delay` expects a number, got `{raw}`"))?;
                // Same range check the interactive driver uses, so both reject
                // the same values with the same wording.
                parsed
                    .delays_ms
                    .push(crate::audio::command::validate_delay_ms(ms)?);
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
    fn no_args_opens_the_window() {
        assert_eq!(parse_str(&[]).unwrap(), Command::Gui);
        assert_eq!(parse_str(&["gui"]).unwrap(), Command::Gui);
    }

    #[test]
    fn list_still_has_to_be_asked_for_by_name() {
        assert_eq!(parse_str(&["list"]).unwrap(), Command::List);
        assert!(parse_str(&["list", "extra"]).is_err());
        assert!(parse_str(&["gui", "extra"]).is_err());
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
            delays_ms: vec![120.0],
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
            "--delay",
            "120",
            "--duration",
            "5",
            "--status-interval",
            "2.5",
        ])
        .unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn delay_is_repeatable_and_ordered() {
        let Command::Play(args) = parse_str(&[
            "play", "--sink", "6", "--delay", "0", "--sink", "4", "--delay", "120",
        ])
        .unwrap() else {
            panic!("expected a play command");
        };
        assert_eq!(args.sinks, vec!["6".to_string(), "4".to_string()]);
        assert_eq!(args.delays_ms, vec![0.0, 120.0]);
    }

    #[test]
    fn delay_defaults_to_none_given() {
        let Command::Play(args) = parse_str(&["play", "--sink", "4"]).unwrap() else {
            panic!("expected a play command");
        };
        assert!(args.delays_ms.is_empty());
    }

    #[test]
    fn rejects_a_delay_outside_the_supported_range() {
        for bad in ["-1", "251", "1000", "soon"] {
            assert!(
                parse_str(&["play", "--sink", "4", "--delay", bad]).is_err(),
                "`--delay {bad}` should have been refused"
            );
        }
        // The boundaries are fine.
        assert!(parse_str(&["play", "--sink", "4", "--delay", "0"]).is_ok());
        assert!(parse_str(&["play", "--sink", "4", "--delay", "250"]).is_ok());
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
