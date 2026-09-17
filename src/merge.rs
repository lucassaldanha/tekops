//! Ordering two log sources into one timeline.
//!
//! Pure: no threads, no IO and no clock of its own - every instant arrives as
//! a parameter. The same split `stack::detect_stack` and `logs::producer_argv`
//! use, and for the same reason: the ordering rule is the part that is easy to
//! get subtly wrong and impossible to debug from a live tail, so it is
//! testable without spawning anything.
//!
//! `logs.rs` owns the threads that feed this.

use crate::logfmt::{parse_timestamp, LogTime, Stamp};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a record waits for the other source to speak before it is emitted.
///
/// Correct ordering needs a line held long enough to see whether the other
/// stream has an earlier one. A quarter second is imperceptible when watching
/// logs and covers ordinary producer jitter - `docker logs` in particular
/// delivers in batches rather than line by line.
///
/// Deliberately not a flag. A knob here would be one more thing to get wrong
/// in exchange for a number nobody can pick better than this one.
pub const MERGE_WINDOW: Duration = Duration::from_millis(250);

/// Which process a record came from.
///
/// No `#[allow(dead_code)]` here: `resolve_log_sources`'s rule 3 pattern-matches
/// both variants (`Some(Source::Bn) => ...`, `Some(Source::Vc) => ...` on
/// `inputs.select`), which is reachable from `main` via `run_logs`, and a
/// pattern match on a variant counts as a use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Bn,
    Vc,
}

impl Source {
    /// The tag printed at the head of the line. Matches the `bn`/`vc`
    /// vocabulary of `--bn-metric-url` and `--vc-metric-url`, so the operator
    /// reads the same two words everywhere.
    ///
    /// No `#[allow(dead_code)]` here: `cli.rs::check_selection` calls this to
    /// build its error message, and that path is reachable from `main`.
    pub fn tag(&self) -> &'static str {
        match self {
            Source::Bn => "bn",
            Source::Vc => "vc",
        }
    }
}

/// One timestamped line plus the untimestamped lines that follow it.
///
/// The merge unit is a record and not a line, because Teku logs a stack trace
/// as one timestamped line followed by continuation lines carrying no
/// timestamp of their own. Sorting lines would scatter a trace through the
/// other source's output; grouping first is what prevents it. This is not an
/// optimisation - a line-level merge is wrong on any node that logs an
/// exception.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub source: Source,
    pub time: LogTime,
    pub lines: Vec<String>,
}

const DAY_MS: i64 = 86_400_000;

/// Groups one source's raw lines into records and gives every record a
/// comparable timestamp.
///
/// Holds the per-source state that dating a timestamp needs: the last time
/// seen (for monotonicity and for rollover) and the last date seen (for the
/// console layout's time-only variant).
pub struct RecordBuilder {
    source: Source,
    /// Where an undated record lands when this source has never carried a
    /// date - the session's start, supplied by the caller so this stays pure.
    session_start_ms: i64,
    last: Option<LogTime>,
    pending: Option<Record>,
}

impl RecordBuilder {
    pub fn new(source: Source, session_start_ms: i64) -> Self {
        RecordBuilder {
            source,
            session_start_ms,
            last: None,
            pending: None,
        }
    }

    /// Feeds one raw line, returning the record it completed, if any.
    ///
    /// A line with a timestamp starts a new record and closes the previous
    /// one. A line without one is a continuation and joins the record in
    /// progress.
    pub fn push_line(&mut self, raw: &str) -> Option<Record> {
        let Some(stamp) = leading_timestamp(raw) else {
            // A continuation. With no record in progress it is still output
            // and must not be dropped, so it opens one of its own at whatever
            // time this source last reached.
            match &mut self.pending {
                Some(p) => p.lines.push(raw.to_string()),
                None => {
                    self.pending = Some(Record {
                        source: self.source,
                        time: self.last.unwrap_or(LogTime(self.session_start_ms)),
                        lines: vec![raw.to_string()],
                    })
                }
            }
            return None;
        };

        let time = self.resolve(stamp);
        self.last = Some(time);
        self.pending.replace(Record {
            source: self.source,
            time,
            lines: vec![raw.to_string()],
        })
    }

    /// The record still in progress, if there is one. Called at EOF.
    pub fn finish(&mut self) -> Option<Record> {
        self.pending.take()
    }

