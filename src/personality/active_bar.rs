//! Process-wide bar/tracing coordination via a singleton `MultiProgress`.
//!
//! Indicatif redraws progress bars by emitting ANSI cursor escapes that
//! assume nothing else has written to the terminal between draws. Tracing
//! writes to stderr break that assumption: a WARN/ERROR event lands mid-
//! redraw and the bar's lines duplicate or get partially overwritten.
//!
//! `MultiProgress` provides `suspend(f)` which clears the bars, runs `f`
//! (where stderr writes happen), and redraws. We register every bar with the
//! singleton and wrap stderr writes in `with_suspended` so tracing events
//! cleanly print above an intact bar.

use std::sync::OnceLock;

use indicatif::MultiProgress;

/// Process-wide MultiProgress. Lazily initialised so test binaries that never
/// touch the bar surface don't pay the cost.
static MULTI: OnceLock<MultiProgress> = OnceLock::new();

/// Return the singleton MultiProgress, initialising on first call.
pub fn multi() -> &'static MultiProgress {
    MULTI.get_or_init(MultiProgress::new)
}

/// Run `f` with all registered bars temporarily hidden. Used by the stderr
/// writer wrapper so tracing events print above the bar block without
/// colliding with an in-flight redraw.
///
/// Cheap when no bars are registered: indicatif's `suspend` short-circuits
/// to a direct call. Safe to invoke on every stderr write.
pub fn with_suspended<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    multi().suspend(f)
}

/// Print a line above the registered bars in a single atomic draw.
///
/// `MultiProgress::suspend` has a clear -> caller-writes -> redraw cycle
/// where the caller's writes don't update indicatif's line tracking. If
/// the cursor was at the right edge of the last bar row (indicatif fills
/// the last row to `term_width` so subsequent writes wrap to a new line),
/// suspend's redraw can land off-by-one and leave a stale row above the
/// bar. `MultiProgress::println` builds the new line plus the bars into
/// one `draw` call, so the cursor accounting stays internally consistent.
///
/// Strips a single trailing `\n` if present so callers can keep using
/// `writeln!`/`println!`-style strings without the extra blank line that
/// indicatif's println would otherwise emit.
pub fn println_above_bars(msg: &str) {
    let trimmed = msg.strip_suffix('\n').unwrap_or(msg);
    let _ = multi().println(trimmed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_returns_same_instance() {
        let a = multi() as *const MultiProgress;
        let b = multi() as *const MultiProgress;
        assert_eq!(a, b, "multi() must be a singleton");
    }

    #[test]
    fn with_suspended_returns_closure_value() {
        let v: u32 = with_suspended(|| 42);
        assert_eq!(v, 42);
    }
}
