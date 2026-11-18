//! Recurrence rules for tasks.
//!
//! A recurring task carries an RFC 5545 `RRULE` in its `X-Orb-Task-Recurrence`
//! header — a deliberately small subset: `FREQ=DAILY|WEEKLY|MONTHLY|YEARLY`,
//! `INTERVAL`, `BYDAY`, `BYMONTHDAY`, and at most one of `UNTIL` or `COUNT`.
//! The string in the header is the source of truth, so parsing and
//! serialisation must round-trip a rule exactly; anything the parser cannot
//! represent is rejected loudly rather than dropped, because a silently
//! discarded `INTERVAL=3` turns a quarterly task into a monthly one and nobody
//! notices until it has fired twice too often.
//!
//! # Everything is a local civil date
//!
//! All arithmetic here is on `NaiveDate` / `NaiveTime`, never on an instant.
//! "The 6th" means the 6th on the wall calendar, and a reminder at 09:00 means
//! 09:00 on the wall clock — including on the day a DST transition makes 09:00
//! either 23 or 25 hours after the previous 09:00. Converting to UTC and adding
//! 86_400 seconds is the classic way to make a task appear at 01:00 the day
//! before, or a reminder fire an hour early for half the year; the types in
//! this module make that mistake impossible to write.
//!
//! # Occurrences are computed by re-anchoring, not by expanding from DTSTART
//!
//! There is no `DTSTART`. A task sits on exactly one occurrence at a time (its
//! scheduled date), and completing it moves it to the next one; the history of
//! previous occurrences lives in the task log, not in an expansion. So every
//! query here takes an *anchor* — the date the task currently sits on — and the
//! `INTERVAL` counts from that anchor. Because each advance lands on a genuine
//! occurrence, re-anchoring produces the same series an expansion from the
//! original start would have, while surviving the things an expansion cannot:
//! a task whose dates were edited by hand, a rule that was changed mid-series,
//! and a device that has never seen the original start date.
//!
//! # Missing calendar days are skipped, not clamped
//!
//! `FREQ=MONTHLY` anchored on Jan 31 advances to Mar 31: February has no 31st,
//! so that period yields nothing (RFC 5545 §3.3.10, "invalid dates are
//! ignored"). Clamping to Feb 28 instead would be friendlier exactly once and
//! wrong forever after: the next advance re-anchors on the 28th, so the series
//! silently migrates to the 28th of every month and never returns to the end of
//! the month. A task that really means "the last day of the month" says so with
//! `BYMONTHDAY=-1`, which is stable. The same rule makes `FREQ=YEARLY` anchored
//! on Feb 29 land on the next leap year rather than on Mar 1.

use std::fmt;
use std::str::FromStr;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use thiserror::Error;

/// How many periods the occurrence scanner inspects before giving up.
///
/// Rules with a `BY*` part can legitimately leave whole periods empty
/// (`FREQ=MONTHLY;BYMONTHDAY=31` skips five months a year, `FREQ=YEARLY`
/// anchored on Feb 29 skips three years out of four), and rules that can *never*
/// match are also expressible (`FREQ=YEARLY;BYMONTHDAY=30` anchored in
/// February). A bounded scan distinguishes the two without hanging: the widest
/// legitimate gap is a handful of periods, so anything beyond this horizon is
/// an unsatisfiable rule and is reported as "no further occurrence".
const MAX_PERIOD_SCAN: u32 = 1_000;

/// How many occurrences a single advance will step through while catching up.
///
/// A daily task abandoned for five years needs ~1_800 steps to reach the
/// present; the cap only exists so that a pathological anchor (a date from the
/// year 1 in a corrupted message) cannot spin forever. On hitting it the
/// advance returns the date it reached, which is honest — the task is simply
/// still behind — rather than pretending the series ended.
const MAX_CATCH_UP_STEPS: u32 = 10_000;

/// Everything that can be wrong with a recurrence rule string.
///
/// Parsing is strict on purpose: these strings are written by Orb itself, so
/// anything unexpected is either a corrupted header or a rule from a richer
/// client, and in both cases guessing is worse than refusing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecurrenceError {
    #[error("recurrence rule is empty")]
    Empty,

    #[error("recurrence rule part '{0}' is not NAME=VALUE")]
    MalformedPart(String),

    #[error("recurrence rule part '{0}' appears more than once")]
    DuplicatePart(String),

    #[error(
        "unsupported recurrence part '{0}' (supported: FREQ, INTERVAL, BYDAY, BYMONTHDAY, UNTIL, COUNT)"
    )]
    UnsupportedPart(String),

    #[error("recurrence rule has no FREQ")]
    MissingFreq,

    #[error("unsupported FREQ '{0}' (supported: DAILY, WEEKLY, MONTHLY, YEARLY)")]
    UnsupportedFreq(String),

    #[error("invalid {part} value '{value}'")]
    InvalidValue { part: &'static str, value: String },

    #[error("UNTIL and COUNT cannot both be given")]
    UntilAndCount,

    #[error("BYDAY ordinal in '{0}' is only meaningful for FREQ=MONTHLY or FREQ=YEARLY")]
    OrdinalNotAllowed(String),

    #[error("unsupported WKST '{0}': weeks start on Monday")]
    UnsupportedWeekStart(String),
}

impl From<RecurrenceError> for crate::Error {
    fn from(error: RecurrenceError) -> Self {
        crate::Error::InvalidInput(error.to_string())
    }
}

/// The `FREQ` of a rule: the unit its `INTERVAL` counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Frequency {
    /// The RFC 5545 keyword, which is also what goes in the header.
    pub fn as_str(self) -> &'static str {
        match self {
            Frequency::Daily => "DAILY",
            Frequency::Weekly => "WEEKLY",
            Frequency::Monthly => "MONTHLY",
            Frequency::Yearly => "YEARLY",
        }
    }

    /// Whether an ordinal `BYDAY` (`2MO`, `-1FR`) means anything for this
    /// frequency. Only month-shaped periods have a "second Monday".
    fn allows_by_day_ordinals(self) -> bool {
        matches!(self, Frequency::Monthly | Frequency::Yearly)
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Frequency {
    type Err = RecurrenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_uppercase().as_str() {
            "DAILY" => Ok(Frequency::Daily),
            "WEEKLY" => Ok(Frequency::Weekly),
            "MONTHLY" => Ok(Frequency::Monthly),
            "YEARLY" => Ok(Frequency::Yearly),
            _ => Err(RecurrenceError::UnsupportedFreq(value.to_string())),
        }
    }
}

/// One entry of `BYDAY`: a weekday, optionally qualified by an ordinal.
///
/// The ordinal counts occurrences of that weekday inside the period, from the
/// start when positive (`2MO` — the second Monday) and from the end when
/// negative (`-1FR` — the last Friday). It is only accepted for monthly and
/// yearly rules; RFC 5545 forbids it elsewhere, and "the second Monday of a
/// week" has no meaning anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByDay {
    pub ordinal: Option<i8>,
    pub weekday: Weekday,
}

impl ByDay {
    /// An unqualified weekday, e.g. `MO`.
    pub fn every(weekday: Weekday) -> Self {
        Self {
            ordinal: None,
            weekday,
        }
    }

    /// The `ordinal`-th `weekday` of the period, e.g. `ByDay::nth(2, Weekday::Tue)`.
    pub fn nth(ordinal: i8, weekday: Weekday) -> Self {
        Self {
            ordinal: Some(ordinal),
            weekday,
        }
    }
}

impl fmt::Display for ByDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ordinal) = self.ordinal {
            write!(f, "{}", ordinal)?;
        }
        f.write_str(weekday_code(self.weekday))
    }
}

impl FromStr for ByDay {
    type Err = RecurrenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let upper = value.trim().to_ascii_uppercase();
        let invalid = || RecurrenceError::InvalidValue {
            part: "BYDAY",
            value: value.to_string(),
        };

        // The weekday code is always the last two characters; whatever precedes
        // it is the signed ordinal. Only ASCII can be one or the other, and
        // splitting inside a wider character would panic.
        if upper.len() < 2 || !upper.is_ascii() {
            return Err(invalid());
        }
        let split = upper.len() - 2;
        let (prefix, code) = upper.split_at(split);
        let weekday = parse_weekday_code(code).ok_or_else(invalid)?;

