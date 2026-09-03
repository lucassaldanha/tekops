//! Making untrusted text safe to print to a terminal.
//!
//! Two channels carry data this binary didn't author all the way to the
//! operator's screen: log fields (peer identifiers, remote agent strings,
//! exception text) rendered by `less -R`, and an endpoint's error body echoed
//! to stderr. Printed verbatim, either can clear the screen, retitle the
//! window, or forge output that looks like it came from tekops itself.

/// Replaces control characters with the Unicode replacement character,
/// keeping the text readable while stripping its ability to drive the
/// terminal.
pub fn sanitize(raw: &str) -> String {
    raw.chars().map(|c| if is_safe(c) { c } else { '\u{fffd}' }).collect()
}

/// Tab and newline are the only control characters worth keeping - they're
/// load-bearing in multi-line log output, notably Java stack traces. Everything
/// else in the C0/C1 ranges (`ESC` above all) is replaced.
fn is_safe(c: char) -> bool {
    c == '\t' || c == '\n' || !(c.is_control() || matches!(c, '\u{80}'..='\u{9f}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequences() {
        let evil = "\u{1b}[2J\u{1b}]0;PWNED\u{7}text";
        let clean = sanitize(evil);
        assert!(!clean.contains('\u{1b}'), "ESC survived: {clean:?}");
        assert!(!clean.contains('\u{7}'), "BEL survived: {clean:?}");
        assert!(clean.ends_with("text"));
    }

    #[test]
    fn keeps_tabs_newlines_and_unicode() {
        assert_eq!(sanitize("a\tb\nc"), "a\tb\nc");
        assert_eq!(sanitize("héllo ✓"), "héllo ✓");
    }

    #[test]
    fn strips_c1_controls() {
        assert!(!sanitize("a\u{9b}b").contains('\u{9b}'));
    }

    #[test]
    fn strips_carriage_returns_that_would_overwrite_the_line() {
        assert!(!sanitize("real line\rforged line").contains('\r'));
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let plain = "Slot 123456 imported, peer 16Uiu2HAm, 0.42s";
        assert_eq!(sanitize(plain), plain);
    }
}
