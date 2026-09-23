use crate::term::sanitize;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde_json::Value;

fn color_for_level(level: &str) -> &'static str {
    match level {
        "ERROR" => "31",
        "WARN" => "33",
        "INFO" => "32",
        "DEBUG" => "36",
        _ => "0",
    }
}

/// The levels `parse_console` will accept in a line's level field.
///
/// TRACE and FATAL are accepted here but have no entry in `color_for_level`, so
/// they render uncoloured - the same treatment TRACE already gets on the JSON
/// path.
const LEVELS: [&str; 6] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE", "FATAL"];

/// A log timestamp as milliseconds since the Unix epoch.
///
/// One integer rather than a date type, because the only thing anything does
/// with it is compare it against another one and occasionally add a day.
/// Adding a date dependency to a single-binary CLI to do that would be a poor
/// trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogTime(pub i64);

/// What `parse_timestamp` could recover from a line.
///
/// Teku's console layout has a time-only variant with no date at all, so a
/// parsed timestamp is not always locatable on its own. Dating a `TimeOfDay`
/// needs the source's recent history, which `merge.rs` keeps - resolving it
/// here would mean guessing with less information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    Absolute(LogTime),
    TimeOfDay(i64),
}

/// Milliseconds since midnight from `HH:MM:SS` or `HH:MM:SS.mmm`.
///
/// The separator before the milliseconds may be a comma: Teku's JSON layout
/// writes `19:17:51,172`, which is what log4j2's `%d{DEFAULT}` produces, while
/// its console layout writes a dot. Both are real and both arrive here.
fn time_of_day_ms(s: &str) -> Option<i64> {
    let (hms, millis) = match s.split_once(['.', ',']) {
        Some((hms, ms)) => {
            if ms.len() > 3 || ms.is_empty() || !ms.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // `.21` means 210ms, not 21ms.
            let scaled: i64 = ms.parse::<i64>().ok()? * 10_i64.pow(3 - ms.len() as u32);
            (hms, scaled)
        }
        None => (s, 0),
    };

    let mut parts = hms.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some(h * 3_600_000 + m * 60_000 + sec * 1_000 + millis)
}

/// `YYYY-MM-DD` to days since the Unix epoch.
fn date_days(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(crate::dump::days_from_civil(y, m, d))
}

/// The timestamp of one log line, in whichever of Teku's three layouts it
/// arrived.
///
/// | Layout | Shape | Zone |
/// | --- | --- | --- |
/// | JSON, bare-metal | `2026-09-17T19:17:51,172` | none stated |
/// | JSON, ECS template | `2026-09-01T10:00:00.000Z` | UTC |
/// | Console, both Docker stacks | `2026-09-14 01:16:54.217` | none stated |
/// | Console, time-only | `01:16:54.217` | none stated, no date |
///
/// The first row is what a stock bare-metal Teku writes, taken off a real
/// node: a comma before the milliseconds and no zone at all. The `Z`-suffixed
/// form belongs to log4j2's ECS template, which this table once claimed was
/// the bare-metal shape - it is not, and assuming so cost `merge.rs` every
/// line of a bare-metal beacon node.
///
/// **Only the ECS form states a timezone; everything else is taken at face
/// value.** If a beacon node logs local time while a validator container runs
/// a non-UTC `TZ`, a merge across the two is wrong by that offset. Containers
/// default to UTC and servers usually run UTC, so this is a documented
/// limitation rather than a correction; the fix, if a real node shows it, is
/// to estimate a per-source offset from arrival times.
///
/// Returning `None` is meaningful rather than a failure: a line with no
/// timestamp is a stack trace's continuation, and `merge.rs` uses exactly
/// this to keep a trace attached to the line that introduced it.
pub fn parse_timestamp(s: &str) -> Option<Stamp> {
    let s = s.trim_end_matches('Z');
    if let Some((date, time)) = s.split_once(['T', ' ']) {
        return Some(Stamp::Absolute(LogTime(
            date_days(date)? * 86_400_000 + time_of_day_ms(time)?,
        )));
    }
    time_of_day_ms(s).map(Stamp::TimeOfDay)
}

const DAY_MS: i64 = 86_400_000;