        let ordinal = if prefix.is_empty() {
            None
        } else {
            let parsed: i32 = prefix.parse().map_err(|_| invalid())?;
            // RFC 5545 allows ±1..=±53; zero is meaningless ("the zeroth
            // Monday") and is the shape a truncated string tends to take.
            if parsed == 0 || parsed.abs() > 53 {
                return Err(invalid());
            }
            Some(parsed as i8)
        };

        Ok(ByDay { ordinal, weekday })
    }
}

/// How a series stops, if it stops at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecurrenceEnd {
    /// No `UNTIL` and no `COUNT`: the series runs forever.
    #[default]
    Never,
    /// `UNTIL`: no occurrence may fall after this date.
    Until(NaiveDate),
    /// `COUNT`: the total number of occurrences in the series, the first one
    /// included.
    Count(u32),
}

/// A parsed recurrence rule.
///
/// Field order is the serialisation order, and `by_day` / `by_month_day` keep
/// the order they were written in, so a rule that came off the wire serialises
/// back byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceRule {
    pub freq: Frequency,
    /// Always at least 1; `INTERVAL=0` is rejected at parse time.
    pub interval: u32,
    pub by_day: Vec<ByDay>,
    /// Days of the month, 1..=31 counting from the start and -1..=-31 counting
    /// from the end (`-1` is the last day, whatever length the month has).
    pub by_month_day: Vec<i8>,
    pub end: RecurrenceEnd,
}

impl RecurrenceRule {
    /// A bare rule: every period, no filters, no end.
    pub fn new(freq: Frequency) -> Self {
        Self {
            freq,
            interval: 1,
            by_day: Vec::new(),
            by_month_day: Vec::new(),
            end: RecurrenceEnd::Never,
        }
    }

    pub fn with_interval(mut self, interval: u32) -> Self {
        self.interval = interval.max(1);
        self
    }

    pub fn with_by_day(mut self, by_day: Vec<ByDay>) -> Self {
        self.by_day = by_day;
        self
    }

    pub fn with_by_month_day(mut self, by_month_day: Vec<i8>) -> Self {
        self.by_month_day = by_month_day;
        self
    }

    pub fn with_end(mut self, end: RecurrenceEnd) -> Self {
        self.end = end;
        self
    }

    /// Parse an `X-Orb-Task-Recurrence` value.
    ///
    /// A leading `RRULE:` is tolerated because that is how the rule appears in
    /// a calendar file and therefore how it arrives when someone pastes one,
    /// and casing is normalised because RFC 5545 keywords are case-insensitive.
    /// Everything else is strict: an unknown part is an error rather than a
    /// silent omission.
    pub fn parse(input: &str) -> Result<Self, RecurrenceError> {
        let trimmed = input.trim();
        let body = trimmed
            .strip_prefix("RRULE:")
            .or_else(|| trimmed.strip_prefix("rrule:"))
            .unwrap_or(trimmed)
            .trim();

        if body.is_empty() {
            return Err(RecurrenceError::Empty);
        }

        let mut freq: Option<Frequency> = None;
        let mut interval: Option<u32> = None;
        let mut by_day: Option<Vec<ByDay>> = None;
        let mut by_month_day: Option<Vec<i8>> = None;
        let mut until: Option<NaiveDate> = None;
        let mut count: Option<u32> = None;
        let mut seen_week_start = false;

        for part in body.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((name, value)) = part.split_once('=') else {
                return Err(RecurrenceError::MalformedPart(part.to_string()));
            };
            let name = name.trim().to_ascii_uppercase();
            let value = value.trim();

            macro_rules! set_once {
                ($slot:ident, $parsed:expr) => {{
                    if $slot.is_some() {
                        return Err(RecurrenceError::DuplicatePart(name));
                    }
                    $slot = Some($parsed);
                }};
            }

            match name.as_str() {
                "FREQ" => set_once!(freq, value.parse::<Frequency>()?),
                "INTERVAL" => set_once!(interval, parse_interval(value)?),
                "BYDAY" => set_once!(by_day, parse_by_day(value)?),
                "BYMONTHDAY" => set_once!(by_month_day, parse_by_month_day(value)?),
                "UNTIL" => set_once!(until, parse_until(value)?),
                "COUNT" => set_once!(count, parse_count(value)?),
                // WKST is accepted only in the value we ourselves assume, so
                // that a rule copied from elsewhere is either honoured exactly
                // or refused — never reinterpreted under a different week start.
                "WKST" => {
                    if seen_week_start {
                        return Err(RecurrenceError::DuplicatePart(name));
                    }
                    seen_week_start = true;
                    if !value.eq_ignore_ascii_case("MO") {
                        return Err(RecurrenceError::UnsupportedWeekStart(value.to_string()));
                    }
                }
                _ => return Err(RecurrenceError::UnsupportedPart(name)),
            }
        }

        let freq = freq.ok_or(RecurrenceError::MissingFreq)?;

        let end = match (until, count) {
            (Some(_), Some(_)) => return Err(RecurrenceError::UntilAndCount),
            (Some(until), None) => RecurrenceEnd::Until(until),
            (None, Some(count)) => RecurrenceEnd::Count(count),
            (None, None) => RecurrenceEnd::Never,
        };

        let by_day = by_day.unwrap_or_default();
        if !freq.allows_by_day_ordinals()
            && let Some(offender) = by_day.iter().find(|entry| entry.ordinal.is_some())
        {
            return Err(RecurrenceError::OrdinalNotAllowed(offender.to_string()));
        }

