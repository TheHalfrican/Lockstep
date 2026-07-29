//! Parameter changes crossing from the control thread to a render thread.
//!
//! # Why a queue and not an atomic
//!
//! CLAUDE.md's rule is atomics or a triple buffer for parameter changes, and an
//! `rtrb` command queue for anything that cannot be expressed atomically. A
//! delay is only a `usize`, so it *looks* atomic-able — but it is not, because
//! what matters is not the value, it is each distinct *transition*. Every
//! change starts a crossfade, and the delay line coalesces changes deliberately
//! in its own pending slot. Publishing the delay through an atomic would let two
//! changes silently merge into one before the render thread ever saw the first,
//! which is a different behaviour from the one the delay line was designed and
//! tested for.
//!
//! Gain and mute, when they arrive, genuinely are atomics: their value is the
//! whole story and a missed intermediate value is invisible. They should not go
//! through this queue.
//!
//! # Real-time safety
//!
//! [`Command`] is `Copy` and contains no heap data, so draining the queue on the
//! audio thread moves plain bytes and drops nothing. Parsing — the part that
//! allocates and can fail — happens on the control thread; only the enum
//! crosses the boundary.

use super::delay::{MAX_DELAY_MS, ms_to_frames};

/// One parameter change for one render thread.
///
/// Deliberately an enum with a single variant today. Gain and mute will be
/// atomics, but a crossfade-triggering setting like the click-train calibration
/// toggle will want to arrive here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Move to this delay, in frames, via the delay line's crossfade.
    ///
    /// Frames rather than milliseconds because frames are the delay line's
    /// canonical unit and the conversion needs a sample rate, which the control
    /// thread already has. The render thread should do no arithmetic it can
    /// avoid.
    SetDelayFrames(usize),
}

/// What one line of operator input asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinAction {
    /// Wind the session down.
    Stop,
    /// Deliver a command to one sink's render thread.
    Send { sink: usize, command: Command },
}

/// Parse a line of operator input.
///
/// Control thread only: it allocates error strings and is not remotely
/// real-time safe. That is the point of the split.
pub fn parse_line(line: &str, sink_count: usize, sample_rate: u32) -> Result<StdinAction, String> {
    let trimmed = line.trim();

    // A bare Enter is the documented way to stop an open-ended run.
    if trimmed.is_empty() {
        return Ok(StdinAction::Stop);
    }

    let mut tokens = trimmed.split_whitespace();
    let verb = tokens.next().unwrap_or_default();

    match verb {
        "quit" | "exit" | "q" | "stop" => Ok(StdinAction::Stop),
        "delay" => {
            let sink_token = tokens
                .next()
                .ok_or_else(|| "usage: delay <sink-index> <ms>".to_string())?;
            let ms_token = tokens
                .next()
                .ok_or_else(|| "usage: delay <sink-index> <ms>".to_string())?;
            if let Some(extra) = tokens.next() {
                return Err(format!(
                    "unexpected `{extra}`; usage: delay <sink-index> <ms>"
                ));
            }

            let sink: usize = sink_token
                .parse()
                .map_err(|_| format!("`{sink_token}` is not a sink index"))?;
            if sink >= sink_count {
                return Err(format!(
                    "no sink {sink}; this session has {sink_count} sink(s), numbered 0..{}",
                    sink_count.saturating_sub(1)
                ));
            }

            let ms: f64 = ms_token
                .parse()
                .map_err(|_| format!("`{ms_token}` is not a number of milliseconds"))?;
            let frames = validate_delay_ms(ms)
                .map(|ms| ms_to_frames(ms, sample_rate))
                .map_err(|e| e.to_string())?;

            Ok(StdinAction::Send {
                sink,
                command: Command::SetDelayFrames(frames),
            })
        }
        other => Err(format!(
            "unknown command `{other}`; try `delay <sink-index> <ms>` or `quit`"
        )),
    }
}

/// Check a delay against the range the delay line is built for.
///
/// Shared by the CLI and the interactive driver so both reject the same values
/// with the same wording.
pub fn validate_delay_ms(ms: f64) -> Result<f64, DelayRangeError> {
    if !ms.is_finite() {
        return Err(DelayRangeError { given: ms });
    }
    if !(0.0..=MAX_DELAY_MS).contains(&ms) {
        return Err(DelayRangeError { given: ms });
    }
    Ok(ms)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelayRangeError {
    pub given: f64,
}

impl std::fmt::Display for DelayRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delay must be between 0 and {MAX_DELAY_MS:.0} ms, got {}",
            self.given
        )
    }
}

impl std::error::Error for DelayRangeError {}