    /// Turns a parsed stamp into an absolute time on this source's timeline.
    ///
    /// Two jobs. A time-only stamp is dated from the last absolute time seen,
    /// rolling the date forward when the time of day goes backwards (which is
    /// midnight, not a clock change). And the result is clamped to be
    /// non-decreasing, because `Merger` compares queue heads and would
    /// silently misorder if a queue were not sorted - a clock change must cost
    /// one misplaced line, not a corrupted merge.
    fn resolve(&self, stamp: Stamp) -> LogTime {
        let candidate = match stamp {
            Stamp::Absolute(t) => t,
            Stamp::TimeOfDay(ms) => {
                let base = self.last.map(|t| t.0).unwrap_or(self.session_start_ms);
                let day = base.div_euclid(DAY_MS);
                let same_day = day * DAY_MS + ms;
                if same_day < base {
                    LogTime(same_day + DAY_MS)
                } else {
                    LogTime(same_day)
                }
            }
        };
        match self.last {
            Some(last) if candidate < last => last,
            _ => candidate,
        }
    }
}

/// The timestamp at the head of a raw line, in either layout.
///
/// JSON carries it in `@timestamp`; the console layout puts it first, before
/// the level. Anything else has none, which is what marks a continuation
/// line.
fn leading_timestamp(raw: &str) -> Option<Stamp> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        return value
            .get("@timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_timestamp);
    }
    // The console layout's head is `<timestamp> <LEVEL> - `, so the timestamp
    // is everything before the last space of the head. Splitting on " - "
    // first is what keeps a message containing a space-dash-space whole, the
    // same reasoning as `logfmt::parse_console`.
    let (head, _) = raw.split_once(" - ")?;
    let (timestamp, _level) = head.trim_end().rsplit_once(' ')?;
    parse_timestamp(timestamp)
}

/// Merges per-source record streams into one ordered stream.
///
/// Each source's own records are non-decreasing in time, because a log is
/// written in order and `RecordBuilder` clamps anything that is not. So only
/// the head of each queue matters.
///
/// The rule: **emit the earliest head when every other source either has a
/// head to compare against, or has been silent for the window, or is at
/// EOF.** A source with an empty queue that is neither idle nor finished may
/// still deliver something older, and emitting past it is exactly the
/// inversion this exists to prevent.
///
/// The same rule covers the `-n` backlog and live follow with no mode switch.
/// `tail -n 500` and `docker logs --tail 500` deliver their backlog as an
/// immediate burst, so both queues fill and the comparison sorts it exactly.
pub struct Merger {
    sources: Vec<Source>,
    queues: Vec<VecDeque<Record>>,
    last_push: Vec<Instant>,
    eof: Vec<bool>,
    window: Duration,
}

impl Merger {
    /// `started_at` seeds every source's idle clock.
    ///
    /// Taken in the constructor rather than set by a separate `start()` call
    /// precisely because it cannot then be forgotten: a source with no idle
    /// clock at all is never idle, so the other source's entire backlog would
    /// wait forever on one that has not spoken yet. That is a silent hang on
    /// a quiet node, which is the worst failure this type could have, so the
    /// type makes it unrepresentable instead of documenting it.
    pub fn new(sources: Vec<Source>, window: Duration, started_at: Instant) -> Self {
        let n = sources.len();
        Merger {
            sources,
            queues: vec![VecDeque::new(); n],
            last_push: vec![started_at; n],
            eof: vec![false; n],
            window,
        }
    }

    fn index(&self, source: Source) -> usize {
        self.sources
            .iter()
            .position(|s| *s == source)
            .expect("record from a source this merger was not built with")
    }

    pub fn push(&mut self, record: Record, at: Instant) {
        let i = self.index(record.source);
        self.queues[i].push_back(record);
        self.last_push[i] = at;
    }

    pub fn eof(&mut self, source: Source) {
        let i = self.index(source);
        self.eof[i] = true;
    }

    /// Whether every source has finished and everything queued has been
    /// emitted.
    pub fn is_done(&self) -> bool {
        self.eof.iter().all(|e| *e) && self.queues.iter().all(|q| q.is_empty())
    }

    /// Everything safe to emit as of `now`, in order.
    pub fn drain_ready(&mut self, now: Instant) -> Vec<Record> {
        let mut out = Vec::new();
        while let Some(i) = self.next_index(now) {
            out.push(self.queues[i].pop_front().expect("head was just checked"));
        }
        out
    }

    /// Everything queued, in order, ignoring the window. For EOF, where
    /// waiting cannot produce anything new.
    pub fn drain_all(&mut self) -> Vec<Record> {
        let mut out = Vec::new();
        while let Some(i) = self.earliest_head() {
            out.push(self.queues[i].pop_front().expect("head was just checked"));
        }
        out
    }

    /// The queue whose head is earliest. Ties break towards the source
    /// declared first, so output is stable run to run and a dump diffs
    /// against itself.
    fn earliest_head(&self) -> Option<usize> {
        self.queues
            .iter()
            .enumerate()
            .filter_map(|(i, q)| q.front().map(|r| (i, r.time)))
            .min_by_key(|(i, time)| (*time, *i))
            .map(|(i, _)| i)
    }