/// The zone `tekops logs --tz` renders timestamps in.
///
/// Every stamp is read as UTC. That is the premise of the flag: tekops
/// recommends logging UTC, and `--tz` is how an operator reads such a log in
/// their own time. A node already logging local time would be shifted twice,
/// and nothing in the line can tell tekops that it was.
///
/// `today` is the zone's offset at session start, in milliseconds, and is only
/// for the time-only console layout: with no date there is no instant to look
/// an offset up by, so a session that crosses a DST change keeps rendering
/// those lines at the offset it started with.
pub struct DisplayZone {
    tz: TimeZone,
    today: i64,
}

impl DisplayZone {
    pub fn new(tz: TimeZone, now: Timestamp) -> Self {
        let today = i64::from(tz.to_offset(now).seconds()) * 1_000;
        Self { tz, today }
    }
}

/// `s` rewritten into `zone`, in the same shape it arrived in, with the offset
/// appended (replacing a `Z` if it had one).
///
/// Same shape means the same date/time separator, the same millisecond
/// separator and the same number of millisecond digits, so a converted line
/// reads like the node wrote it. The offset is what keeps it honest: an
/// unsuffixed local time would sit in the pager looking exactly like the UTC
/// stamps the node writes.
///
/// `None` for anything `parse_timestamp` does not accept, which the caller
/// leaves untouched rather than guessing at.
fn restamp(s: &str, zone: &DisplayZone) -> Option<String> {
    let body = s.strip_suffix('Z').unwrap_or(s);
    let (date_sep, time) = match body.split_once(['T', ' ']) {
        Some((date, time)) => (Some(&body[date.len()..=date.len()]), time),
        None => (None, body),
    };
    let millis = time
        .split_once(['.', ','])
        .map(|(hms, ms)| (&time[hms.len()..=hms.len()], ms.len()));

    let (days, ms_of_day, offset) = match parse_timestamp(s)? {
        Stamp::Absolute(LogTime(ms)) => {
            let at = Timestamp::from_millisecond(ms).ok()?;
            let offset = i64::from(zone.tz.to_offset(at).seconds()) * 1_000;
            let local = ms + offset;
            (
                Some(local.div_euclid(DAY_MS)),
                local.rem_euclid(DAY_MS),
                offset,
            )
        }
        Stamp::TimeOfDay(ms) => (None, (ms + zone.today).rem_euclid(DAY_MS), zone.today),
    };

    let mut out = String::new();
    if let (Some(days), Some(sep)) = (days, date_sep) {
        let (y, m, d) = crate::dump::civil_from_days(days);
        out.push_str(&format!("{y:04}-{m:02}-{d:02}{sep}"));
    }
    let secs = ms_of_day / 1_000;
    out.push_str(&format!(
        "{:02}:{:02}:{:02}",
        secs / 3_600,
        secs / 60 % 60,
        secs % 60
    ));
    if let Some((sep, width)) = millis {
        let ms = format!("{:03}", ms_of_day % 1_000);
        out.push_str(sep);
        out.push_str(&ms[..width]);
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let off_min = offset.abs() / 60_000;
    out.push_str(&format!("{sign}{:02}:{:02}", off_min / 60, off_min % 60));
    Some(out)
}

/// One log record's fields, however they were parsed.
///
/// `middle` is the already-rendered `[thread] class ` segment (or empty),
/// decided by the parser rather than by `render`. The JSON layout always
/// carries a thread and a class, so `parse_json` builds `middle`
/// unconditionally; Teku's console layout carries neither, so `parse_console`
/// leaves it empty. Deciding this in `render` instead - e.g. omitting the
/// brackets when a field happens to be empty - is what let the JSON path
/// silently drift from its pre-console-parser output; putting the decision in
/// the parser makes the JSON path byte-identical by construction instead of by
/// convention.
struct Fields {
    timestamp: String,
    level: String,
    middle: String,
    message: String,
    throwable: String,
}

/// Whether a string has the shape of one of Teku's console timestamps,
/// `2026-09-14 01:16:54.217` or the time-only `01:16:54.217`.
///
/// This is the second of `parse_console`'s two guards, and it is not optional:
/// the level check alone would happily accept `some text INFO - hello`.
fn looks_like_timestamp(s: &str) -> bool {
    !s.is_empty()
        && s.contains(':')
        && s.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | ' '))
}