        Ok(Self {
            freq,
            interval: interval.unwrap_or(1),
            by_day,
            by_month_day: by_month_day.unwrap_or_default(),
            end,
        })
    }

    /// The `UNTIL` date, if the series has one.
    pub fn until(&self) -> Option<NaiveDate> {
        match self.end {
            RecurrenceEnd::Until(date) => Some(date),
            _ => None,
        }
    }

    /// The `COUNT`, if the series has one.
    pub fn count(&self) -> Option<u32> {
        match self.end {
            RecurrenceEnd::Count(count) => Some(count),
            _ => None,
        }
    }

    /// The first occurrence strictly after `anchor`, or `None` when the series
    /// has run past its `UNTIL` (or the rule can never match again).
    ///
    /// `COUNT` is deliberately not consulted here: counting requires knowing how
    /// many occurrences already happened, which is the task's business, not the
    /// rule's. [`advance`] is the entry point that honours it.
    pub fn next_after(&self, anchor: NaiveDate) -> Option<NaiveDate> {
        let until = self.until();

        for step in 0..MAX_PERIOD_SCAN {
            // A period the calendar cannot represent means the scan has walked
            // off the end of time; there is nothing beyond it.
            let period = self.period(anchor, step)?;
            // Once whole periods start after UNTIL there is nothing left to
            // find, so stop rather than scanning the full horizon.
            if let Some(until) = until
                && period.start > until
            {
                return None;
            }

            for candidate in self.candidates(anchor, period) {
                if candidate <= anchor {
                    continue;
                }
                if let Some(until) = until
                    && candidate > until
                {
                    return None;
                }
                return Some(candidate);
            }
        }

        None
    }

    /// The next `limit` occurrences after `anchor`, for previews.
    ///
    /// Like [`RecurrenceRule::next_after`] this honours `UNTIL` but not
    /// `COUNT`, and stops early when the series ends.
    pub fn occurrences_after(&self, anchor: NaiveDate, limit: usize) -> Vec<NaiveDate> {
        let mut out = Vec::with_capacity(limit);
        let mut cursor = anchor;
        for _ in 0..limit {
            match self.next_after(cursor) {
                Some(next) => {
                    out.push(next);
                    cursor = next;
                }
                None => break,
            }
        }
        out
    }

    /// The `step`-th period after the one containing `anchor`.
    ///
    /// A period is the span an `INTERVAL` unit covers: a day, a Monday-based
    /// week, a calendar month, or — for yearly rules — the anchor's month in a
    /// later year. Restricting yearly periods to the anchor's month is what
    /// makes `BYDAY`/`BYMONTHDAY` usable without `BYMONTH`, which is not part
    /// of this subset: `FREQ=YEARLY;BYDAY=1MO` anchored in September means the
    /// first Monday of September, not the first Monday of the year.
    fn period(&self, anchor: NaiveDate, step: u32) -> Option<Period> {
        let offset = i64::from(step) * i64::from(self.interval);

        match self.freq {
            Frequency::Daily => {
                let start = anchor.checked_add_signed(Duration::try_days(offset)?)?;
                Some(Period { start, end: start })
            }
            Frequency::Weekly => {
                let week_start = anchor
                    .checked_sub_signed(Duration::days(i64::from(
                        anchor.weekday().num_days_from_monday(),
                    )))?
                    .checked_add_signed(Duration::try_days(offset.checked_mul(7)?)?)?;
                Some(Period {
                    start: week_start,
                    end: week_start.checked_add_signed(Duration::days(6))?,
                })
            }
            Frequency::Monthly => {
                let months = i64::from(anchor.year()) * 12 + i64::from(anchor.month0());
                let target = months.checked_add(offset)?;
                let year = i32::try_from(target.div_euclid(12)).ok()?;
                let month = target.rem_euclid(12) as u32 + 1;
                month_period(year, month)
            }
            Frequency::Yearly => {
                let year = i32::try_from(i64::from(anchor.year()).checked_add(offset)?).ok()?;
                month_period(year, anchor.month())
            }
        }
    }

    /// Every date inside `period` the rule matches, ascending and deduplicated.
    ///
    /// `anchor` supplies the parts of the pattern the rule leaves implicit: a
    /// weekly rule without `BYDAY` recurs on the anchor's weekday, a monthly one
    /// without `BYMONTHDAY` on its day of the month, a yearly one on its
    /// month and day.
    fn candidates(&self, anchor: NaiveDate, period: Period) -> Vec<NaiveDate> {
        let mut dates = match self.freq {
            Frequency::Daily => vec![period.start],
            Frequency::Weekly => {
                if self.by_day.is_empty() {
                    vec![day_of_week_in(period.start, anchor.weekday())]
                } else {
                    self.by_day
                        .iter()
                        .map(|entry| day_of_week_in(period.start, entry.weekday))
                        .collect()
                }
            }
            Frequency::Monthly | Frequency::Yearly => {
                let (year, month) = (period.start.year(), period.start.month());
                if !self.by_month_day.is_empty() {
                    self.by_month_day
                        .iter()
                        .filter_map(|&day| resolve_month_day(year, month, day))
                        .collect()
                } else if !self.by_day.is_empty() {
                    self.expand_by_day_in_month(year, month)
                } else {
                    resolve_month_day(year, month, anchor.day() as i8)
                        .into_iter()
                        .collect()
                }
            }
        };

        // Where a BY* part is not the thing generating candidates it narrows
        // them instead, which is RFC 5545's "limit" role. Ordinals are ignored
        // in that role: "the 13th, but only if it is a Friday" is a weekday
        // test, not a position-in-month test.
        if !self.by_month_day.is_empty()
            && !matches!(self.freq, Frequency::Monthly | Frequency::Yearly)
        {
            dates.retain(|date| self.matches_month_day(*date));
        }
        if !self.by_day.is_empty()
            && (matches!(self.freq, Frequency::Daily)
                || (matches!(self.freq, Frequency::Monthly | Frequency::Yearly)
                    && !self.by_month_day.is_empty()))
        {
            dates.retain(|date| {
                self.by_day
                    .iter()
                    .any(|entry| entry.weekday == date.weekday())
            });
        }

        dates.retain(|date| *date >= period.start && *date <= period.end);
        dates.sort_unstable();
        dates.dedup();
        dates
    }

    /// Expand `BYDAY` inside one calendar month, honouring ordinals.
    fn expand_by_day_in_month(&self, year: i32, month: u32) -> Vec<NaiveDate> {
        let mut dates = Vec::new();
        for entry in &self.by_day {
            match entry.ordinal {
                Some(ordinal) => {
                    if let Some(date) = nth_weekday_of_month(year, month, entry.weekday, ordinal) {
                        dates.push(date);
                    }
                }
                None => dates.extend(weekdays_in_month(year, month, entry.weekday)),
            }
        }
        dates
    }

    /// Whether a date's day of the month is one of the `BYMONTHDAY` entries,
    /// with negatives resolved against that month's actual length.
    fn matches_month_day(&self, date: NaiveDate) -> bool {
        self.by_month_day
            .iter()
            .any(|&day| resolve_month_day(date.year(), date.month(), day) == Some(date))
    }
}

impl fmt::Display for RecurrenceRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FREQ={}", self.freq)?;

        // INTERVAL=1 is the default, so writing it would only make two equal
        // rules compare unequal as strings.
        if self.interval != 1 {
            write!(f, ";INTERVAL={}", self.interval)?;
        }

        if !self.by_day.is_empty() {
            f.write_str(";BYDAY=")?;
            for (index, entry) in self.by_day.iter().enumerate() {
                if index > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{}", entry)?;
            }
        }

        if !self.by_month_day.is_empty() {
            f.write_str(";BYMONTHDAY=")?;
            for (index, day) in self.by_month_day.iter().enumerate() {
                if index > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{}", day)?;
            }
        }

        match self.end {
            RecurrenceEnd::Never => {}
            RecurrenceEnd::Until(date) => write!(f, ";UNTIL={}", date.format("%Y%m%d"))?,
            RecurrenceEnd::Count(count) => write!(f, ";COUNT={}", count)?,
        }

        Ok(())
    }
}

impl FromStr for RecurrenceRule {
    type Err = RecurrenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// The span one `INTERVAL` unit covers, both ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Period {
    start: NaiveDate,
    end: NaiveDate,
}

/// Which date a completion advances the series from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdvanceFrom {
    /// From the date the occurrence was scheduled for. A weekly Monday task
    /// completed on Thursday still moves to Monday, so a series done late does
    /// not drift off its slot. This is the default.
    #[default]
    Scheduled,
    /// From the date the task was actually completed — habit-style, where
    /// "every three days" means three days after the last time it was done.
    Completion,
}

/// The dates a task carries, in the two roles §2 gives them.
///
/// The postpone date is what the task is *scheduled* for and is therefore the
/// anchor whenever it is set; the due date is a deadline that trails it. Both
/// are optional, which is why advancement has to say which fields it filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskSchedule {
    pub postpone: Option<NaiveDate>,
    pub due: Option<NaiveDate>,
}

impl TaskSchedule {
    pub fn new(postpone: Option<NaiveDate>, due: Option<NaiveDate>) -> Self {
        Self { postpone, due }
    }

    /// The date the current occurrence sits on: the postpone date if there is
    /// one, otherwise the due date.
    pub fn anchor(&self) -> Option<NaiveDate> {
        self.postpone.or(self.due)
    }

    /// Whole days between the anchor and the due date, when the task has both.
    ///
    /// Preserving this gap is what keeps "starts Monday, due Friday" intact
    /// across an advance instead of collapsing both onto the same day.
    fn due_gap(&self) -> Option<i64> {
        match (self.postpone, self.due) {
            (Some(postpone), Some(due)) => Some((due - postpone).num_days()),
            _ => None,
        }
    }
}

/// A series moved forward by one completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvancedSchedule {
    /// The task's new dates.
    pub schedule: TaskSchedule,
    /// The occurrence the task now sits on, and the anchor for the next
    /// advance.
    pub anchor: NaiveDate,
    /// Occurrences behind us now, the new one included. Store this on the task
    /// and hand it back next time so `COUNT` can be honoured.
    pub occurrences_elapsed: u32,
    /// How many occurrences this advance consumed. More than one means the
    /// completion was late enough that whole occurrences were missed, which is
    /// worth a log entry.
    pub steps: u32,
}

/// The outcome of completing a recurring task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advancement {
    /// The series continues; apply these dates and clear `completed_at`.
    Next(AdvancedSchedule),
    /// `UNTIL` or `COUNT` is exhausted (or the rule can never match again):
    /// the completion stands and the task stays completed. Reporting this
    /// explicitly is the point — the alternative is a bogus date that quietly
    /// resurrects a finished series.
    Finished,
}