/// Pair `--delay` values with `--sink` occurrences, positionally.
///
/// Sinks without a matching delay get zero, which is the common case: most runs
/// name two sinks and delay only one of them.
pub fn pair_delays(sink_count: usize, delays_ms: &[f64]) -> Result<Vec<f64>, String> {
    if delays_ms.len() > sink_count {
        return Err(format!(
            "{} --delay value(s) given but only {sink_count} --sink(s); each --delay pairs with \
             the --sink in the same position",
            delays_ms.len()
        ));
    }

    let mut paired = vec![0.0; sink_count];
    for (slot, ms) in paired.iter_mut().zip(delays_ms) {
        *slot = *ms;
    }
    Ok(paired)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    #[test]
    fn an_empty_line_stops_the_session() {
        assert_eq!(parse_line("", 2, RATE).unwrap(), StdinAction::Stop);
        assert_eq!(parse_line("   \t ", 2, RATE).unwrap(), StdinAction::Stop);
    }

    #[test]
    fn the_quit_words_stop_the_session() {
        for word in ["quit", "exit", "q", "stop"] {
            assert_eq!(parse_line(word, 2, RATE).unwrap(), StdinAction::Stop);
        }
    }

    #[test]
    fn a_delay_command_converts_to_frames() {
        let action = parse_line("delay 1 120", 2, RATE).unwrap();
        assert_eq!(
            action,
            StdinAction::Send {
                sink: 1,
                // 120 ms at 48 kHz.
                command: Command::SetDelayFrames(5_760),
            }
        );
    }

    #[test]
    fn a_delay_command_accepts_fractional_milliseconds() {
        let action = parse_line("delay 0 0.5", 2, RATE).unwrap();
        assert_eq!(
            action,
            StdinAction::Send {
                sink: 0,
                command: Command::SetDelayFrames(24),
            }
        );
    }

    #[test]
    fn extra_whitespace_is_tolerated() {
        assert_eq!(
            parse_line("  delay   1    120  ", 2, RATE).unwrap(),
            StdinAction::Send {
                sink: 1,
                command: Command::SetDelayFrames(5_760),
            }
        );
    }

    #[test]
    fn zero_delay_is_a_valid_command() {
        assert_eq!(
            parse_line("delay 0 0", 2, RATE).unwrap(),
            StdinAction::Send {
                sink: 0,
                command: Command::SetDelayFrames(0),
            }
        );
    }

    #[test]
    fn a_sink_index_past_the_end_is_refused() {
        let error = parse_line("delay 5 100", 2, RATE).unwrap_err();
        assert!(error.contains("no sink 5"), "{error}");
        assert!(error.contains("0..1"), "{error}");
    }

    #[test]
    fn a_delay_outside_the_supported_range_is_refused() {
        for line in ["delay 0 -1", "delay 0 251", "delay 0 1000"] {
            let error = parse_line(line, 2, RATE).unwrap_err();
            assert!(error.contains("between 0 and 250 ms"), "{line}: {error}");
        }
    }

    #[test]
    fn malformed_input_explains_itself() {
        for (line, needle) in [
            ("delay", "usage"),
            ("delay 1", "usage"),
            ("delay one 100", "not a sink index"),
            ("delay 1 soon", "not a number"),
            ("delay 1 100 extra", "unexpected"),
            ("wobble 1 100", "unknown command"),
        ] {
            let error = parse_line(line, 2, RATE).unwrap_err();
            assert!(
                error.contains(needle),
                "`{line}` produced `{error}`, expected it to mention `{needle}`"
            );
        }
    }

    #[test]
    fn the_boundary_delays_are_accepted() {
        assert!(validate_delay_ms(0.0).is_ok());
        assert!(validate_delay_ms(MAX_DELAY_MS).is_ok());
        assert!(validate_delay_ms(MAX_DELAY_MS + 0.001).is_err());
        assert!(validate_delay_ms(-0.001).is_err());
        assert!(validate_delay_ms(f64::NAN).is_err());
        assert!(validate_delay_ms(f64::INFINITY).is_err());
    }

    #[test]
    fn delays_pair_positionally_with_sinks() {
        assert_eq!(pair_delays(2, &[0.0, 120.0]).unwrap(), vec![0.0, 120.0]);
        assert_eq!(pair_delays(2, &[120.0]).unwrap(), vec![120.0, 0.0]);
    }

    #[test]
    fn sinks_without_a_delay_default_to_zero() {
        assert_eq!(pair_delays(2, &[]).unwrap(), vec![0.0, 0.0]);
        assert_eq!(pair_delays(1, &[]).unwrap(), vec![0.0]);
    }

    #[test]
    fn more_delays_than_sinks_is_refused() {
        let error = pair_delays(1, &[0.0, 120.0]).unwrap_err();
        assert!(error.contains("2 --delay"), "{error}");
        assert!(error.contains("only 1 --sink"), "{error}");
    }

    #[test]
    fn commands_are_copy_and_cheap_to_move() {
        // The queue drain on the audio thread relies on this: no Drop, no heap.
        fn assert_copy<T: Copy>() {}
        assert_copy::<Command>();
        assert!(!std::mem::needs_drop::<Command>());
    }
}
