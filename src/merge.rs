//! Ordering two log sources into one timeline.
//!
//! Pure: no threads, no IO and no clock of its own - every instant arrives as
//! a parameter. The same split `stack::detect_stack` and `logs::producer_argv`
//! use, and for the same reason: the ordering rule is the part that is easy to
//! get subtly wrong and impossible to debug from a live tail, so it is
//! testable without spawning anything.
//!
//! `logs.rs` owns the threads that feed this.

use crate::logfmt::{json_timestamp, parse_timestamp, LogTime, Stamp};
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

/// What feeding one line did to a `RecordBuilder`.
///
/// `stamped_at` is separate from `completed` because they are about different
/// lines: a timestamped line *closes* the record before it while *opening* its
/// own, and the offset estimate needs this line's stamp paired with this
/// line's arrival, not the previous record's.
struct Pushed {
    completed: Option<Record>,
    stamped_at: Option<LogTime>,
}

/// A duration as an operator would say it: `12h`, `5h30m`, `90s`.
///
/// Only ever used for the skew note, where the number is the whole point -
/// `12h` says "timezone" to anyone reading it, where `43200000ms` says
/// nothing.
fn humanize_ms(ms: i64) -> String {
    let seconds = ms / 1_000;
    let (h, m, s) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

/// Groups one source's raw lines into records and gives every record a
/// comparable timestamp.
///
/// Holds the per-source state that dating a timestamp needs: the last time
/// seen (for monotonicity and for rollover) and the last date seen (for the
/// console layout's time-only variant).
///
/// Owned by `Merger` rather than by the thread reading the source. A reader
/// thread blocks on `read_line`, so a builder living there can only ever act
/// when a line arrives - which is exactly the wrong property for a record
/// that needs releasing *because* no line has arrived. See
/// `Merger::flush_stale`.
struct RecordBuilder {
    source: Source,
    /// Where an undated record lands when this source has never carried a
    /// date - the session's start, supplied by the caller so this stays pure.
    session_start_ms: i64,
    last: Option<LogTime>,
    pending: Option<Record>,
}

impl RecordBuilder {
    fn new(source: Source, session_start_ms: i64) -> Self {
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
    ///
    /// **Until this source has produced its first recognised timestamp, an
    /// untimestamped line is its own record and is returned at once**, because
    /// it cannot be the continuation of a line that never arrived. That is not
    /// a nicety. A held record is released only by the next timestamped line,
    /// by `Merger::flush_stale` once the source goes quiet, or by `finish()`
    /// at EOF - and `tekops logs` runs its producers under `tail -F` and
    /// `docker logs -f`, which never reach EOF. Before the flush existed, a
    /// source whose layout this module does not parse accumulated every line
    /// it ever wrote and emitted none of them: a live beacon node showing
    /// nothing at all, with no error anywhere, which is what a stock
    /// bare-metal Teku did while `leading_timestamp` looked for the wrong JSON
    /// key. Any unparsed layout must degrade to out-of-order output rather
    /// than to silence.
    fn push_line(&mut self, raw: &str) -> Pushed {
        let Some(stamp) = leading_timestamp(raw) else {
            match &mut self.pending {
                Some(p) => p.lines.push(raw.to_string()),
                // Nothing to continue, and nothing that ever will: emit.
                None if self.last.is_none() => {
                    return Pushed {
                        completed: Some(Record {
                            source: self.source,
                            time: LogTime(self.session_start_ms),
                            lines: vec![raw.to_string()],
                        }),
                        stamped_at: None,
                    }
                }
                // This source does carry timestamps, so a later line will
                // close this record. It opens at whatever time the source
                // last reached.
                None => {
                    self.pending = Some(Record {
                        source: self.source,
                        time: self.last.unwrap_or(LogTime(self.session_start_ms)),
                        lines: vec![raw.to_string()],
                    })
                }
            }
            return Pushed {
                completed: None,
                stamped_at: None,
            };
        };

        let time = self.resolve(stamp);
        self.last = Some(time);
        Pushed {
            completed: self.pending.replace(Record {
                source: self.source,
                time,
                lines: vec![raw.to_string()],
            }),
            stamped_at: Some(time),
        }
    }

    /// The record still in progress, if there is one. Taken at EOF, and by
    /// `Merger::flush_stale` once the source has been silent for the window.
    fn finish(&mut self) -> Option<Record> {
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
/// JSON carries it in `timestamp` or `@timestamp` (see
/// `logfmt::json_timestamp`); the console layout puts it first, before the
/// level. Anything else has none, which is what marks a continuation line.
fn leading_timestamp(raw: &str) -> Option<Stamp> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        return json_timestamp(&value).and_then(parse_timestamp);
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
///
/// **The per-source `RecordBuilder`s live here**, not in the threads reading
/// the sources. A reader thread blocks on `read_line` and so can only act when
/// a line arrives; releasing a record precisely *because* no line has arrived
/// needs something that ticks, and the emitter already does. See
/// `flush_stale`.
pub struct Merger {
    sources: Vec<Source>,
    builders: Vec<RecordBuilder>,
    queues: Vec<VecDeque<Record>>,
    /// When each source last delivered a *line*, which is not the same as
    /// when it last completed a record: a long stack trace delivers lines for
    /// as long as it takes to write without completing anything. Idleness is a
    /// statement about the producer, so it is measured on the producer's
    /// output rather than on this type's own bookkeeping.
    last_line: Vec<Instant>,
    eof: Vec<bool>,
    window: Duration,
    /// Each source's estimated offset from this host's clock, in
    /// milliseconds, as `min(arrival - stamp)` over its lines. `None` until
    /// that source has produced a line with a timestamp this module can read.
    ///
    /// Never applied to anything. Its only use is `check_skew`, which compares
    /// two of them to notice that the node's two processes disagree about what
    /// time it is, and says so.
    offsets: Vec<Option<i64>>,
    /// Set once, when calibration first shows the sources disagreeing by more
    /// than `SKEW_WORTH_REPORTING`. Taken by the caller, never printed here -
    /// this module has no output of its own.
    skew_note: Option<String>,
    /// Whether the skew has already been judged. Separate from `skew_note`
    /// being `Some`, which stops being true the moment the note is taken.
    skew_judged: bool,
}

/// The granularity every real timezone offset is a multiple of. India is
/// `+05:30`, Nepal `+05:45`, the Chatham Islands `+12:45`; nothing in use is
/// finer.
const ZONE_STEP_MS: i64 = 15 * 60_000;

/// How far a difference may sit from a multiple of `ZONE_STEP_MS` and still be
/// read as a timezone.
///
/// Wide enough to absorb the estimate's own error - the newest line of a
/// source is a fraction of a second old, not zero - and narrow enough that a
/// source which merely went quiet has to land within a second of a quarter
/// hour to be mistaken for one.
const ZONE_TOLERANCE_MS: i64 = 1_000;

/// A difference between two sources' offsets, read as a timezone offset if it
/// can be, and `None` if it cannot.
///
/// `None` is the answer for everything below one step, which includes every
/// ordinary case: two processes on one host, both stamping the same clock,
/// differ by milliseconds.
fn as_zone_offset(difference: i64) -> Option<i64> {
    if difference < ZONE_STEP_MS {
        return None;
    }
    let steps = (difference + ZONE_STEP_MS / 2) / ZONE_STEP_MS;
    let rounded = steps * ZONE_STEP_MS;
    ((difference - rounded).abs() <= ZONE_TOLERANCE_MS).then_some(rounded)
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
    ///
    /// `session_start_ms` is where a record lands when its source has given no
    /// date to place it by. `logs` passes the wall clock, so an undated line
    /// sorts next to the live ones; `dump` passes 0, so one sorts to the top
    /// of the artifact.
    pub fn new(
        sources: Vec<Source>,
        window: Duration,
        started_at: Instant,
        session_start_ms: i64,
    ) -> Self {
        let n = sources.len();
        Merger {
            builders: sources
                .iter()
                .map(|s| RecordBuilder::new(*s, session_start_ms))
                .collect(),
            sources,
            queues: vec![VecDeque::new(); n],
            last_line: vec![started_at; n],
            eof: vec![false; n],
            window,
            offsets: vec![None; n],
            skew_note: None,
            skew_judged: false,
        }
    }

    fn index(&self, source: Source) -> usize {
        self.sources
            .iter()
            .position(|s| *s == source)
            .expect("record from a source this merger was not built with")
    }

    /// Feeds one raw line from one source, queueing whatever record it
    /// completed.
    ///
    /// `at` is when the line arrived, and it is what `flush_stale` and the
    /// emission rule both measure silence against. `arrival_ms` is the same
    /// moment on the wall clock, which `Instant` cannot give: it is only
    /// comparable to itself, and an offset is a comparison against the
    /// timestamps a process writes.
    pub fn push_line(&mut self, source: Source, raw: &str, at: Instant, arrival_ms: i64) {
        let i = self.index(source);
        self.last_line[i] = at;
        let pushed = self.builders[i].push_line(raw);
        if let Some(stamp) = pushed.stamped_at {
            self.observe(i, arrival_ms - stamp.0);
        }
        if let Some(record) = pushed.completed {
            self.queues[i].push_back(record);
        }
    }

    /// Folds one `arrival - stamp` sample into a source's offset estimate.
    ///
    /// **The minimum, not the latest or the mean.** Every sample is the true
    /// offset plus however long that line took to reach us, and that delay is
    /// never negative - so the smallest sample is the least contaminated one.
    /// This is the filter NTP uses, and it is what makes a backlog harmless:
    /// `tail -n 500` hands over lines stamped minutes ago, which produce large
    /// samples, and the first live line produces a small one that wins
    /// outright. A mean would let the backlog drag the estimate for the rest of
    /// the session; the latest sample would let one stale line undo it.
    fn observe(&mut self, i: usize, sample: i64) {
        self.offsets[i] = Some(match self.offsets[i] {
            Some(current) => current.min(sample),
            None => sample,
        });
        self.check_skew();
    }

    /// Notices, once, that the two sources are not stamping the same clock.
    ///
    /// **This reports; it does not correct.** The timestamps a process writes
    /// are what the merge is ordered by, and tekops takes them at face value:
    /// if two processes disagree about what time it is, no ordering across
    /// them is right, and the fix is in the node's log configuration rather
    /// than in a guess made here. Correcting instead was tried and removed -
    /// the estimate below cannot tell a source whose clock is an hour behind
    /// from a source whose newest line is simply an hour old, and acting on
    /// that reordered a `dump-logs` that had been correct. Saying so plainly
    /// and leaving the order alone is the smaller claim and the honest one.
    ///
    /// The bar for saying anything: the sources must disagree by at least a
    /// quarter of an hour, and by *within a second of a multiple* of one.
    /// Every real timezone offset is a multiple of fifteen minutes; a source
    /// that has merely gone quiet is only that, to the second, by coincidence.
    /// Below the bar is ordinary delivery jitter and not worth a line of the
    /// operator's screen.
    fn check_skew(&mut self) {
        if self.skew_judged || self.sources.len() < 2 {
            return;
        }

        let mut known = Vec::with_capacity(self.sources.len());
        for offset in &self.offsets {
            match offset {
                Some(o) => known.push(*o),
                // A source with nothing to say yet cannot be compared to one
                // that has spoken, so there is nothing to judge until it does.
                None => return,
            }
        }

        let base = *known.iter().min().expect("at least two sources");
        let Some(zone) = as_zone_offset(*known.iter().max().expect("at least two sources") - base)
        else {
            return;
        };
        self.skew_judged = true;

        // The source at `base` has the smallest offset, which means it stamps
        // times furthest ahead of this host's clock: the offset is what would
        // have to be *added* to a source's stamps to land on that clock.
        let ahead = self.sources[self
            .offsets
            .iter()
            .position(|o| *o == Some(base))
            .expect("base came from these offsets")];
        let behind = self.sources[self
            .offsets
            .iter()
            .position(|o| *o != Some(base))
            .expect("the spread is non-zero, so some source differs")];
        self.skew_note = Some(format!(
            "{} stamps its log {} ahead of {}, so the two are interleaved by times \
             that do not mean the same thing - set both processes to log UTC \
             (log4j2: %d{{ISO8601}}{{UTC}})",
            ahead.tag(),
            humanize_ms(zone),
            behind.tag(),
        ));
    }

    /// The skew note, once. `None` afterwards, and `None` when the sources
    /// agree or only one of them is being read.
    pub fn take_skew_note(&mut self) -> Option<String> {
        self.skew_note.take()
    }

    /// One source's estimated offset. Test-only: nothing in production reads
    /// the number, only whether two of them disagree by a timezone.
    ///
    /// `None` until that source has produced a readable timestamp.
    #[cfg(test)]
    pub fn offset_ms(&self, source: Source) -> Option<i64> {
        self.offsets[self.index(source)]
    }

    /// Releases any record whose source has been silent for the window.
    ///
    /// Called on the emitter's tick, which is the whole point of the builders
    /// living here. A record is held open only because a continuation line
    /// might still join it, and log4j writes a stack trace's lines in one
    /// burst - so once a source has said nothing for `window`, whatever it is
    /// holding is finished, and waiting longer just hides the newest thing the
    /// node wrote. Under `tail -F` and `docker logs -f` "longer" means until
    /// the node next writes, or forever.
    ///
    /// The released record is the newest one that source has, and every record
    /// already queued for it is older, so appending keeps the queue
    /// non-decreasing - which is the invariant the whole comparison rests on.
    pub fn flush_stale(&mut self, now: Instant) {
        for i in 0..self.sources.len() {
            if now.duration_since(self.last_line[i]) < self.window {
                continue;
            }
            if let Some(record) = self.builders[i].finish() {
                self.queues[i].push_back(record);
            }
        }
    }

    /// Queues an already-built record. Test-only since the builders moved in
    /// here: production has nothing but raw lines to offer, and feeds them
    /// through `push_line`.
    ///
    /// `#[cfg(test)]` rather than `#[allow(dead_code)]` because the two say
    /// different things - this is not a use waiting to be found, it is a seam
    /// the tests need. The emission rule is worth testing against records with
    /// timestamps stated outright, independently of whether `leading_timestamp`
    /// can parse them; routing those tests through `push_line` would couple
    /// every one of them to the layout parser and hide a broken rule behind a
    /// working parser, or the reverse.
    #[cfg(test)]
    pub fn push(&mut self, record: Record, at: Instant) {
        let i = self.index(record.source);
        self.queues[i].push_back(record);
        self.last_line[i] = at;
    }

    /// The source is finished, so nothing can continue the record it was
    /// holding: that record is queued here rather than being left for a line
    /// that cannot come.
    pub fn eof(&mut self, source: Source) {
        let i = self.index(source);
        if let Some(record) = self.builders[i].finish() {
            self.queues[i].push_back(record);
        }
        self.eof[i] = true;
    }

    /// Whether every source has finished and everything queued has been
    /// emitted. The builders need no check of their own: `eof` empties each
    /// one as it is marked, so nothing can be held behind a finished source.
    pub fn is_done(&self) -> bool {
        self.eof.iter().all(|e| *e) && self.queues.iter().all(|q| q.is_empty())
    }

    /// Everything safe to emit as of `now`, in order.
    pub fn drain_ready(&mut self, now: Instant) -> Vec<Record> {
        let mut out = Vec::new();
        while let Some(i) = self.next_index(now) {
            out.push(self.take(i));
        }
        out
    }

    /// Everything queued, in order, ignoring the window. For EOF and for
    /// teardown, where waiting cannot produce anything new.
    ///
    /// Open records are finished first. Nothing else will ever close them, so
    /// leaving them held would drop the last line of every source from a
    /// session the operator ended - which is the tail they were watching.
    pub fn drain_all(&mut self) -> Vec<Record> {
        for i in 0..self.sources.len() {
            if let Some(record) = self.builders[i].finish() {
                self.queues[i].push_back(record);
            }
        }
        let mut out = Vec::new();
        while let Some(i) = self.earliest_head() {
            out.push(self.take(i));
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

    fn take(&mut self, i: usize) -> Record {
        self.queues[i].pop_front().expect("head was just checked")
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
                // why `new` seeds the clock. Treating "never spoken" as idle
                // outright would emit one source's whole backlog before the
                // other had a chance to deliver its first line.
                //
                // Measured on lines rather than on completed records: a source
                // part-way through a long stack trace has an empty queue and
                // nothing finished, but it is plainly not idle, and emitting
                // past it would put the other source's output inside the trace.
                || now.duration_since(self.last_line[i]) >= self.window
        });
        safe.then_some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushes a line as if it arrived at the moment it was stamped, so the
    /// source's offset estimate lands on zero. The right setup for every test
    /// that is about the emission rule rather than about the correction.
    fn synced(m: &mut Merger, source: Source, raw: &str, at: Instant) {
        let stamped = match leading_timestamp(raw) {
            Some(Stamp::Absolute(t)) => t.0,
            Some(Stamp::TimeOfDay(ms)) => ms,
            None => 0,
        };
        m.push_line(source, raw, at, stamped);
    }

    fn rec(source: Source, ms: i64, text: &str) -> Record {
        Record {
            source,
            time: LogTime(ms),
            lines: vec![text.to_string()],
        }
    }

    /// Midnight UTC on the day the reference lines were taken off a node.
    fn day_ms() -> i64 {
        crate::dump::days_from_civil(2026, 9, 17) * 86_400_000
    }

    /// A console-layout line stamped at `h:m:s` on that day, tagged so the
    /// assertions can read the order off the output.
    fn at(h: i64, m: i64, s: i64, text: &str) -> String {
        on(17, h, m, s, text)
    }

    /// The same, on a named day of that month - a source stamping twelve hours
    /// ahead crosses midnight while the host has not.
    fn on(day: i64, h: i64, m: i64, s: i64, text: &str) -> String {
        format!("2026-09-{day:02} {h:02}:{m:02}:{s:02}.000 INFO  - {text}")
    }

    fn ms(h: i64, m: i64, s: i64) -> i64 {
        day_ms() + h * 3_600_000 + m * 60_000 + s * 1_000
    }

    /// The reported deployment, reduced to its essentials: a bare-metal beacon
    /// node stamping local time 12 hours ahead of a validator container
    /// stamping UTC. Both wrote their lines at the same real moments, and the
    /// printed timestamps do not say so.
    ///
    /// **tekops does not reorder them.** It cannot know which reading is
    /// right, and a guess that rearranges a node's logs is worse than an
    /// honest ordering by what each process wrote. So the output stays in
    /// stamp order - every vc line before every bn line, which is visibly odd
    /// - and the note is what explains it and names the fix.
    #[test]
    fn two_sources_twelve_hours_apart_are_reported_rather_than_reordered() {
        let t0 = Instant::now();
        let mut m = Merger::new(
            vec![Source::Bn, Source::Vc],
            Duration::from_millis(250),
            t0,
            ms(12, 0, 0),
        );

        // Host clock 11:59:58 through 12:00:02 on the 17th. bn stamps twelve
        // hours ahead, so it crosses midnight into the 18th while the host has
        // not; vc stamps the host's own time.
        m.push_line(
            Source::Bn,
            &on(17, 23, 59, 58, "bn first"),
            t0,
            ms(11, 59, 58),
        );
        m.push_line(
            Source::Vc,
            &on(17, 11, 59, 59, "vc first"),
            t0,
            ms(11, 59, 59),
        );
        m.push_line(Source::Bn, &on(18, 0, 0, 1, "bn second"), t0, ms(12, 0, 1));
        m.push_line(Source::Vc, &on(17, 12, 0, 2, "vc second"), t0, ms(12, 0, 2));

        let note = m
            .take_skew_note()
            .expect("a half-day disagreement must be reported");
        assert!(note.contains("12h"), "{note}");
        assert!(
            note.contains("UTC"),
            "the operator needs the fix, not just the fact: {note}"
        );

        let later = t0 + Duration::from_millis(300);
        m.flush_stale(later);
        let drained = m.drain_ready(later);
        let expected = [
            on(17, 11, 59, 59, "vc first"),
            on(17, 12, 0, 2, "vc second"),
            on(17, 23, 59, 58, "bn first"),
            on(18, 0, 0, 1, "bn second"),
        ];
        assert_eq!(
            texts(&drained),
            expected.iter().map(String::as_str).collect::<Vec<_>>(),
            "ordered by the times the processes printed, odd as that reads"
        );
    }

    /// Two sources that agree are left alone: the correction must not invent
    /// an offset where there is none.
    #[test]
    fn agreeing_sources_raise_nothing() {
        let t0 = Instant::now();
        let mut m = Merger::new(
            vec![Source::Bn, Source::Vc],
            Duration::from_millis(250),
            t0,
            ms(12, 0, 0),
        );
        m.push_line(Source::Bn, &at(12, 0, 0, "bn"), t0, ms(12, 0, 0));
        m.push_line(Source::Vc, &at(12, 0, 1, "vc"), t0, ms(12, 0, 1));

        let later = t0 + Duration::from_millis(300);
        m.flush_stale(later);
        let drained = m.drain_ready(later);
        let expected = [at(12, 0, 0, "bn"), at(12, 0, 1, "vc")];
        assert_eq!(
            texts(&drained),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    /// The offset is the *minimum* of `arrival - stamp`, not the newest
    /// sample. That is what makes a backlog harmless: its lines were stamped
    /// long before they arrived, so they produce large samples, and the live
    /// line that follows produces a small one and wins. Taking the latest
    /// sample instead would let one stale line poison the estimate.
    #[test]
    fn the_offset_is_the_minimum_sample_so_a_backlog_cannot_poison_it() {
        let t0 = Instant::now();
        let mut m = Merger::new(
            vec![Source::Bn],
            Duration::from_millis(250),
            t0,
            ms(12, 0, 0),
        );

        // A backlog burst: stamped an hour ago, all arriving now.
        m.push_line(Source::Bn, &at(11, 0, 0, "old"), t0, ms(12, 0, 0));
        assert_eq!(
            m.offset_ms(Source::Bn),
            Some(3_600_000),
            "only stale samples so far"
        );

        // Then a live line, stamped as it arrives.
        m.push_line(Source::Bn, &at(12, 0, 1, "live"), t0, ms(12, 0, 1));
        assert_eq!(
            m.offset_ms(Source::Bn),
            Some(0),
            "the live sample is smaller and wins"
        );
    }

    /// Said once, not on every tick: the emitter calls `take_skew_note` fifty
    /// times a second.
    #[test]
    fn a_large_skew_is_reported_once() {
        let t0 = Instant::now();
        let mut m = Merger::new(
            vec![Source::Bn, Source::Vc],
            Duration::from_millis(250),
            t0,
            ms(12, 0, 0),
        );
        assert_eq!(
            m.take_skew_note(),
            None,
            "nothing to say before calibration"
        );

        m.push_line(Source::Bn, &on(17, 23, 59, 58, "bn"), t0, ms(11, 59, 58));
        m.push_line(Source::Vc, &on(17, 11, 59, 59, "vc"), t0, ms(11, 59, 59));

        let note = m
            .take_skew_note()
            .expect("a half-day skew must be reported");
        assert!(note.contains("bn"), "{note}");
        assert!(note.contains("vc"), "{note}");
        assert!(note.contains("12h"), "{note}");
        assert_eq!(m.take_skew_note(), None, "said once, not on every tick");
    }

    /// The gate, directly. Everything below a quarter hour is ordinary
    /// disagreement; above it, only a near-exact multiple is a timezone.
    #[test]
    fn only_a_plausible_timezone_is_read_as_one() {
        assert_eq!(as_zone_offset(0), None, "identical clocks");
        assert_eq!(as_zone_offset(1_000), None, "a second of delivery jitter");
        assert_eq!(as_zone_offset(14 * 60_000), None, "under one step");
        assert_eq!(as_zone_offset(15 * 60_000), Some(15 * 60_000), "+00:15");
        assert_eq!(
            as_zone_offset(12 * 3_600_000 + 400),
            Some(12 * 3_600_000),
            "a real offset, plus the estimate's own error"
        );
        assert_eq!(
            as_zone_offset(5 * 3_600_000 + 30 * 60_000),
            Some(5 * 3_600_000 + 30 * 60_000),
            "+05:30 is a real zone"
        );
        assert_eq!(
            as_zone_offset(37 * 60_000),
            None,
            "a source quiet for 37 minutes is not in another timezone"
        );
    }

    /// The failure that made the gate necessary, from `dump-logs`: with no
    /// live phase there is nothing to calibrate against, so a source whose
    /// newest line is a second older than the other's reads as a source whose
    /// clock is a second behind. Correcting on that evidence reorders a dump
    /// that was already right.
    #[test]
    fn a_source_whose_newest_line_is_merely_older_is_not_a_timezone() {
        let t0 = Instant::now();
        let mut m = Merger::new(
            vec![Source::Bn, Source::Vc],
            Duration::from_millis(250),
            t0,
            0,
        );
        let read_at = ms(20, 0, 0);

        // A dump: every line arrives now, whatever it is stamped.
        m.push_line(Source::Bn, &at(1, 0, 0, "bn one"), t0, read_at);
        m.push_line(Source::Bn, &at(1, 0, 2, "bn two"), t0, read_at);
        m.push_line(Source::Vc, &at(1, 0, 1, "vc one"), t0, read_at);

        let drained = m.drain_all();
        let expected = [
            at(1, 0, 0, "bn one"),
            at(1, 0, 1, "vc one"),
            at(1, 0, 2, "bn two"),
        ];
        assert_eq!(
            texts(&drained),
            expected.iter().map(String::as_str).collect::<Vec<_>>(),
            "the printed stamps are the only evidence here, and they were right"
        );
        assert_eq!(
            m.take_skew_note(),
            None,
            "one second between two logs' newest lines is not a timezone"
        );
    }

    #[test]
    fn a_small_skew_is_not_worth_reporting() {
        let t0 = Instant::now();
        let mut m = Merger::new(
            vec![Source::Bn, Source::Vc],
            Duration::from_millis(250),
            t0,
            ms(12, 0, 0),
        );
        m.push_line(Source::Bn, &at(12, 0, 0, "bn"), t0, ms(12, 0, 0));
        m.push_line(Source::Vc, &at(12, 0, 1, "vc"), t0, ms(12, 0, 1));
        assert_eq!(
            m.take_skew_note(),
            None,
            "ordinary delivery jitter is not a timezone"
        );
    }

    /// The one-record lag. A timestamped line opens a record that only the
    /// *next* timestamped line can close, so under `tail -F` - which has no
    /// next line to offer and no EOF either - the newest thing the node wrote
    /// is the one thing the operator cannot see. On a quiet source that is a
    /// whole slot of waiting, and on a source that stops writing it is
    /// forever.
    #[test]
    fn a_pending_record_is_released_once_its_source_goes_quiet() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn], Duration::from_millis(250), t0, 0);
        synced(&mut m, Source::Bn, "01:00:00.000 INFO  - newest", t0);

        // Still open: a stack trace may yet follow, and it must travel with
        // the line that introduced it.
        assert!(
            m.drain_ready(t0).is_empty(),
            "a fresh record waits for the continuation that may follow"
        );

        let later = t0 + Duration::from_millis(300);
        m.flush_stale(later);
        assert_eq!(
            texts(&m.drain_ready(later)),
            vec!["01:00:00.000 INFO  - newest"],
            "a record whose source has gone quiet is released, not held"
        );
    }

    /// The property the flush must not break: log4j writes a trace's lines in
    /// one burst, so every line refreshes the source's clock and the record
    /// stays open across all of them.
    #[test]
    fn a_stack_trace_arriving_in_a_burst_is_not_split_by_the_flush() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn], Duration::from_millis(250), t0, 0);
        synced(&mut m, Source::Bn, "01:00:00.000 ERROR - boom", t0);
        synced(
            &mut m,
            Source::Bn,
            "\tat org.example.Thing",
            t0 + Duration::from_millis(1),
        );
        synced(
            &mut m,
            Source::Bn,
            "\tat org.example.Other",
            t0 + Duration::from_millis(2),
        );

        let later = t0 + Duration::from_millis(300);
        m.flush_stale(later);
        let out = m.drain_ready(later);
        assert_eq!(out.len(), 1, "one record, not three");
        assert_eq!(out[0].lines.len(), 3, "the trace travels whole");
    }

    /// A flushed record must never land behind one already emitted, because
    /// `Merger` only ever compares queue heads.
    #[test]
    fn a_flushed_record_keeps_its_queue_non_decreasing() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn], Duration::from_millis(250), t0, 0);
        synced(&mut m, Source::Bn, "01:00:00.000 INFO  - first", t0);
        synced(&mut m, Source::Bn, "01:00:01.000 INFO  - second", t0);
        let first = m.drain_ready(t0);
        assert_eq!(texts(&first), vec!["01:00:00.000 INFO  - first"]);

        let later = t0 + Duration::from_millis(300);
        m.flush_stale(later);
        let second = m.drain_ready(later);
        assert_eq!(texts(&second), vec!["01:00:01.000 INFO  - second"]);
        assert!(
            second[0].time >= first[0].time,
            "the flushed record is not older than what preceded it"
        );
    }

    /// `flush_stale` is about silence, so a source still delivering lines
    /// keeps its record open however long the session has run.
    #[test]
    fn a_busy_source_keeps_its_record_open() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn], Duration::from_millis(250), t0, 0);
        synced(&mut m, Source::Bn, "01:00:00.000 INFO  - open", t0);
        let busy = t0 + Duration::from_millis(200);
        synced(&mut m, Source::Bn, "\tstill writing", busy);

        m.flush_stale(busy + Duration::from_millis(100));
        assert!(
            m.drain_ready(busy + Duration::from_millis(100)).is_empty(),
            "only 100ms of silence: the record is still being written"
        );
    }

    /// One real line from a bare-metal Teku, copied verbatim off a node.
    ///
    /// Two things in it that the parser did not expect: the JSON key is
    /// `timestamp`, not `@timestamp`, and the millisecond separator is a
    /// comma. Teku's own JSON layout writes both that way.
    const REAL_BARE_METAL_LINE: &str = r#"{"timestamp":"2026-09-17T19:17:51,172","host":"validator","level":"INFO","thread":"TimeTickTask","class":"teku-event-log","message":"Slot Event  *** Slot: 15233787, Peers: 95","throwable":""}"#;

    #[test]
    fn the_real_bare_metal_json_line_has_a_timestamp() {
        let stamp = leading_timestamp(REAL_BARE_METAL_LINE)
            .expect("a Teku JSON line must carry a timestamp");
        let Stamp::Absolute(t) = stamp else {
            panic!("a dated line is absolute, got {stamp:?}");
        };
        // 2026-09-17T19:17:51.172Z
        assert_eq!(t, LogTime(1789672671172));
    }

    /// The failure this caused, which is worse than a misordered line. An
    /// unrecognised timestamp makes every line look like a stack trace's
    /// continuation, and a continuation is held until the *next* timestamped
    /// line closes the record it belongs to. Under `tail -F` that line never
    /// comes, so the whole source accumulates in `pending` and nothing is ever
    /// pushed - a live beacon node that shows absolutely nothing, forever.
    #[test]
    fn a_source_whose_format_is_not_understood_still_emits() {
        let mut b = RecordBuilder::new(Source::Bn, 1_000);
        let got: Vec<Record> = ["not a log line at all", "nor this one", "nor this"]
            .iter()
            .filter_map(|l| b.push_line(l).completed)
            .collect();
        assert_eq!(
            got.len(),
            3,
            "every line must reach the merger without waiting for an EOF that never comes"
        );
        assert!(b.finish().is_none(), "nothing left held back");
    }

    /// The reason the rule above is "until the first timestamp" rather than
    /// "always": a stack trace still has to stay attached to the line that
    /// introduced it.
    #[test]
    fn a_continuation_after_a_timestamped_line_still_joins_it() {
        let mut b = RecordBuilder::new(Source::Bn, 1_000);
        assert!(b.push_line("01:00:00.000 INFO  - boom").completed.is_none());
        assert!(
            b.push_line("\tat org.example.Thing").completed.is_none(),
            "a trace line joins the record above it"
        );
        let closed = b
            .push_line("01:00:01.000 INFO  - next")
            .completed
            .expect("the next timestamped line closes the record");
        assert_eq!(closed.lines.len(), 2);
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
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
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
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
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
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
        m.push(rec(Source::Bn, 100, "bn"), t0);

        let later = t0 + MERGE_WINDOW + Duration::from_millis(1);
        assert_eq!(texts(&m.drain_ready(later)), vec!["bn"]);
    }

    /// A source at EOF can never deliver anything again, so it stops blocking
    /// immediately rather than after the window.
    #[test]
    fn a_source_at_eof_stops_blocking_at_once() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
        m.push(rec(Source::Bn, 100, "bn"), t0);
        m.eof(Source::Vc);

        assert_eq!(texts(&m.drain_ready(t0)), vec!["bn"]);
    }

    /// Both queues non-empty is the common case during the `-n` backlog burst,
    /// and it needs no waiting at all: the heads can be compared directly.
    #[test]
    fn two_non_empty_queues_emit_without_waiting() {
        let t0 = Instant::now();
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
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
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
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
            .completed
            .is_none());
        assert!(b
            .push_line("java.lang.IllegalStateException: nope")
            .completed
            .is_none());
        assert!(b
            .push_line("\tat tech.pegasys.teku.Foo.bar(Foo.java:42)")
            .completed
            .is_none());

        let done = b
            .push_line("2026-09-14 01:16:55.000 INFO  - Slot event")
            .completed
            .expect("the next timestamped line completes the previous record");
        assert_eq!(done.lines.len(), 3, "trace lines must travel together");
        assert!(done.lines[2].contains("Foo.java:42"));
    }

    /// A line with no timestamp and nothing before it still has to go
    /// somewhere, and dropping it would lose output.
    ///
    /// It is now returned immediately rather than held for `finish()`, and
    /// this line is the reason the distinction matters: `producer_argv` merges
    /// `docker logs`'s stderr into the stream precisely so that a missing
    /// container reports itself this way, and that is documented as the whole
    /// error-reporting path for one. Held until EOF, it reached the operator
    /// in `dump-logs` and never in `logs`, where `docker logs -f` has no EOF -
    /// so `tekops logs --vc-container nope` sat there showing nothing.
    #[test]
    fn a_leading_untimestamped_line_is_emitted_at_once() {
        let mut b = RecordBuilder::new(Source::Vc, 7_000);
        let done = b
            .push_line("docker: no such container")
            .completed
            .expect("nothing precedes it, so nothing can close it later");
        assert_eq!(done.lines, vec!["docker: no such container"]);
        assert_eq!(done.time, LogTime(7_000), "falls back to session start");
        assert!(b.finish().is_none(), "and nothing is left owed");
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
            .completed
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
        let mut m = Merger::new(vec![Source::Bn], MERGE_WINDOW, t0, 0);
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
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
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
        let mut m = Merger::new(vec![Source::Bn, Source::Vc], MERGE_WINDOW, t0, 0);
        m.push(rec(Source::Bn, 100, "a"), t0);
        m.eof(Source::Bn);
        assert!(!m.is_done(), "Vc has not finished");
        m.eof(Source::Vc);
        assert!(!m.is_done(), "a record is still queued");
        m.drain_all();
        assert!(m.is_done());
    }
}