/// Move a recurring task to its next occurrence.
///
/// `occurrences_elapsed` counts the occurrences the task has already been
/// scheduled for, the current one included, so a freshly created task passes 1;
/// 0 is treated as 1 because a task always sits on some occurrence.
///
/// In [`AdvanceFrom::Scheduled`] mode the next occurrence is computed from the
/// scheduled date, which preserves the slot — the phase of the series — no
/// matter how late the completion was. If that occurrence is already in the
/// past because the completion is *very* late, the advance keeps stepping (each
/// step consuming one of `COUNT`'s occurrences, since those occurrences really
/// did pass unmet) until it lands after the completion date. Doing otherwise
/// would hand back a task that is instantly overdue again and demand one
/// completion per missed day.
pub fn advance(
    rule: &RecurrenceRule,
    schedule: TaskSchedule,
    occurrences_elapsed: u32,
    completed_on: NaiveDate,
    mode: AdvanceFrom,
) -> Advancement {
    let mut elapsed = occurrences_elapsed.max(1);
    let mut anchor = match mode {
        AdvanceFrom::Completion => completed_on,
        // A recurring task with no dates at all still recurs; the completion
        // date is the only anchor available.
        AdvanceFrom::Scheduled => schedule.anchor().unwrap_or(completed_on),
    };

    let mut steps = 0;
    loop {
        if let Some(count) = rule.count()
            && elapsed >= count
        {
            return Advancement::Finished;
        }

        let Some(next) = rule.next_after(anchor) else {
            return Advancement::Finished;
        };

        anchor = next;
        elapsed = elapsed.saturating_add(1);
        steps += 1;

        let still_behind = mode == AdvanceFrom::Scheduled && next <= completed_on;
        if !still_behind || steps >= MAX_CATCH_UP_STEPS {
            break;
        }
    }

    Advancement::Next(AdvancedSchedule {
        schedule: apply_anchor(schedule, anchor),
        anchor,
        occurrences_elapsed: elapsed,
        steps,
    })
}

/// Rebuild a schedule around a new anchor, keeping whichever date roles were in
/// use and the distance between them.
///
/// A task with neither date gets a postpone date, because that is the field the
/// Today view keys on: a dateless recurring task that advanced into nothing
/// would simply never come back.
fn apply_anchor(schedule: TaskSchedule, anchor: NaiveDate) -> TaskSchedule {
    match (schedule.postpone, schedule.due) {
        (Some(_), Some(_)) => {
            let gap = schedule.due_gap().unwrap_or(0);
            TaskSchedule {
                postpone: Some(anchor),
                due: shift_days(anchor, gap).or(Some(anchor)),
            }
        }
        (Some(_), None) => TaskSchedule {
            postpone: Some(anchor),
            due: None,
        },
        (None, Some(_)) => TaskSchedule {
            postpone: None,
            due: Some(anchor),
        },
        (None, None) => TaskSchedule {
            postpone: Some(anchor),
            due: None,
        },
    }
}

/// A reminder's position relative to the task's scheduled date.
///
/// §8 requires reminders to follow a recurring task forward, and this is the
/// form that makes that meaningful: whole days of separation plus a wall-clock
/// time. "The evening before" stays the evening before whichever date the task
/// lands on, at the same local time, across DST and across month lengths —
/// which an absolute instant or a duration in seconds would not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderOffset {
    /// Days from the anchor to the reminder's date. Negative means before.
    pub days: i64,
    /// Local wall-clock time of day.
    pub time: NaiveTime,
}

impl ReminderOffset {
    /// Derive the offset of an existing reminder from the date it was set
    /// against.
    pub fn between(anchor: NaiveDate, remind_at: NaiveDateTime) -> Self {
        Self {
            days: (remind_at.date() - anchor).num_days(),
            time: remind_at.time(),
        }
    }

    /// Place the reminder against another anchor date.
    pub fn apply(&self, anchor: NaiveDate) -> Option<NaiveDateTime> {
        Some(shift_days(anchor, self.days)?.and_time(self.time))
    }
}

/// Move a task's reminders from one occurrence to the next.
///
/// Each reminder keeps its distance in whole days and its local time of day, so
/// a "09:00 on the day" reminder is 09:00 on the new day and a "18:00 the
/// evening before" reminder is 18:00 the evening before it. The arithmetic is
/// on civil dates precisely so that a shift across a DST boundary does not slide
/// every reminder by an hour.
///
/// A reminder that cannot be represented against the new anchor (only reachable
/// with absurd dates) is passed through unchanged rather than dropped: a
/// reminder in the wrong place is recoverable, a vanished one is not.
pub fn shift_reminders(
    from: NaiveDate,
    to: NaiveDate,
    reminders: &[NaiveDateTime],
) -> Vec<NaiveDateTime> {
    reminders
        .iter()
        .map(|reminder| {
            ReminderOffset::between(from, *reminder)
                .apply(to)
                .unwrap_or(*reminder)
        })
        .collect()
}

/// Add a signed number of days to a civil date without panicking on overflow.
fn shift_days(date: NaiveDate, days: i64) -> Option<NaiveDate> {
    date.checked_add_signed(Duration::try_days(days)?)
}

/// The whole calendar month `month` of `year`, as a period.
fn month_period(year: i32, month: u32) -> Option<Period> {
    let start = NaiveDate::from_ymd_opt(year, month, 1)?;
    let end = NaiveDate::from_ymd_opt(year, month, last_day_of_month(year, month)?)?;
    Some(Period { start, end })
}

/// How many days `month` has in `year`, leap years included.
fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
    // The day before the first of the next month, which needs no table and is
    // right in February of every leap year.
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(first_of_next.pred_opt()?.day())
}

/// Resolve a `BYMONTHDAY` entry against a specific month.
///
/// Positive days count from the start, negative from the end (`-1` is the last
/// day). A day the month does not have — the 31st of February — resolves to
/// `None` and that period simply yields nothing.
fn resolve_month_day(year: i32, month: u32, day: i8) -> Option<NaiveDate> {
    let length = last_day_of_month(year, month)?;
    let day_of_month = match day.cmp(&0) {
        std::cmp::Ordering::Greater => u32::try_from(day).ok()?,
        std::cmp::Ordering::Less => {
            let from_end = u32::try_from(-i32::from(day)).ok()?;
            length.checked_sub(from_end)?.checked_add(1)?
        }
        std::cmp::Ordering::Equal => return None,
    };
    if day_of_month == 0 || day_of_month > length {
        return None;
    }
    NaiveDate::from_ymd_opt(year, month, day_of_month)
}

/// Every date in a month falling on `weekday`, ascending.
fn weekdays_in_month(year: i32, month: u32, weekday: Weekday) -> Vec<NaiveDate> {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return Vec::new();
    };
    let Some(length) = last_day_of_month(year, month) else {
        return Vec::new();
    };

    let offset = (7 + weekday.num_days_from_monday() - first.weekday().num_days_from_monday()) % 7;
    let mut dates = Vec::with_capacity(5);
    let mut day = 1 + offset;
    while day <= length {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            dates.push(date);
        }
        day += 7;
    }
    dates
}

/// The `ordinal`-th `weekday` of a month, counting from the end when negative.
fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, ordinal: i8) -> Option<NaiveDate> {
    let all = weekdays_in_month(year, month, weekday);
    if ordinal > 0 {
        all.get(usize::try_from(ordinal).ok()?.checked_sub(1)?)
            .copied()
    } else if ordinal < 0 {
        let from_end = usize::try_from(-i32::from(ordinal)).ok()?;
        all.len()
            .checked_sub(from_end)
            .and_then(|i| all.get(i))
            .copied()
    } else {
        None
    }
}

/// The day of `week_start`'s week falling on `weekday` (weeks start Monday).
fn day_of_week_in(week_start: NaiveDate, weekday: Weekday) -> NaiveDate {
    week_start + Duration::days(i64::from(weekday.num_days_from_monday()))
}