    /// The queue whose head may be emitted now, if any.
    fn next_index(&self, now: Instant) -> Option<usize> {
        let candidate = self.earliest_head()?;
        let safe = (0..self.sources.len()).all(|i| {
            i == candidate
                || !self.queues[i].is_empty()
                || self.eof[i]
                // A source that has not spoken since the session began is
                // idle once the window has passed from *that* point, which is
                // why `new` seeds the clock. Treating "never pushed" as idle
                // outright would emit one source's whole backlog before the
                // other had a chance to deliver its first line.
                || now.duration_since(self.last_push[i]) >= self.window
        });
        safe.then_some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(source: Source, ms: i64, text: &str) -> Record {
        Record {
            source,
            time: LogTime(ms),
            lines: vec![text.to_string()],
        }
    }

    fn texts(records: &[Record]) -> Vec<&str> {
        records
            .iter()
            .map(|r| r.lines[0].as_str())
            .collect::<Vec<_>>()
    }

    /// The basic promise: two sources, one timeline.
    ///
    /// Each source is pushed in its own increasing order, because that is the
    /// precondition `Merger` documents and `RecordBuilder` enforces - the
    /// head-only merge is only correct on sorted queues.
    #[test]
    fn emits_both_sources_in_timestamp_order() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 200, "bn-mid"), t0);
        m.push(rec(Source::Vc, 100, "vc-early"), t0);
        m.push(rec(Source::Bn, 300, "bn-late"), t0);
        m.eof(Source::Bn);
        m.eof(Source::Vc);

        assert_eq!(texts(&m.drain_all()), vec!["vc-early", "bn-mid", "bn-late"]);
    }

    /// The rule that makes ordering correct: a source that is neither idle nor
    /// at EOF may still deliver something older, so nothing may be emitted
    /// past it. Without this the merge degrades to arrival order.
    #[test]
    fn an_active_but_empty_source_blocks_emission() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 100, "bn"), t0);

        // Vc has said nothing, but has not been quiet long enough to assume it
        // has nothing to say.
        assert!(m.drain_ready(t0).is_empty());
    }

    /// Once the other source has been quiet for the window, waiting longer
    /// would just be latency: emit.
    #[test]
    fn an_idle_source_stops_blocking_after_the_window() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 100, "bn"), t0);

        let later = t0 + MERGE_WINDOW + Duration::from_millis(1);
        assert_eq!(texts(&m.drain_ready(later)), vec!["bn"]);
    }

    /// A source at EOF can never deliver anything again, so it stops blocking
    /// immediately rather than after the window.
    #[test]
    fn a_source_at_eof_stops_blocking_at_once() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 100, "bn"), t0);
        m.eof(Source::Vc);

        assert_eq!(texts(&m.drain_ready(t0)), vec!["bn"]);
    }

    /// Both queues non-empty is the common case during the `-n` backlog burst,
    /// and it needs no waiting at all: the heads can be compared directly.
    #[test]
    fn two_non_empty_queues_emit_without_waiting() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 200, "bn"), t0);
        m.push(rec(Source::Vc, 100, "vc"), t0);

        // Only the earlier one: emitting `bn` would need to know Vc has
        // nothing older still coming.
        assert_eq!(texts(&m.drain_ready(t0)), vec!["vc"]);
    }

    /// Equal timestamps must not reorder run to run, or a dump diffs against
    /// itself.
    #[test]
    fn equal_timestamps_break_deterministically_towards_the_beacon_node() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Vc, 100, "vc"), t0);
        m.push(rec(Source::Bn, 100, "bn"), t0);
        m.eof(Source::Bn);
        m.eof(Source::Vc);

        assert_eq!(texts(&m.drain_all()), vec!["bn", "vc"]);
    }

    /// The reason records exist at all. A Teku stack trace is one timestamped
    /// line followed by untimestamped ones; a line-level merge would scatter
    /// the trace through the other source's output.
    #[test]
    fn a_stack_trace_stays_attached_to_the_line_that_introduced_it() {
        let mut b = RecordBuilder::new(Source::Bn, 0);
        assert!(b
            .push_line("2026-09-14 01:16:54.217 ERROR - Failed to import block")
            .is_none());
        assert!(b
            .push_line("java.lang.IllegalStateException: nope")
            .is_none());
        assert!(b
            .push_line("\tat tech.pegasys.teku.Foo.bar(Foo.java:42)")
            .is_none());

        let done = b
            .push_line("2026-09-14 01:16:55.000 INFO  - Slot event")
            .expect("the next timestamped line completes the previous record");
        assert_eq!(done.lines.len(), 3, "trace lines must travel together");
        assert!(done.lines[2].contains("Foo.java:42"));
    }

    /// A line with no timestamp and nothing before it still has to go
    /// somewhere, and dropping it would lose output.
    #[test]
    fn a_leading_untimestamped_line_is_not_dropped() {
        let mut b = RecordBuilder::new(Source::Vc, 7_000);
        assert!(b.push_line("docker: no such container").is_none());
        let done = b.finish().expect("a record is still owed");
        assert_eq!(done.lines, vec!["docker: no such container"]);
        assert_eq!(done.time, LogTime(7_000), "falls back to session start");
    }

    /// The queues are only sorted if each source is monotonic, so a backwards
    /// timestamp is clamped rather than trusted. A clock change would
    /// otherwise corrupt the merge instead of misplacing one line.
    #[test]
    fn a_backwards_timestamp_is_clamped_to_its_predecessor() {
        let mut b = RecordBuilder::new(Source::Bn, 0);
        b.push_line("2026-09-14 02:00:00.000 INFO  - first");
        b.push_line("2026-09-14 01:00:00.000 INFO  - went backwards");
        // The third line is what closes and returns the backwards record.
        let clamped = b
            .push_line("2026-09-14 03:00:00.000 INFO  - forward")
            .expect("the backwards record is returned here");

        assert!(
            clamped.lines[0].contains("went backwards"),
            "wrong record under test"
        );
        assert_eq!(
            clamped.time,
            LogTime(crate::dump::days_from_civil(2026, 9, 14) * 86_400_000 + 7_200_000),
            "should be pinned to its predecessor's 02:00, not its own 01:00"
        );
    }

    /// The time-only console variant inherits the date from its source's last
    /// full timestamp, so it can be compared against the other source at all.
    #[test]
    fn a_time_only_timestamp_inherits_the_last_seen_date() {
        let mut b = RecordBuilder::new(Source::Vc, 0);
        b.push_line("2026-09-14 01:00:00.000 INFO  - dated");
        b.push_line("01:30:00.000 INFO  - undated");
        let undated = b.finish().expect("the undated record is still pending");

        assert!(
            undated.lines[0].contains("undated"),
            "wrong record under test"
        );
        assert_eq!(
            undated.time,
            LogTime(crate::dump::days_from_civil(2026, 9, 14) * 86_400_000 + 5_400_000)
        );
    }

    /// Midnight during a live session: the time of day goes backwards by
    /// nearly a full day, which is a rollover rather than a clock change.
    #[test]
    fn a_time_only_timestamp_rolls_over_at_midnight() {
        let mut b = RecordBuilder::new(Source::Vc, 0);
        b.push_line("2026-09-14 23:59:59.000 INFO  - before");
        b.push_line("00:00:01.000 INFO  - after");
        let after = b.finish().expect("the after record is still pending");

        assert!(after.lines[0].contains("after"), "wrong record under test");
        assert_eq!(
            after.time,
            LogTime(crate::dump::days_from_civil(2026, 9, 15) * 86_400_000 + 1_000)
        );
    }

    /// A single-source session must behave exactly as an unmerged stream: no
    /// waiting on a second source that does not exist.
    #[test]
    fn one_source_never_waits() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 100, "a"), t0);
        m.push(rec(Source::Bn, 200, "b"), t0);

        assert_eq!(texts(&m.drain_ready(t0)), vec!["a", "b"]);
    }

    /// The `-n` backlog case end to end: one source's burst is held until the
    /// other speaks, then both interleave. The rule's clauses are tested
    /// individually above; this pins them composing across drain calls.
    #[test]
    fn a_backlog_is_held_until_the_other_source_speaks_then_interleaves() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 100, "bn-1"), t0);
        m.push(rec(Source::Bn, 300, "bn-2"), t0);
        assert!(
            m.drain_ready(t0).is_empty(),
            "Vc may still have something older"
        );

        let t1 = t0 + Duration::from_millis(10);
        m.push(rec(Source::Vc, 50, "vc-1"), t1);
        m.push(rec(Source::Vc, 200, "vc-2"), t1);

        assert_eq!(texts(&m.drain_ready(t1)), vec!["vc-1", "bn-1", "vc-2"]);
        // bn-2 is held: Vc is empty again and neither idle nor at EOF.
    }

    #[test]
    fn is_done_only_once_every_source_is_at_eof_and_drained() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0);
        m.push(rec(Source::Bn, 100, "a"), t0);
        m.eof(Source::Bn);
        assert!(!m.is_done(), "Vc has not finished");
        m.eof(Source::Vc);
        assert!(!m.is_done(), "a record is still queued");
        m.drain_all();
        assert!(m.is_done());
    }
}