/// Teku's console layout, which is what both Eth Docker and Rocket Pool run it
/// with (`--log-destination=CONSOLE`):
///
/// ```text
/// %d{yyyy-MM-dd HH:mm:ss.SSS} %-5level - %msg%n
/// 2026-09-14 01:16:54.217 INFO  - Teku version: teku/v26.7.1
/// ```
///
/// Three fields, not five: there is no thread and no class. Teku's five-field
/// pipe-delimited `FILE_MESSAGE_FORMAT` belongs to the *file* appender and is
/// deliberately not parsed here - no deployment this targets emits it, and Teku
/// puts literal pipes inside its own messages (`Configuration | Network: hoodi`),
/// so a pipe splitter would both miss every real line and mangle those.
///
/// Splitting on the *first* `" - "` is what keeps a message containing one
/// whole, since the header never contains a `" - "`. `%-5level` right-pads, so
/// `INFO` arrives padded and `ERROR` does not; trimming the head handles both.
fn parse_console(raw: &str) -> Option<Fields> {
    let (head, message) = raw.split_once(" - ")?;
    let (timestamp, level) = head.trim_end().rsplit_once(' ')?;

    if !LEVELS.contains(&level) || !looks_like_timestamp(timestamp) {
        return None;
    }

    Some(Fields {
        timestamp: sanitize(timestamp),
        level: sanitize(level),
        // No thread, no class in this layout - see the note on `Fields::middle`.
        middle: String::new(),
        message: sanitize(message),
        // The console layout has no throwable field. Log4j appends stack traces
        // as separate physical lines, which reach `format_log_line` on their own
        // and fall through to passthrough.
        throwable: String::new(),
    })
}

/// The timestamp field of a JSON log line, under either of the two names it
/// is written with.
///
/// Teku's own JSON layout calls it `timestamp`; `@timestamp` is what log4j2's
/// ECS and Logstash templates emit, and what a node configured with one of
/// those writes. This module looked only for `@timestamp`, so a stock
/// bare-metal Teku - which writes `timestamp` - rendered every line with an
/// empty timestamp column, and `merge::leading_timestamp`, which shares this
/// lookup, read every line as having no timestamp at all. That second one was
/// not cosmetic: see the note on `RecordBuilder::push_line`.
///
/// `timestamp` is checked first because it is Teku's own; a line carrying
/// both is a template that added one, and the node's own field is the one to
/// trust.
pub fn json_timestamp(value: &Value) -> Option<&str> {
    ["timestamp", "@timestamp"]
        .into_iter()
        .find_map(|k| value.get(k).and_then(Value::as_str))
}

/// Log fields carry data the node didn't author - peer identifiers, remote
/// agent strings, exception text from malformed gossip. The colorized output is
/// written to a temp file that `less -R` renders with escapes live, so an
/// unsanitized field can clear the operator's screen, retitle the terminal, or
/// forge a red ERROR line that appears to come from tekops itself.
fn parse_json(value: &Value) -> Fields {
    let get = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(sanitize)
            .unwrap_or_default()
    };
    let thread = get("thread");
    let class = get("class");
    Fields {
        timestamp: json_timestamp(value).map(sanitize).unwrap_or_default(),
        level: get("level"),
        // Built unconditionally, brackets and all, even when thread/class are
        // empty - see the note on `Fields::middle` for why this can't move
        // into `render`.
        middle: format!("[{thread}] {class} "),
        message: get("message"),
        throwable: get("throwable"),
    }
}

/// Renders one parsed record. `f.middle` is already whatever the parser
/// decided it should be (see `Fields::middle`) - `render` just interpolates
/// it, with no knowledge of thread/class at all, so it cannot reintroduce the
/// drift that putting that decision here once caused.
fn render(f: &Fields) -> String {
    let color = color_for_level(&f.level);
    let middle = &f.middle;
    let throwable_suffix = if f.throwable.is_empty() {
        String::new()
    } else {
        format!("\n{}", f.throwable)
    };
    format!(
        "\u{1b}[{color}m{} {} {middle}- {}{throwable_suffix}\u{1b}[0m",
        f.timestamp, f.level, f.message
    )
}