/// The RFC 5545 two-letter code for a weekday.
fn weekday_code(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

fn parse_weekday_code(code: &str) -> Option<Weekday> {
    match code {
        "MO" => Some(Weekday::Mon),
        "TU" => Some(Weekday::Tue),
        "WE" => Some(Weekday::Wed),
        "TH" => Some(Weekday::Thu),
        "FR" => Some(Weekday::Fri),
        "SA" => Some(Weekday::Sat),
        "SU" => Some(Weekday::Sun),
        _ => None,
    }
}

fn parse_interval(value: &str) -> Result<u32, RecurrenceError> {
    // INTERVAL=0 would make every period the same period: an infinite loop
    // dressed as a schedule.
    match value.parse::<u32>() {
        Ok(interval) if interval >= 1 => Ok(interval),
        _ => Err(RecurrenceError::InvalidValue {
            part: "INTERVAL",
            value: value.to_string(),
        }),
    }
}

fn parse_count(value: &str) -> Result<u32, RecurrenceError> {
    match value.parse::<u32>() {
        Ok(count) if count >= 1 => Ok(count),
        _ => Err(RecurrenceError::InvalidValue {
            part: "COUNT",
            value: value.to_string(),
        }),
    }
}

fn parse_by_day(value: &str) -> Result<Vec<ByDay>, RecurrenceError> {
    if value.is_empty() {
        return Err(RecurrenceError::InvalidValue {
            part: "BYDAY",
            value: value.to_string(),
        });
    }
    value.split(',').map(str::parse::<ByDay>).collect()
}

fn parse_by_month_day(value: &str) -> Result<Vec<i8>, RecurrenceError> {
    if value.is_empty() {
        return Err(RecurrenceError::InvalidValue {
            part: "BYMONTHDAY",
            value: value.to_string(),
        });
    }
    value
        .split(',')
        .map(|entry| {
            let parsed: i32 = entry
                .trim()
                .parse()
                .map_err(|_| RecurrenceError::InvalidValue {
                    part: "BYMONTHDAY",
                    value: entry.to_string(),
                })?;
            if parsed == 0 || parsed.abs() > 31 {
                return Err(RecurrenceError::InvalidValue {
                    part: "BYMONTHDAY",
                    value: entry.to_string(),
                });
            }
            Ok(parsed as i8)
        })
        .collect()
}

/// Parse an `UNTIL` value.
///
/// RFC 5545 also allows a UTC date-time here. Orb's rules are civil-date only,
/// so the time is read and discarded rather than refused — a rule pasted from a
/// calendar should still work, and "no occurrence after this day" is the only
/// meaning a date-only series can give it.
fn parse_until(value: &str) -> Result<NaiveDate, RecurrenceError> {
    let date_part = value.split(['T', 't']).next().unwrap_or(value);
    NaiveDate::parse_from_str(date_part, "%Y%m%d").map_err(|_| RecurrenceError::InvalidValue {
        part: "UNTIL",
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid test date")
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        date(year, month, day)
            .and_hms_opt(hour, minute, 0)
            .expect("valid test time")
    }

    fn rule(text: &str) -> RecurrenceRule {
        RecurrenceRule::parse(text).unwrap_or_else(|e| panic!("{text} should parse: {e}"))
    }

    // ---- parsing and serialisation ------------------------------------

    #[test]
    fn canonical_strings_round_trip_unchanged() {
        for text in [
            "FREQ=DAILY",
            "FREQ=DAILY;INTERVAL=3",
            "FREQ=WEEKLY;BYDAY=MO,WE,FR",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
            "FREQ=MONTHLY;BYMONTHDAY=1,15",
            "FREQ=MONTHLY;BYMONTHDAY=-1",
            "FREQ=MONTHLY;BYDAY=2TU",
            "FREQ=MONTHLY;BYDAY=-1FR;COUNT=12",
            "FREQ=YEARLY;INTERVAL=2;BYMONTHDAY=24;UNTIL=20301231",
            "FREQ=MONTHLY;INTERVAL=3;BYDAY=MO,-1SU;BYMONTHDAY=1,-1;COUNT=4",
        ] {
            assert_eq!(rule(text).to_string(), text, "round trip of {text}");
        }
    }

    #[test]
    fn rules_survive_a_string_round_trip() {
        let original = RecurrenceRule::new(Frequency::Monthly)
            .with_interval(6)
            .with_by_day(vec![
                ByDay::nth(-1, Weekday::Fri),
                ByDay::every(Weekday::Mon),
            ])
            .with_by_month_day(vec![1, -2])
            .with_end(RecurrenceEnd::Until(date(2031, 2, 28)));

        assert_eq!(rule(&original.to_string()), original);
    }

    #[test]
    fn parsing_is_case_insensitive_and_tolerates_an_rrule_prefix() {
        assert_eq!(
            rule("RRULE:freq=weekly;interval=2;byday=mo,we"),
            rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE")
        );
    }

    #[test]
    fn interval_one_is_never_written_out() {
        assert_eq!(rule("FREQ=DAILY;INTERVAL=1").to_string(), "FREQ=DAILY");
    }

    #[test]
    fn until_accepts_the_date_time_form_and_normalises_it() {
        assert_eq!(
            rule("FREQ=DAILY;UNTIL=20261231T235959Z").until(),
            Some(date(2026, 12, 31))
        );
        assert_eq!(
            rule("FREQ=DAILY;UNTIL=20261231T235959Z").to_string(),
            "FREQ=DAILY;UNTIL=20261231"
        );
    }

    #[test]
    fn week_start_monday_is_accepted_and_anything_else_refused() {
        assert_eq!(rule("FREQ=WEEKLY;WKST=MO"), rule("FREQ=WEEKLY"));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=WEEKLY;WKST=SU"),
            Err(RecurrenceError::UnsupportedWeekStart(_))
        ));
    }

    #[test]
    fn malformed_rules_are_rejected_rather_than_guessed() {
        use RecurrenceError::*;
        assert!(matches!(RecurrenceRule::parse("   "), Err(Empty)));
        assert!(matches!(
            RecurrenceRule::parse("INTERVAL=2"),
            Err(MissingFreq)
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=HOURLY"),
            Err(UnsupportedFreq(_))
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;BYSETPOS=1"),
            Err(UnsupportedPart(_))
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;INTERVAL"),
            Err(MalformedPart(_))
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;FREQ=WEEKLY"),
            Err(DuplicatePart(_))
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;INTERVAL=0"),
            Err(InvalidValue { .. })
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;COUNT=0"),
            Err(InvalidValue { .. })
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=MONTHLY;BYMONTHDAY=0"),
            Err(InvalidValue { .. })
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=MONTHLY;BYMONTHDAY=32"),
            Err(InvalidValue { .. })
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=WEEKLY;BYDAY=XX"),
            Err(InvalidValue { .. })
        ));
        // A wide character where the split falls was a panic, not an error.
        for byday in ["aéa", "é", "1ÄMO", "ＭＯ"] {
            assert!(matches!(
                RecurrenceRule::parse(&format!("FREQ=WEEKLY;BYDAY={byday}")),
                Err(InvalidValue { .. })
            ));
        }
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;UNTIL=2026-12-31"),
            Err(InvalidValue { .. })
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;UNTIL=20261231;COUNT=3"),
            Err(UntilAndCount)
        ));
    }

    #[test]
    fn by_day_ordinals_are_refused_where_they_have_no_meaning() {
        assert!(matches!(
            RecurrenceRule::parse("FREQ=WEEKLY;BYDAY=2MO"),
            Err(RecurrenceError::OrdinalNotAllowed(_))
        ));
        assert!(matches!(
            RecurrenceRule::parse("FREQ=DAILY;BYDAY=-1FR"),
            Err(RecurrenceError::OrdinalNotAllowed(_))
        ));
        assert!(RecurrenceRule::parse("FREQ=MONTHLY;BYDAY=2MO").is_ok());
        assert!(matches!(
            RecurrenceRule::parse("FREQ=MONTHLY;BYDAY=0MO"),
            Err(RecurrenceError::InvalidValue { .. })
        ));
    }

    // ---- daily ---------------------------------------------------------

    #[test]
    fn daily_steps_by_its_interval() {
        assert_eq!(
            rule("FREQ=DAILY").next_after(date(2026, 8, 6)),
            Some(date(2026, 8, 7))
        );
        assert_eq!(
            rule("FREQ=DAILY;INTERVAL=3").next_after(date(2026, 8, 6)),
            Some(date(2026, 8, 9))
        );
        assert_eq!(
            rule("FREQ=DAILY;INTERVAL=10").next_after(date(2026, 12, 28)),
            Some(date(2027, 1, 7))
        );
    }

    #[test]
    fn daily_crosses_a_leap_day_without_skipping_it() {
        assert_eq!(
            rule("FREQ=DAILY").next_after(date(2028, 2, 28)),
            Some(date(2028, 2, 29))
        );
        assert_eq!(
            rule("FREQ=DAILY").next_after(date(2026, 2, 28)),
            Some(date(2026, 3, 1))
        );
    }

    #[test]
    fn daily_with_byday_is_filtered_to_those_weekdays() {
        // Weekdays only: Friday's next occurrence is Monday.
        let weekdays = rule("FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR");
        assert_eq!(
            weekdays.next_after(date(2026, 8, 7)),
            Some(date(2026, 8, 10))
        );
        assert_eq!(
            weekdays.next_after(date(2026, 8, 10)),
            Some(date(2026, 8, 11))
        );
    }

    #[test]
    fn daily_with_bymonthday_only_matches_that_day() {
        assert_eq!(
            rule("FREQ=DAILY;BYMONTHDAY=1").next_after(date(2026, 8, 6)),
            Some(date(2026, 9, 1))
        );
    }

    // ---- weekly --------------------------------------------------------

    #[test]
    fn weekly_without_byday_keeps_the_anchors_weekday() {
        // 2026-08-06 is a Thursday.
        assert_eq!(
            rule("FREQ=WEEKLY").next_after(date(2026, 8, 6)),
            Some(date(2026, 8, 13))
        );
        assert_eq!(
            rule("FREQ=WEEKLY;INTERVAL=2").next_after(date(2026, 8, 6)),
            Some(date(2026, 8, 20))
        );
    }

    #[test]
    fn weekly_byday_walks_within_the_week_then_crosses_into_the_next() {
        let rule = rule("FREQ=WEEKLY;BYDAY=MO,WE,FR");
        // Monday -> Wednesday -> Friday, all inside one week.
        assert_eq!(rule.next_after(date(2026, 8, 3)), Some(date(2026, 8, 5)));
        assert_eq!(rule.next_after(date(2026, 8, 5)), Some(date(2026, 8, 7)));
        // Friday is the last match of its week, so the next is the following
        // Monday — the week boundary, not "three days on".
        assert_eq!(rule.next_after(date(2026, 8, 7)), Some(date(2026, 8, 10)));
    }

    #[test]
    fn weekly_byday_is_matched_even_from_an_anchor_that_is_not_in_the_set() {
        // Anchored on a Sunday, the next MO,WE,FR is the very next day.
        assert_eq!(
            rule("FREQ=WEEKLY;BYDAY=MO,WE,FR").next_after(date(2026, 8, 9)),
            Some(date(2026, 8, 10))
        );
    }

    #[test]
    fn a_fortnightly_byday_set_skips_the_intervening_week() {
        let rule = rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,TH");
        // Anchored Monday Aug 3: Thursday is still in the same period.
        assert_eq!(rule.next_after(date(2026, 8, 3)), Some(date(2026, 8, 6)));
        // From that Thursday the period is exhausted, so the whole of the next
        // week is skipped and the series resumes on Monday Aug 17.
        assert_eq!(rule.next_after(date(2026, 8, 6)), Some(date(2026, 8, 17)));
    }

    #[test]
    fn weekly_byday_order_does_not_affect_the_result() {
        assert_eq!(
            rule("FREQ=WEEKLY;BYDAY=FR,MO,WE").next_after(date(2026, 8, 3)),
            rule("FREQ=WEEKLY;BYDAY=MO,WE,FR").next_after(date(2026, 8, 3))
        );
    }

    // ---- monthly -------------------------------------------------------

    #[test]
    fn monthly_keeps_the_anchors_day_and_skips_months_without_it() {
        let rule = rule("FREQ=MONTHLY");
        assert_eq!(rule.next_after(date(2026, 8, 6)), Some(date(2026, 9, 6)));
        // Jan 31 + 1 month: February has no 31st, so the series lands on Mar 31
        // rather than clamping to Feb 28 and drifting there forever.
        assert_eq!(rule.next_after(date(2026, 1, 31)), Some(date(2026, 3, 31)));
        assert_eq!(rule.next_after(date(2026, 3, 31)), Some(date(2026, 5, 31)));
        // A 30th survives February the same way but exists in April.
        assert_eq!(rule.next_after(date(2026, 1, 30)), Some(date(2026, 3, 30)));
    }

    #[test]
    fn monthly_bymonthday_31_skips_february() {
        assert_eq!(
            rule("FREQ=MONTHLY;BYMONTHDAY=31").next_after(date(2026, 1, 31)),
            Some(date(2026, 3, 31))
        );
        assert_eq!(
            rule("FREQ=MONTHLY;BYMONTHDAY=31").next_after(date(2028, 1, 31)),
            Some(date(2028, 3, 31))
        );
    }

    #[test]
    fn monthly_bymonthday_minus_one_is_the_last_day_of_every_month() {
        let rule = rule("FREQ=MONTHLY;BYMONTHDAY=-1");
        assert_eq!(rule.next_after(date(2026, 1, 31)), Some(date(2026, 2, 28)));
        assert_eq!(rule.next_after(date(2026, 2, 28)), Some(date(2026, 3, 31)));
        assert_eq!(rule.next_after(date(2026, 4, 30)), Some(date(2026, 5, 31)));
        // Leap February really has 29 days.
        assert_eq!(rule.next_after(date(2028, 1, 31)), Some(date(2028, 2, 29)));
    }

    #[test]
    fn monthly_with_several_bymonthdays_visits_them_in_order() {
        let rule = rule("FREQ=MONTHLY;BYMONTHDAY=1,15");
        assert_eq!(rule.next_after(date(2026, 8, 1)), Some(date(2026, 8, 15)));
        assert_eq!(rule.next_after(date(2026, 8, 15)), Some(date(2026, 9, 1)));
        assert_eq!(rule.next_after(date(2026, 8, 20)), Some(date(2026, 9, 1)));
    }

    #[test]
    fn monthly_interval_counts_months_not_occurrences() {
        let rule = rule("FREQ=MONTHLY;INTERVAL=3;BYMONTHDAY=10");
        assert_eq!(rule.next_after(date(2026, 8, 10)), Some(date(2026, 11, 10)));
        assert_eq!(rule.next_after(date(2026, 11, 10)), Some(date(2027, 2, 10)));
    }

    #[test]
    fn monthly_ordinal_byday_picks_the_nth_weekday() {
        // Second Tuesday of each month.
        let second_tuesday = rule("FREQ=MONTHLY;BYDAY=2TU");
        assert_eq!(
            second_tuesday.next_after(date(2026, 8, 1)),
            Some(date(2026, 8, 11))
        );
        assert_eq!(
            second_tuesday.next_after(date(2026, 8, 11)),
            Some(date(2026, 9, 8))
        );

        // Last Friday of each month, including a month whose last Friday is the
        // fourth rather than the fifth.
        let last_friday = rule("FREQ=MONTHLY;BYDAY=-1FR");
        assert_eq!(
            last_friday.next_after(date(2026, 8, 1)),
            Some(date(2026, 8, 28))
        );
        assert_eq!(
            last_friday.next_after(date(2026, 8, 28)),
            Some(date(2026, 9, 25))
        );
    }

    #[test]
    fn an_ordinal_that_does_not_exist_skips_the_month() {
        // February 2026 starts on a Sunday and has 28 days, so it has only four
        // Mondays; the fifth Monday is in March.
        assert_eq!(
            rule("FREQ=MONTHLY;BYDAY=5MO").next_after(date(2026, 1, 29)),
            Some(date(2026, 3, 30))
        );
    }

    #[test]
    fn monthly_byday_without_an_ordinal_matches_every_such_weekday() {
        let rule = rule("FREQ=MONTHLY;BYDAY=MO");
        assert_eq!(rule.next_after(date(2026, 8, 3)), Some(date(2026, 8, 10)));
        assert_eq!(rule.next_after(date(2026, 8, 31)), Some(date(2026, 9, 7)));
    }

    #[test]
    fn byday_narrows_bymonthday_rather_than_adding_to_it() {
        // Friday the 13th: the 13th, but only when it is a Friday.
        let rule = rule("FREQ=MONTHLY;BYMONTHDAY=13;BYDAY=FR");
        assert_eq!(rule.next_after(date(2026, 1, 1)), Some(date(2026, 2, 13)));
        assert_eq!(rule.next_after(date(2026, 2, 13)), Some(date(2026, 3, 13)));
        assert_eq!(rule.next_after(date(2026, 3, 13)), Some(date(2026, 11, 13)));
    }

    // ---- yearly --------------------------------------------------------

    #[test]
    fn yearly_keeps_the_month_and_day() {
        assert_eq!(
            rule("FREQ=YEARLY").next_after(date(2026, 8, 6)),
            Some(date(2027, 8, 6))
        );
        assert_eq!(
            rule("FREQ=YEARLY;INTERVAL=2").next_after(date(2026, 8, 6)),
            Some(date(2028, 8, 6))
        );
    }

    #[test]
    fn yearly_on_a_leap_day_waits_for_the_next_leap_year() {
        assert_eq!(
            rule("FREQ=YEARLY").next_after(date(2028, 2, 29)),
            Some(date(2032, 2, 29))
        );
        // 2100 is not a leap year, so a series crossing it skips eight years.
        assert_eq!(
            rule("FREQ=YEARLY").next_after(date(2096, 2, 29)),
            Some(date(2104, 2, 29))
        );
    }

    #[test]
    fn yearly_by_parts_apply_within_the_anchors_month() {
        assert_eq!(
            rule("FREQ=YEARLY;BYMONTHDAY=-1").next_after(date(2026, 2, 28)),
            Some(date(2027, 2, 28))
        );
        assert_eq!(
            rule("FREQ=YEARLY;BYMONTHDAY=-1").next_after(date(2027, 2, 28)),
            Some(date(2028, 2, 29))
        );
        // First Monday of December, every year.
        assert_eq!(
            rule("FREQ=YEARLY;BYDAY=1MO").next_after(date(2026, 12, 7)),
            Some(date(2027, 12, 6))
        );
    }

    #[test]
    fn an_unsatisfiable_rule_reports_no_occurrence_instead_of_hanging() {
        // February never has a 30th, so this yearly rule can never fire again.
        assert_eq!(
            rule("FREQ=YEARLY;BYMONTHDAY=30").next_after(date(2026, 2, 1)),
            None
        );
    }

    // ---- UNTIL and COUNT ------------------------------------------------

    #[test]
    fn until_stops_the_series_at_its_date() {
        let rule = rule("FREQ=WEEKLY;UNTIL=20260820");
        assert_eq!(rule.next_after(date(2026, 8, 6)), Some(date(2026, 8, 13)));
        // Aug 20 is the UNTIL date itself and is still an occurrence.
        assert_eq!(rule.next_after(date(2026, 8, 13)), Some(date(2026, 8, 20)));
        assert_eq!(rule.next_after(date(2026, 8, 20)), None);
    }

    #[test]
    fn a_series_past_its_until_is_finished_not_advanced() {
        let rule = rule("FREQ=WEEKLY;UNTIL=20260820");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 20)), None);
        assert_eq!(
            advance(
                &rule,
                schedule,
                3,
                date(2026, 8, 20),
                AdvanceFrom::Scheduled
            ),
            Advancement::Finished
        );
    }

    #[test]
    fn count_is_exhausted_after_exactly_that_many_occurrences() {
        let rule = rule("FREQ=DAILY;COUNT=3");
        let mut schedule = TaskSchedule::new(Some(date(2026, 8, 6)), None);
        let mut elapsed = 1;
        let mut anchor = date(2026, 8, 6);

        // Each occurrence is completed on the day it is scheduled for.
        for expected in [date(2026, 8, 7), date(2026, 8, 8)] {
            let Advancement::Next(next) =
                advance(&rule, schedule, elapsed, anchor, AdvanceFrom::Scheduled)
            else {
                panic!("series ended early");
            };
            assert_eq!(next.anchor, expected);
            schedule = next.schedule;
            elapsed = next.occurrences_elapsed;
            anchor = next.anchor;
        }

        assert_eq!(elapsed, 3);
        assert_eq!(
            advance(
                &rule,
                schedule,
                elapsed,
                date(2026, 8, 8),
                AdvanceFrom::Scheduled
            ),
            Advancement::Finished
        );
    }

    #[test]
    fn a_zero_elapsed_count_is_treated_as_the_first_occurrence() {
        // A task restored from a store that never recorded the counter must not
        // burn an occurrence, nor immediately declare a COUNT=1 series over
        // twice.
        let rule = rule("FREQ=DAILY;COUNT=2");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 6)), None);
        let Advancement::Next(next) =
            advance(&rule, schedule, 0, date(2026, 8, 6), AdvanceFrom::Scheduled)
        else {
            panic!("first advance should be allowed");
        };
        assert_eq!(next.occurrences_elapsed, 2);
        assert_eq!(
            advance(
                &rule,
                next.schedule,
                next.occurrences_elapsed,
                date(2026, 8, 7),
                AdvanceFrom::Scheduled
            ),
            Advancement::Finished
        );
    }

    #[test]
    fn catching_up_consumes_the_occurrences_it_passes() {
        // COUNT=5 daily from Aug 1, completed on Aug 4: the occurrences of the
        // 2nd, 3rd and 4th really did pass, so only the 5th is left.
        let rule = rule("FREQ=DAILY;COUNT=5");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 1)), None);
        let Advancement::Next(next) =
            advance(&rule, schedule, 1, date(2026, 8, 4), AdvanceFrom::Scheduled)
        else {
            panic!("series should still have an occurrence left");
        };
        assert_eq!(next.anchor, date(2026, 8, 5));
        assert_eq!(next.steps, 4);
        assert_eq!(next.occurrences_elapsed, 5);
        assert_eq!(
            advance(
                &rule,
                next.schedule,
                next.occurrences_elapsed,
                date(2026, 8, 5),
                AdvanceFrom::Scheduled
            ),
            Advancement::Finished
        );
    }

    // ---- advancement ----------------------------------------------------

    #[test]
    fn scheduled_advancement_keeps_the_slot_when_a_task_is_done_late() {
        // Weekly on Monday, scheduled Aug 3, actually done Thursday Aug 6.
        let rule = rule("FREQ=WEEKLY;BYDAY=MO");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 3)), None);
        let Advancement::Next(next) =
            advance(&rule, schedule, 1, date(2026, 8, 6), AdvanceFrom::Scheduled)
        else {
            panic!("series continues");
        };
        assert_eq!(next.anchor, date(2026, 8, 10));
        assert_eq!(next.steps, 1);
    }

    #[test]
    fn completion_advancement_moves_the_slot_to_the_completion_day() {
        let rule = rule("FREQ=WEEKLY;BYDAY=MO");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 3)), None);
        let Advancement::Next(next) = advance(
            &rule,
            schedule,
            1,
            date(2026, 8, 6),
            AdvanceFrom::Completion,
        ) else {
            panic!("series continues");
        };
        // Anchored on the Thursday it was done, the next Monday is Aug 10 as
        // well — the difference shows up over repeated cycles, below.
        assert_eq!(next.anchor, date(2026, 8, 10));
    }

    #[test]
    fn scheduled_mode_does_not_drift_where_completion_mode_does() {
        // "Every 7 days", done two days late each time.
        let rule = rule("FREQ=DAILY;INTERVAL=7");

        let mut scheduled = TaskSchedule::new(Some(date(2026, 8, 3)), None);
        let mut elapsed = 1;
        let mut day = date(2026, 8, 3);
        for _ in 0..4 {
            let completed = day + Duration::days(2);
            let Advancement::Next(next) =
                advance(&rule, scheduled, elapsed, completed, AdvanceFrom::Scheduled)
            else {
                panic!("series continues");
            };
            scheduled = next.schedule;
            elapsed = next.occurrences_elapsed;
            day = next.anchor;
        }
        // Four cycles of seven days from the original Monday, still a Monday.
        assert_eq!(day, date(2026, 8, 31));
        assert_eq!(day.weekday(), Weekday::Mon);

        let mut habit = TaskSchedule::new(Some(date(2026, 8, 3)), None);
        let mut habit_day = date(2026, 8, 3);
        for _ in 0..4 {
            let completed = habit_day + Duration::days(2);
            let Advancement::Next(next) =
                advance(&rule, habit, 1, completed, AdvanceFrom::Completion)
            else {
                panic!("series continues");
            };
            habit = next.schedule;
            habit_day = next.anchor;
        }
        // Each late completion pushes the habit two days further out.
        assert_eq!(habit_day, date(2026, 9, 8));
    }

    #[test]
    fn a_very_late_completion_catches_up_instead_of_landing_in_the_past() {
        let rule = rule("FREQ=DAILY");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 1)), None);
        let Advancement::Next(next) = advance(
            &rule,
            schedule,
            1,
            date(2026, 8, 10),
            AdvanceFrom::Scheduled,
        ) else {
            panic!("series continues");
        };
        assert_eq!(next.anchor, date(2026, 8, 11));
        assert_eq!(next.steps, 10);
    }

    #[test]
    fn catching_up_still_lands_on_the_series_own_slot() {
        // Weekly on Monday, forgotten for a month: the task comes back on a
        // Monday, not on the day it was finally ticked off.
        let rule = rule("FREQ=WEEKLY;BYDAY=MO");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 3)), None);
        let Advancement::Next(next) =
            advance(&rule, schedule, 1, date(2026, 9, 2), AdvanceFrom::Scheduled)
        else {
            panic!("series continues");
        };
        assert_eq!(next.anchor, date(2026, 9, 7));
        assert_eq!(next.anchor.weekday(), Weekday::Mon);
    }

    #[test]
    fn an_early_completion_does_not_move_the_slot() {
        let rule = rule("FREQ=MONTHLY;BYMONTHDAY=15");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 15)), None);
        let Advancement::Next(next) = advance(
            &rule,
            schedule,
            1,
            date(2026, 8, 12),
            AdvanceFrom::Scheduled,
        ) else {
            panic!("series continues");
        };
        assert_eq!(next.anchor, date(2026, 9, 15));
        assert_eq!(next.steps, 1);
    }

    #[test]
    fn the_gap_between_postpone_and_due_survives_an_advance() {
        let rule = rule("FREQ=WEEKLY");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 3)), Some(date(2026, 8, 7)));
        let Advancement::Next(next) =
            advance(&rule, schedule, 1, date(2026, 8, 5), AdvanceFrom::Scheduled)
        else {
            panic!("series continues");
        };
        assert_eq!(next.schedule.postpone, Some(date(2026, 8, 10)));
        assert_eq!(next.schedule.due, Some(date(2026, 8, 14)));
    }

    #[test]
    fn a_due_only_task_advances_its_due_date_and_gains_no_postpone_date() {
        let rule = rule("FREQ=MONTHLY");
        let schedule = TaskSchedule::new(None, Some(date(2026, 8, 20)));
        let Advancement::Next(next) = advance(
            &rule,
            schedule,
            1,
            date(2026, 8, 20),
            AdvanceFrom::Scheduled,
        ) else {
            panic!("series continues");
        };
        assert_eq!(
            next.schedule,
            TaskSchedule::new(None, Some(date(2026, 9, 20)))
        );
    }

    #[test]
    fn a_dateless_recurring_task_is_anchored_on_its_completion() {
        let rule = rule("FREQ=DAILY;INTERVAL=2");
        let Advancement::Next(next) = advance(
            &rule,
            TaskSchedule::default(),
            1,
            date(2026, 8, 6),
            AdvanceFrom::Scheduled,
        ) else {
            panic!("series continues");
        };
        assert_eq!(next.schedule.postpone, Some(date(2026, 8, 8)));
        assert_eq!(next.schedule.due, None);
    }

    #[test]
    fn a_year_out_series_lands_on_the_right_civil_date() {
        // §14.1: a task scheduled a year out must appear on that exact date.
        let rule = rule("FREQ=YEARLY");
        let schedule = TaskSchedule::new(Some(date(2026, 8, 6)), None);
        let Advancement::Next(next) =
            advance(&rule, schedule, 1, date(2026, 8, 6), AdvanceFrom::Scheduled)
        else {
            panic!("series continues");
        };
        assert_eq!(next.anchor, date(2027, 8, 6));
    }

    // ---- previews -------------------------------------------------------

    #[test]
    fn occurrences_after_lists_the_series_and_stops_at_its_end() {
        assert_eq!(
            rule("FREQ=WEEKLY;BYDAY=MO,FR").occurrences_after(date(2026, 8, 3), 4),
            vec![
                date(2026, 8, 7),
                date(2026, 8, 10),
                date(2026, 8, 14),
                date(2026, 8, 17),
            ]
        );
        assert_eq!(
            rule("FREQ=DAILY;UNTIL=20260808").occurrences_after(date(2026, 8, 6), 5),
            vec![date(2026, 8, 7), date(2026, 8, 8)]
        );
    }

    // ---- reminders ------------------------------------------------------

    #[test]
    fn reminders_keep_their_day_offset_and_wall_clock_time() {
        let reminders = [
            at(2026, 8, 6, 9, 0),   // on the day
            at(2026, 8, 5, 18, 30), // the evening before
            at(2026, 7, 30, 7, 15), // a week ahead
        ];
        let shifted = shift_reminders(date(2026, 8, 6), date(2026, 9, 6), &reminders);
        assert_eq!(
            shifted,
            vec![
                at(2026, 9, 6, 9, 0),
                at(2026, 9, 5, 18, 30),
                at(2026, 8, 30, 7, 15),
            ]
        );
    }

    #[test]
    fn shifting_across_a_dst_change_keeps_the_local_time() {
        // Central European summer time ends on 2026-10-25. A 09:00 reminder is
        // 09:00 on both sides of it; adding 86_400 seconds would have made it
        // 08:00.
        let reminders = [at(2026, 10, 24, 9, 0)];
        assert_eq!(
            shift_reminders(date(2026, 10, 24), date(2026, 10, 26), &reminders),
            vec![at(2026, 10, 26, 9, 0)]
        );
    }

    #[test]
    fn shifting_backwards_is_symmetric() {
        let forward = shift_reminders(date(2026, 8, 6), date(2026, 9, 6), &[at(2026, 8, 5, 18, 0)]);
        let back = shift_reminders(date(2026, 9, 6), date(2026, 8, 6), &forward);
        assert_eq!(back, vec![at(2026, 8, 5, 18, 0)]);
    }

    #[test]
    fn a_reminder_offset_round_trips_through_its_anchor() {
        let offset = ReminderOffset::between(date(2026, 8, 6), at(2026, 8, 4, 21, 45));
        assert_eq!(offset.days, -2);
        assert_eq!(
            offset.apply(date(2027, 3, 1)),
            Some(at(2027, 2, 27, 21, 45))
        );
    }

    #[test]
    fn reminders_follow_a_task_that_advances_across_a_month_end() {
        let rule = rule("FREQ=MONTHLY;BYMONTHDAY=-1");
        let schedule = TaskSchedule::new(Some(date(2026, 1, 31)), None);
        let Advancement::Next(next) = advance(
            &rule,
            schedule,
            1,
            date(2026, 1, 31),
            AdvanceFrom::Scheduled,
        ) else {
            panic!("series continues");
        };
        assert_eq!(next.anchor, date(2026, 2, 28));

        let shifted = shift_reminders(date(2026, 1, 31), next.anchor, &[at(2026, 1, 30, 8, 0)]);
        assert_eq!(shifted, vec![at(2026, 2, 27, 8, 0)]);
    }

    // ---- calendar helpers ------------------------------------------------

    #[test]
    fn month_lengths_are_right_including_leap_februaries() {
        assert_eq!(last_day_of_month(2026, 2), Some(28));
        assert_eq!(last_day_of_month(2028, 2), Some(29));
        assert_eq!(last_day_of_month(2000, 2), Some(29));
        assert_eq!(last_day_of_month(2100, 2), Some(28));
        assert_eq!(last_day_of_month(2026, 12), Some(31));
        assert_eq!(last_day_of_month(2026, 4), Some(30));
    }

    #[test]
    fn negative_month_days_resolve_from_the_end_of_the_month() {
        assert_eq!(resolve_month_day(2026, 2, -1), Some(date(2026, 2, 28)));
        assert_eq!(resolve_month_day(2028, 2, -1), Some(date(2028, 2, 29)));
        assert_eq!(resolve_month_day(2026, 2, -28), Some(date(2026, 2, 1)));
        assert_eq!(resolve_month_day(2026, 2, -29), None);
        assert_eq!(resolve_month_day(2026, 2, 31), None);
        assert_eq!(resolve_month_day(2026, 2, 0), None);
    }

    #[test]
    fn nth_weekday_counts_from_both_ends() {
        // August 2026 has five Saturdays: 1, 8, 15, 22, 29.
        assert_eq!(
            nth_weekday_of_month(2026, 8, Weekday::Sat, 1),
            Some(date(2026, 8, 1))
        );
        assert_eq!(
            nth_weekday_of_month(2026, 8, Weekday::Sat, 5),
            Some(date(2026, 8, 29))
        );
        assert_eq!(
            nth_weekday_of_month(2026, 8, Weekday::Sat, -1),
            Some(date(2026, 8, 29))
        );
        assert_eq!(
            nth_weekday_of_month(2026, 8, Weekday::Sat, -5),
            Some(date(2026, 8, 1))
        );
        assert_eq!(nth_weekday_of_month(2026, 8, Weekday::Sat, 6), None);
        assert_eq!(nth_weekday_of_month(2026, 8, Weekday::Sat, -6), None);
    }

    #[test]
    fn a_recurrence_error_becomes_an_invalid_input_error() {
        let error: crate::Error = RecurrenceError::MissingFreq.into();
        assert!(matches!(error, crate::Error::InvalidInput(_)));
    }
}
