//! Making untrusted text safe to print to a terminal.
//!
//! Two channels carry data this binary didn't author all the way to the
//! operator's screen: log fields (peer identifiers, remote agent strings,
//! exception text) rendered by `less -R`, and an endpoint's error body echoed
//! to stderr. Printed verbatim, either can clear the screen, retitle the
//! window, or forge output that looks like it came from tekops itself.

/// Removes complete ANSI CSI sequences, then replaces any remaining control
/// character with the Unicode replacement character, keeping the text
/// readable while stripping its ability to drive the terminal.
///
/// CSI (`ESC [ params intermediates final`) is handled separately from every
/// other control character because Teku itself colours its event-log
/// messages this way under Docker: the old per-character replacement turned
/// `ESC` into U+FFFD but left the rest of the sequence - `[37m` and so on -
/// as visible literal text, which is exactly the noise this function is
/// supposed to prevent. Removing the whole sequence is strictly safer than
/// the old behaviour, not weaker: a hostile `ESC[31mFAKE ERROR ESC[0m`
/// becomes the plain text "FAKE ERROR", with no colour and no cursor
/// control - the visible payload survived under the old behaviour too, just
/// with a U+FFFD glued to the front of it. Passing CSI through unmodified
/// would reintroduce the screen-clear/retitle/forged-line attack this
/// function exists to close - see the module doc.
///
/// Every other escape form - OSC (`ESC ]`, used for window titles) and a bare
/// `ESC` - keeps the old per-character behaviour: the `ESC` becomes U+FFFD
/// and the remaining bytes stay as inert literal text.
pub fn sanitize(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\u{1b}' && chars.get(i + 1) == Some(&'[') {
            match find_csi_final(&chars, i + 2) {
                Some(final_idx) => {
                    // The whole sequence - ESC through the final byte - carries
                    // no information a plain-text reader needs, so none of it
                    // is emitted.
                    i = final_idx + 1;
                }
                None => {
                    // Truncated or malformed: a naive "consume until 'm'"
                    // would run to end-of-string looking for a final byte
                    // that never arrives, swallowing the rest of the line.
                    // Instead, only the aborted "ESC [" attempt becomes a
                    // single U+FFFD, and normal processing resumes right
                    // after it - so a real line that happens to end mid-
                    // sequence still shows everything that came after.
                    out.push('\u{fffd}');
                    i += 2;
                }
            }
            continue;
        }
        out.push(if is_safe(c) { c } else { '\u{fffd}' });
        i += 1;
    }
    out
}

/// Scans forward from just after `ESC [` for a valid CSI final byte, per the
/// grammar `ESC [ params* intermediates* final`: parameter bytes
/// `0x30..=0x3F`, intermediate bytes `0x20..=0x2F`, one final byte
/// `0x40..=0x7E`. Returns the index of the final byte, or `None` if the
/// sequence runs off the end of input without ever reaching one - the signal
/// to the caller that this was not a real CSI sequence at all.
fn find_csi_final(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start;
    while j < chars.len() && matches!(chars[j], '\u{30}'..='\u{3f}') {
        j += 1;
    }
    while j < chars.len() && matches!(chars[j], '\u{20}'..='\u{2f}') {
        j += 1;
    }
    if j < chars.len() && matches!(chars[j], '\u{40}'..='\u{7e}') {
        Some(j)
    } else {
        None
    }
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

    // Teku colours its own event-log messages inline under Docker. Passing the
    // ESC through as U+FFFD (the old behaviour) left the rest of the CSI
    // sequence - "[37m" and so on - as visible garbage in front of otherwise
    // ordinary text. A CSI sequence carries no information a terminal-unaware
    // reader needs, and removing it entirely also closes the same forged-output
    // hole `sanitize` exists for: `ESC[31mFAKE ERROR ESC[0m` becomes the plain
    // text "FAKE ERROR", with no colour and no cursor control - never passed
    // through, which would reintroduce it.
    #[test]
    fn removes_a_colour_sequence_entirely() {
        assert_eq!(sanitize("\u{1b}[37mSyncing"), "Syncing");
    }

    // The naive "consume until 'm'" approach mis-handles a final byte that
    // isn't 'm' (CSI's final byte is any of 0x40..=0x7E) - ESC[2J (clear
    // screen) has final byte 'J'.
    #[test]
    fn handles_a_non_m_final_byte() {
        assert_eq!(sanitize("a\u{1b}[2Jb"), "ab");
    }

    #[test]
    fn removes_a_multi_parameter_sequence() {
        assert_eq!(sanitize("\u{1b}[1;31;40mX"), "X");
    }

    // A truncated sequence (no final byte before the string ends) must not
    // swallow the rest of the line - only the ESC becomes U+FFFD, and
    // everything after it is processed as plain text, same as before CSI
    // parsing was added.
    #[test]
    fn a_truncated_sequence_does_not_swallow_the_tail() {
        assert_eq!(sanitize("a\u{1b}[31"), "a\u{fffd}31");
    }

    // Only CSI (ESC `[`) is special-cased. OSC (ESC `]`, used for window
    // titles) keeps today's behaviour: the ESC becomes U+FFFD and the rest of
    // the sequence stays as inert literal text.
    #[test]
    fn osc_sequences_keep_the_old_behaviour() {
        assert_eq!(sanitize("\u{1b}]0;title\u{7}"), "\u{fffd}]0;title\u{fffd}");
    }

    #[test]
    fn a_bare_escape_becomes_the_replacement_character() {
        assert_eq!(sanitize("a\u{1b}b"), "a\u{fffd}b");
    }

    #[test]
    fn tab_and_newline_still_survive() {
        assert_eq!(sanitize("a\tb\nc"), "a\tb\nc");
    }

    // The actual line captured from a live, syncing Teku node under Docker.
    #[test]
    fn a_real_captured_teku_line_renders_cleanly() {
        let line =
            "2026-09-14 03:37:48.006 INFO  - \u{1b}[37mSyncing     *** Slot: 3928660\u{1b}[0m";
        let clean = sanitize(line);
        assert!(!clean.contains('\u{fffd}'), "got: {clean:?}");
        assert!(!clean.contains('\u{1b}'), "got: {clean:?}");
    }
}