/// Tries each known layout in turn, most specific first, and falls back to
/// passing the raw line through sanitized.
///
/// JSON is attempted before the console layout because it is unambiguous: a
/// line that parses as JSON is JSON. The console attempt is guarded by its two
/// checks so it cannot claim arbitrary text, and anything neither parser
/// accepts is still untrusted bytes headed for a terminal, so it gets the same
/// sanitizing.
///
/// With a `zone`, a parsed record's timestamp is rewritten into it (see
/// `restamp`). A passthrough line is never touched: it has no timestamp field,
/// and rewriting digits inside free text would be a guess.
pub fn format_log_line(raw: &str, zone: Option<&DisplayZone>) -> String {
    let mut fields = if let Ok(value) = serde_json::from_str::<Value>(raw) {
        parse_json(&value)
    } else if let Some(fields) = parse_console(raw) {
        fields
    } else {
        return sanitize(raw);
    };
    if let Some(stamp) = zone.and_then(|z| restamp(&fields.timestamp, z)) {
        fields.timestamp = stamp;
    }
    render(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line copied verbatim off a bare-metal node. Teku's own JSON layout
    /// names the field `timestamp` and separates the milliseconds with a
    /// comma; this module knew only `@timestamp` and a dot, so it rendered
    /// every one of these lines with an empty timestamp column.
    #[test]
    fn the_real_bare_metal_json_line_keeps_its_timestamp() {
        let raw = r#"{"timestamp":"2026-09-17T19:17:51,172","host":"validator","level":"INFO","thread":"TimeTickTask","class":"teku-event-log","message":"Slot Event  *** Slot: 15233787","throwable":""}"#;
        let out = format_log_line(raw, None);
        assert!(out.contains("2026-09-17T19:17:51,172"), "{out}");
        assert!(out.contains("Slot Event"), "{out}");
        assert!(!out.contains("  INFO"), "no empty timestamp column: {out}");
    }

    /// The other spelling still works: `@timestamp` is what log4j2's ECS and
    /// Logstash templates write, and a node configured with one emits it.
    #[test]
    fn the_ecs_timestamp_field_is_still_read() {
        let value: Value =
            serde_json::from_str(r#"{"@timestamp":"2026-09-01T10:00:00.000Z"}"#).expect("json");
        assert_eq!(json_timestamp(&value), Some("2026-09-01T10:00:00.000Z"));
    }

    /// Teku's own field wins when a template has added the other.
    #[test]
    fn tekus_own_timestamp_field_wins_over_an_added_one() {
        let value: Value =
            serde_json::from_str(r#"{"timestamp":"a","@timestamp":"b"}"#).expect("json");
        assert_eq!(json_timestamp(&value), Some("a"));
    }

    #[test]
    fn milliseconds_parse_after_a_comma_as_well_as_a_dot() {
        assert_eq!(
            parse_timestamp("19:17:51,172"),
            parse_timestamp("19:17:51.172")
        );
    }

    #[test]
    fn formats_info_line_green() {
        let raw = r#"{"@timestamp":"2026-09-01T10:00:00.000Z","level":"INFO","thread":"main","class":"Node","message":"Started"}"#;
        let out = format_log_line(raw, None);
        assert!(
            out.starts_with("\u{1b}[32m"),
            "expected green color code, got: {out}"
        );
        assert!(out.contains("2026-09-01T10:00:00.000Z INFO [main] Node - Started"));
        assert!(out.ends_with("\u{1b}[0m"));
    }

    #[test]
    fn formats_error_line_red_with_throwable() {
        let raw = r#"{"@timestamp":"t","level":"ERROR","thread":"t1","class":"C","message":"boom","throwable":"java.lang.RuntimeException: boom\n\tat C.run"}"#;
        let out = format_log_line(raw, None);
        assert!(out.starts_with("\u{1b}[31m"));
        assert!(out.contains("t ERROR [t1] C - boom\njava.lang.RuntimeException: boom"));
    }

    #[test]
    fn formats_warn_yellow_and_debug_cyan() {
        let warn = r#"{"@timestamp":"t","level":"WARN","thread":"t1","class":"C","message":"m"}"#;
        assert!(format_log_line(warn, None).starts_with("\u{1b}[33m"));

        let debug = r#"{"@timestamp":"t","level":"DEBUG","thread":"t1","class":"C","message":"m"}"#;
        assert!(format_log_line(debug, None).starts_with("\u{1b}[36m"));
    }

    #[test]
    fn unknown_level_has_no_color_code() {
        let raw = r#"{"@timestamp":"t","level":"TRACE","thread":"t1","class":"C","message":"m"}"#;
        let out = format_log_line(raw, None);
        assert!(out.starts_with("\u{1b}[0m"));
    }

    #[test]
    fn missing_throwable_has_no_extra_line() {
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"t1","class":"C","message":"m"}"#;
        let out = format_log_line(raw, None);
        assert_eq!(out.matches('\n').count(), 0);
    }

    #[test]
    fn malformed_json_passes_through_unchanged() {
        let raw = "not json at all";
        assert_eq!(format_log_line(raw, None), "not json at all");
    }

    #[test]
    fn strips_terminal_escapes_from_log_fields() {
        // A peer-supplied string reaching Teku's logs: clear-screen, retitle
        // the window, then forge what looks like a tekops-emitted ERROR line.
        // `\u001b` keeps this valid JSON: raw control bytes inside a JSON
        // string are invalid, and would silently divert this to the
        // malformed-line path instead of the parsed-field path under test.
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"main","class":"P2P","message":"peer: \u001b[2J\u001b]0;PWNED\u0007\u001b[31mFAKE ERROR\u001b[0m"}"#;
        let out = format_log_line(raw, None);

        // Exactly two escapes survive: the ones tekops itself wraps the line in.
        assert_eq!(
            out.matches('\u{1b}').count(),
            2,
            "field escapes leaked: {out:?}"
        );
        assert!(out.starts_with("\u{1b}[32m"));
        assert!(out.ends_with("\u{1b}[0m"));
        assert!(!out.contains('\u{7}'), "BEL leaked: {out:?}");
        assert!(
            out.contains("FAKE ERROR"),
            "text should survive, only control chars go"
        );
    }

    #[test]
    fn strips_terminal_escapes_from_malformed_lines_too() {
        let out = format_log_line("garbage \u{1b}[2J more", None);
        assert!(
            !out.contains('\u{1b}'),
            "escape leaked via the passthrough path: {out:?}"
        );
    }

    #[test]
    fn keeps_newlines_in_java_stack_traces() {
        let raw = r#"{"@timestamp":"t","level":"ERROR","thread":"t1","class":"C","message":"boom","throwable":"java.lang.RuntimeException\n\tat C.run"}"#;
        let out = format_log_line(raw, None);
        assert!(out.contains("java.lang.RuntimeException\n\tat C.run"));
    }

    /// Captured verbatim from a real `consensys/teku:latest` container.
    #[test]
    fn formats_teku_console_lines_with_the_same_colors_as_json() {
        let raw = "2026-09-14 01:16:54.217 INFO  - Teku version: teku/v26.7.1";
        let out = format_log_line(raw, None);
        assert!(
            out.starts_with("\u{1b}[32m"),
            "expected green, got: {out:?}"
        );
        assert!(
            out.contains("2026-09-14 01:16:54.217 INFO - Teku version: teku/v26.7.1"),
            "got: {out:?}"
        );
        assert!(out.ends_with("\u{1b}[0m"));
    }

    /// ERROR is already 5 chars so `%-5level` adds no padding, giving one space
    /// before the dash where INFO gives two. Both are real, both must parse.
    #[test]
    fn console_handles_both_padded_and_unpadded_levels() {
        let unpadded = "2026-09-14 01:17:07.062 ERROR - Failed to update fork choice";
        let out = format_log_line(unpadded, None);
        assert!(out.starts_with("\u{1b}[31m"), "got: {out:?}");
        assert!(out.contains("01:17:07.062 ERROR - Failed to update fork choice"));

        let padded = "2026-09-14 01:17:12.652 WARN  - Syncing started";
        assert!(format_log_line(padded, None).starts_with("\u{1b}[33m"));
    }

    /// The no-date variant of the console format, for a node started with
    /// date prepending disabled.
    #[test]
    fn console_parses_the_time_only_timestamp_variant() {
        let raw = "01:16:54.217 INFO  - Started";
        let out = format_log_line(raw, None);
        assert!(out.starts_with("\u{1b}[32m"), "got: {out:?}");
        assert!(out.contains("01:16:54.217 INFO - Started"));
    }

    /// Teku puts literal pipes in its own messages. This is the line that makes
    /// a pipe-splitting parser wrong, so it is pinned as a test.
    #[test]
    fn console_message_containing_a_pipe_is_not_mangled() {
        let raw =
            "2026-09-14 01:16:54.282 INFO  - Configuration | Network: hoodi, Storage Mode: MINIMAL";
        let out = format_log_line(raw, None);
        assert!(
            out.contains("INFO - Configuration | Network: hoodi, Storage Mode: MINIMAL"),
            "got: {out:?}"
        );
    }

    /// Splitting on the FIRST " - " is what keeps a message containing one
    /// whole, since the header never contains a " - ".
    #[test]
    fn console_message_may_contain_the_separator() {
        let raw = "2026-09-14 01:16:54.217 INFO  - peer said a - b - c";
        let out = format_log_line(raw, None);
        assert!(out.contains("INFO - peer said a - b - c"), "got: {out:?}");
    }

    /// The level guard alone would accept this; the timestamp-shape guard is
    /// what rejects it. Both are needed.
    #[test]
    fn prose_that_merely_contains_a_level_word_is_not_a_log_record() {
        assert_eq!(
            format_log_line("some text INFO - hello", None),
            "some text INFO - hello"
        );
    }

    #[test]
    fn a_line_with_no_separator_falls_through_to_passthrough() {
        assert_eq!(
            format_log_line("just some output", None),
            "just some output"
        );
    }

    /// Java stack traces arrive as continuation lines. They stay readable via
    /// passthrough rather than being mangled.
    #[test]
    fn console_stack_trace_continuation_lines_pass_through() {
        let raw = "\tat tech.pegasys.teku.Foo.run(Foo.java:42)";
        assert_eq!(
            format_log_line(raw, None),
            "\tat tech.pegasys.teku.Foo.run(Foo.java:42)"
        );
    }

    /// Same threat as the JSON path: message text carries remote-authored data
    /// and lands in a file `less -R` renders with escapes live. Teku's own event
    /// colouring arrives this way too, so this path is exercised constantly.
    #[test]
    fn strips_terminal_escapes_from_console_messages() {
        let raw = "2026-09-14 01:16:54.217 INFO  - peer: \u{1b}[2J\u{1b}]0;PWNED\u{7}\u{1b}[31mFAKE\u{1b}[0m";
        let out = format_log_line(raw, None);
        assert_eq!(
            out.matches('\u{1b}').count(),
            2,
            "field escapes leaked: {out:?}"
        );
        assert!(!out.contains('\u{7}'), "BEL leaked: {out:?}");
        assert!(out.contains("FAKE"), "text should survive: {out:?}");
    }

    /// The renderer is now shared between the JSON and console paths, so this
    /// pins that sharing it did not change the JSON output.
    #[test]
    fn json_rendering_is_unchanged_by_the_shared_renderer() {
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"main","class":"Node","message":"Started"}"#;
        let out = format_log_line(raw, None);
        assert!(out.contains("t INFO [main] Node - Started"), "got: {out:?}");
    }

    /// JSON is tried first, so a JSON line whose values contain " - " still
    /// takes the JSON path.
    #[test]
    fn json_still_wins_over_console_parsing() {
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"a - b","class":"C","message":"m"}"#;
        let out = format_log_line(raw, None);
        assert!(out.contains("[a - b] C - m"), "got: {out:?}");
    }

    /// Pins the JSON path's byte-identity in the corner the shared renderer
    /// could have changed: an empty thread still renders as empty brackets,
    /// exactly as it did before the console parser existed.
    #[test]
    fn json_with_an_empty_thread_still_renders_empty_brackets() {
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"","class":"C","message":"m"}"#;
        let out = format_log_line(raw, None);
        assert!(out.contains("t INFO [] C - m"), "got: {out:?}");
    }

    /// The same corner with the key absent rather than empty.
    #[test]
    fn json_with_no_thread_key_still_renders_empty_brackets() {
        let raw = r#"{"@timestamp":"t","level":"INFO","class":"C","message":"m"}"#;
        let out = format_log_line(raw, None);
        assert!(out.contains("t INFO [] C - m"), "got: {out:?}");
    }

    /// The three shapes Teku emits, normalised to one comparable value. JSON is
    /// what a bare-metal node writes; both Docker stacks run Teku with
    /// `--log-destination=CONSOLE`, whose timestamp comes with or without a date.
    #[test]
    fn parses_all_three_timestamp_layouts() {
        // 2026-09-01T10:00:00.000Z = 20697 days since the epoch, plus 10h.
        let json = parse_timestamp("2026-09-01T10:00:00.000Z").unwrap();
        assert_eq!(
            json,
            Stamp::Absolute(LogTime(
                crate::dump::days_from_civil(2026, 9, 1) * 86_400_000 + 10 * 3_600_000
            ))
        );

        let console = parse_timestamp("2026-09-14 01:16:54.217").unwrap();
        assert_eq!(
            console,
            Stamp::Absolute(LogTime(
                crate::dump::days_from_civil(2026, 9, 14) * 86_400_000
                    + 3_600_000
                    + 16 * 60_000
                    + 54_000
                    + 217
            ))
        );

        let time_only = parse_timestamp("01:16:54.217").unwrap();
        assert_eq!(
            time_only,
            Stamp::TimeOfDay(3_600_000 + 16 * 60_000 + 54_000 + 217)
        );
    }

    /// Anything that is not one of the three shapes has no timestamp, which is
    /// how a stack trace's continuation lines are recognised.
    #[test]
    fn rejects_anything_that_is_not_a_timestamp() {
        for s in [
            "",
            "t",
            "garbage",
            "2026-09-14",
            "at tech.pegasys.teku.Foo.bar(Foo.java:42)",
        ] {
            assert!(parse_timestamp(s).is_none(), "accepted {s:?}");
        }
    }

    /// Milliseconds are optional in the wild; a timestamp without them must not
    /// be silently rejected into passthrough.
    #[test]
    fn milliseconds_are_optional() {
        assert_eq!(
            parse_timestamp("01:16:54"),
            Some(Stamp::TimeOfDay(3_600_000 + 16 * 60_000 + 54_000))
        );
    }

    fn fixed(hours: i8, minutes: i8) -> DisplayZone {
        let secs = i32::from(hours) * 3_600 + i32::from(minutes) * 60;
        let tz = TimeZone::fixed(jiff::tz::Offset::from_seconds(secs).expect("offset"));
        DisplayZone::new(tz, Timestamp::UNIX_EPOCH)
    }

    fn stamp_of(raw: &str, zone: &DisplayZone) -> String {
        let out = format_log_line(raw, Some(zone));
        out.trim_start_matches(|c| c != 'm')[1..]
            .split(' ')
            .next()
            .expect("a timestamp")
            .to_string()
    }

    /// The real bare-metal line again: comma millis, no zone, read as UTC.
    #[test]
    fn tz_rewrites_the_bare_metal_json_stamp_in_its_own_shape() {
        let raw = r#"{"timestamp":"2026-09-17T19:17:51,172","level":"INFO","thread":"T","class":"C","message":"m"}"#;
        assert_eq!(
            stamp_of(raw, &fixed(12, 0)),
            "2026-09-18T07:17:51,172+12:00"
        );
    }

    /// The `Z` states UTC, which the offset now states instead.
    #[test]
    fn tz_replaces_a_z_suffix_with_the_offset() {
        let raw = r#"{"@timestamp":"2026-09-01T10:00:00.000Z","level":"INFO","thread":"T","class":"C","message":"m"}"#;
        assert_eq!(
            stamp_of(raw, &fixed(12, 0)),
            "2026-09-01T22:00:00.000+12:00"
        );
    }

    #[test]
    fn tz_moves_the_date_back_across_midnight_west_of_utc() {
        let raw = "2026-09-14 01:16:54.217 INFO  - Started";
        let out = format_log_line(raw, Some(&fixed(-5, 0)));
        assert!(
            out.contains("2026-09-13 20:16:54.217-05:00 INFO - Started"),
            "{out:?}"
        );
    }

    #[test]
    fn tz_handles_half_hour_offsets_on_both_sides() {
        let raw = "2026-09-14 01:16:54.217 INFO  - m";
        let out = format_log_line(raw, Some(&fixed(5, 30)));
        assert!(
            out.contains("2026-09-14 06:46:54.217+05:30 INFO"),
            "{out:?}"
        );
        let out = format_log_line(raw, Some(&fixed(-3, -30)));
        assert!(
            out.contains("2026-09-13 21:46:54.217-03:30 INFO"),
            "{out:?}"
        );
    }

    /// No date means no instant to look an offset up by, so the session's
    /// offset is used and the time wraps around the clock.
    #[test]
    fn tz_shifts_a_time_only_stamp_by_the_session_offset() {
        let raw = "13:16:54.217 INFO  - m";
        let out = format_log_line(raw, Some(&fixed(12, 0)));
        assert!(out.contains("01:16:54.217+12:00 INFO - m"), "{out:?}");
    }

    /// The offset is looked up per line, so one session renders both sides of
    /// a DST change correctly.
    #[test]
    fn tz_applies_each_instants_own_dst_offset() {
        let tz = TimeZone::posix("NZST-12NZDT,M9.5.0,M4.1.0/3").expect("posix tz");
        let zone = DisplayZone::new(tz, Timestamp::UNIX_EPOCH);
        let winter = "2026-07-01 00:00:00.000 INFO  - m";
        let summer = "2026-12-01 00:00:00.000 INFO  - m";
        assert!(format_log_line(winter, Some(&zone)).contains("2026-07-01 12:00:00.000+12:00"));
        assert!(format_log_line(summer, Some(&zone)).contains("2026-12-01 13:00:00.000+13:00"));
    }

    /// The time-only layout takes the offset in force when the session began.
    #[test]
    fn tz_time_only_offset_comes_from_session_start() {
        let tz = TimeZone::posix("NZST-12NZDT,M9.5.0,M4.1.0/3").expect("posix tz");
        let december = Timestamp::from_second(1_796_083_200).expect("2026-12-01");
        let zone = DisplayZone::new(tz, december);
        let out = format_log_line("00:00:00 INFO  - m", Some(&zone));
        assert!(out.contains("13:00:00+13:00 INFO"), "{out:?}");
    }

    /// Absent milliseconds stay absent and a short fraction keeps its width.
    #[test]
    fn tz_keeps_the_millisecond_width_it_was_given() {
        let zone = fixed(1, 0);
        assert!(
            format_log_line("2026-09-14 01:16:54 INFO  - m", Some(&zone))
                .contains("2026-09-14 02:16:54+01:00 INFO")
        );
        assert!(
            format_log_line("2026-09-14 01:16:54.21 INFO  - m", Some(&zone))
                .contains("2026-09-14 02:16:54.21+01:00 INFO")
        );
    }

    /// A timestamp field tekops cannot read is shown as it came, not dropped.
    #[test]
    fn tz_leaves_an_unparseable_stamp_alone() {
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"main","class":"Node","message":"Started"}"#;
        let out = format_log_line(raw, Some(&fixed(12, 0)));
        assert!(out.contains("t INFO [main] Node - Started"), "{out:?}");
    }

    /// Free text is never rewritten, even when it starts with a time.
    #[test]
    fn tz_does_not_touch_passthrough_lines() {
        let raw = "01:16:54 something that is not a record";
        assert_eq!(format_log_line(raw, Some(&fixed(12, 0))), raw);
    }

    /// `days_from_civil` is the inverse of the `civil_from_days` already in
    /// dump.rs. Round-tripping is what pins it.
    #[test]
    fn days_from_civil_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2026, 9, 17), (2000, 2, 29), (1999, 12, 31)] {
            let days = crate::dump::days_from_civil(y, m, d);
            assert_eq!(crate::dump::civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
    }
}
