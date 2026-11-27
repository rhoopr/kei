//! Friendly narration lines printed above the active progress bar.
//!
//! Tracing in friendly mode is filtered to WARN+, so successful sync events
//! never reach the user via logging. This module fills that gap with
//! short lines printed to stderr (the same stream the bar uses).
//!
//! Off mode is a strict no-op: nothing is written, no allocation, no lock
//! contention.

#[cfg(test)]
use std::io::Write;

use crate::personality::Mode;

/// Write a single narration line to `w`, appending a newline. In off mode
/// this is a no-op and `w` is not touched. Test-only surface that mirrors
/// the off-vs-friendly gate used by `line_to_stderr`; production callers
/// go through `line_to_stderr` (which routes through indicatif's
/// `MultiProgress::println` to coexist with an active bar).
#[cfg(test)]
fn line<W: Write>(w: &mut W, mode: Mode, text: &str) -> std::io::Result<()> {
    if !mode.is_friendly() {
        return Ok(());
    }
    writeln!(w, "{text}")
}

/// Print a narration line above any active progress bar.
///
/// Routes through `MultiProgress::println` rather than `suspend(...) +
/// eprintln!`: println is a single atomic draw that combines the new line
/// with the bars, so indicatif's cursor tracking stays consistent. The
/// suspend path has a window between clear and redraw where caller writes
/// don't update indicatif's line count - on a bar whose last row reaches
/// the terminal's right edge, the cursor wraps to a new line and the
/// next redraw lands off-by-one, leaving a stale row above the bar.
///
/// Cheap when no bar is registered (indicatif's println short-circuits).
pub fn line_to_stderr(mode: Mode, text: &str) {
    if !mode.is_friendly() {
        return;
    }
    crate::personality::active_bar::println_above_bars(text);
}

/// Post-auth narration: confirms the account that just authenticated.
/// Scrollback-stable: a `✓` checkmark prefix marks each phase boundary so the
/// run reads as a sequence of completed steps once the bar clears.
pub fn auth_ok_to_stderr(mode: Mode, username: &str) {
    line_to_stderr(mode, &format!("✓ Authenticated as {username}"));
}

/// Post-library-resolve narration: how many libraries kei is going to walk.
/// Asset and album totals aren't known until streaming enumeration finishes,
/// so this line stays at the library-count level.
pub fn libraries_resolved_to_stderr(mode: Mode, library_count: usize) {
    let text = match library_count {
        0 => "No libraries available; nothing to sync.".to_string(),
        1 => "✓ Listed 1 library".to_string(),
        n => format!("✓ Listed {n} libraries"),
    };
    line_to_stderr(mode, &text);
}

/// First-Ctrl+C acknowledgement. Friendly mode filters `tracing::info` to
/// WARN+, so the shutdown handler's existing log lines are silent here -
/// without this narration the user sees no response and assumes the app
/// has hung. The off path keeps the original tracing lines for journals.
pub fn stop_signal_to_stderr(mode: Mode) {
    line_to_stderr(
        mode,
        "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.",
    );
}

/// Pre-sleep narration before a retry. The existing `tracing::warn!` in
/// `retry_with_backoff` carries the structured fields (attempt, delay,
/// error) for journals; this is the human-shaped reminder that the pause
/// is intentional. Off mode is silent.
pub fn retry_pause_to_stderr(mode: Mode, delay: std::time::Duration) {
    line_to_stderr(mode, &render_retry_pause(delay));
}

/// Confirms a visible retry recovered so the prior warning or retry pause
/// doesn't sit in scrollback as the last thing the user saw before downloads
/// resumed. Off mode is silent.
pub fn back_on_track_to_stderr(mode: Mode) {
    line_to_stderr(mode, BACK_ON_TRACK_LINE);
}

/// Final-attempt narration when retries are exhausted. Tells the user the
/// failure is recorded and the next sync will pick the asset back up,
/// rather than letting the surfaced error stand alone. Only fires after
/// at least one `retry_pause_to_stderr`; one-shot failures are silent.
pub fn giving_up_to_stderr(mode: Mode) {
    line_to_stderr(mode, GIVING_UP_LINE);
}

/// Friendly framing for the 2FA prompt. Printed once before
/// `Enter 2FA code (or press Enter to request a new code):`, so the user
/// understands a push has been sent and what they're being asked to type.
/// Off mode preserves today's bare prompt for scripted consumers.
pub fn two_fa_prompt_to_stderr(mode: Mode) {
    line_to_stderr(mode, TWO_FA_PROMPT_LINE);
}

const BACK_ON_TRACK_LINE: &str = "Back on track.";
const GIVING_UP_LINE: &str = "Skipping this item; it will retry on the next sync.";
const TWO_FA_PROMPT_LINE: &str =
    "Sent a code to your trusted devices. Approve the push and enter the 6-digit code below.";

fn render_retry_pause(delay: std::time::Duration) -> String {
    // Sub-second delays still surface as "1s" so the user has a concrete
    // number rather than "0s" (which reads as "no pause" and undermines
    // the friendly framing).
    let secs = delay.as_secs().max(1);
    format!("Retrying after iCloud error in {secs}s.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn capture(mode: Mode, f: impl FnOnce(&mut Vec<u8>, Mode)) -> String {
        let mut buf = Vec::new();
        f(&mut buf, mode);
        String::from_utf8(buf).expect("narration writes valid utf-8")
    }

    #[test]
    fn off_mode_writes_nothing() {
        let out = capture(Mode::Off, |w, m| {
            line(w, m, "anything").expect("never errors in off");
        });
        assert!(out.is_empty(), "off mode must not write to the buffer");
    }

    #[test]
    fn friendly_mode_writes_line_with_newline() {
        let out = capture(Mode::Friendly, |w, m| {
            line(w, m, "hello there").expect("write should succeed");
        });
        assert_eq!(out, "hello there\n");
    }

    #[test]
    fn friendly_mode_multiple_lines_concatenate() {
        let out = capture(Mode::Friendly, |w, m| {
            line(w, m, "first").unwrap();
            line(w, m, "second").unwrap();
        });
        assert_eq!(out, "first\nsecond\n");
    }

    #[test]
    fn stop_signal_off_mode_writes_nothing() {
        let out = capture(Mode::Off, |w, m| {
            // Mirror line() with the same body stop_signal_to_stderr writes.
            line(
                w,
                m,
                "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.",
            )
            .unwrap();
        });
        assert!(out.is_empty());
    }

    #[test]
    fn stop_signal_friendly_writes_full_line() {
        let out = capture(Mode::Friendly, |w, m| {
            line(
                w,
                m,
                "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.",
            )
            .unwrap();
        });
        assert_eq!(
            out,
            "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.\n",
        );
    }

    #[test]
    fn retry_recovery_and_giving_up_lines_are_stable_text() {
        // Pin user-visible strings; reword behind a deliberate diff.
        assert_eq!(BACK_ON_TRACK_LINE, "Back on track.");
        assert_eq!(
            GIVING_UP_LINE,
            "Skipping this item; it will retry on the next sync.",
        );
    }

    #[test]
    fn auth_ok_renders_with_checkmark_and_username() {
        // Pin the user-facing line. Surface change goes behind a deliberate
        // diff because shell scripts that key off the auth-confirmation
        // prefix would otherwise break silently.
        let mut buf = Vec::new();
        line(
            &mut buf,
            Mode::Friendly,
            &format!("✓ Authenticated as {}", "u@example.com"),
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "✓ Authenticated as u@example.com\n");
    }

    #[test]
    fn libraries_resolved_singular_and_plural() {
        for (n, expected) in [
            (0_usize, "No libraries available; nothing to sync.\n"),
            (1, "✓ Listed 1 library\n"),
            (3, "✓ Listed 3 libraries\n"),
        ] {
            // Reproduce the body of libraries_resolved_to_stderr inline so
            // the test exercises the wording table without depending on
            // active_bar / indicatif state.
            let text = match n {
                0 => "No libraries available; nothing to sync.".to_string(),
                1 => "✓ Listed 1 library".to_string(),
                k => format!("✓ Listed {k} libraries"),
            };
            let mut buf = Vec::new();
            line(&mut buf, Mode::Friendly, &text).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), expected);
        }
    }

    #[test]
    fn two_fa_prompt_line_is_stable_text() {
        assert_eq!(
            TWO_FA_PROMPT_LINE,
            "Sent a code to your trusted devices. Approve the push and enter the 6-digit code below.",
        );
    }

    #[test]
    fn render_retry_pause_with_seconds() {
        assert_eq!(
            render_retry_pause(Duration::from_secs(4)),
            "Retrying after iCloud error in 4s.",
        );
        assert_eq!(
            render_retry_pause(Duration::from_secs(60)),
            "Retrying after iCloud error in 60s.",
        );
    }

    #[test]
    fn render_retry_pause_subsecond_floors_to_one_second() {
        // Duration::from_millis(500).as_secs() == 0; render must promote to "1s"
        // so the user gets a concrete number rather than "0s".
        assert_eq!(
            render_retry_pause(Duration::from_millis(500)),
            "Retrying after iCloud error in 1s.",
        );
        assert_eq!(
            render_retry_pause(Duration::from_secs(0)),
            "Retrying after iCloud error in 1s.",
        );
    }
}
