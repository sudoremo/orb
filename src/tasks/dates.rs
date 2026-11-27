//! Fuzzy date and time parsing for the task date picker (spec §12).
//!
//! The picker is a text box: the user types a fragment and gets a ranked list
//! of readings back. That shape dictates three properties this module has to
//! guarantee, and every design decision below follows from one of them.
//!
//! **Nothing is ever guessed.** Input that does not parse yields an empty
//! vector, which the UI renders as "no matching date". A date picker that
//! silently resolves `qq` to *today* is worse than one that resolves nothing,
//! because a wrong postpone date hides a task until a day the user never chose.
//! Every ambiguity is therefore surfaced as *two suggestions*, never resolved
//! by a coin flip: `12/9` under an English locale offers December 9 and
//! September 12, and a bare weekday typed on that same weekday offers both the
//! week-out and the today reading.
//!
//! **All arithmetic is on local civil dates.** Everything here works on
//! [`NaiveDate`] / [`NaiveDateTime`] and moves by [`Days`] and [`Months`],
//! never by an absolute duration on an instant. That is what makes the module
//! DST-safe: "tomorrow" on the day before a spring-forward is the next civil
//! date, not "24 hours later", which in CET would land at 01:00 on the wrong
//! side of a comparison. The existing task code compares dates via `and_utc()`
//! and gets exactly that wrong (spec §4.3); nothing in this module goes near a
//! timezone.
//!
//! **The reference "now" is injected.** Both entry points take the current date
//! (or date and time) as an argument rather than reading the clock, so the
//! whole surface is a pure function and the tests below pin today to
//! 2026-08-06 — the date the spec's example table is written against.
//!
//! # Labels
//!
//! Each suggestion carries a `primary` label and a `secondary` label; the UI
//! right-aligns the secondary in a lighter colour. The two always carry
//! *different* information, which is the rule that decides which is which:
//!
//! - When the user typed a relative phrase (`in 3 days`, `next tuesday`) the
//!   primary resolves it to a concrete date and the secondary shows the
//!   weekday. Echoing "in 3 days" back at someone who just typed it is noise.
//! - When the user typed something absolute (`sept 12`, `12.9.`, a bare day of
//!   the month) the primary is that date and the secondary answers the question
//!   the input leaves open — how far away it is. Within about a month the
//!   weekday is the useful answer; beyond that it is the distance ("in 5
//!   weeks"), because nobody can place a weekday five weeks out.
//! - Named days (`Today`, `Tomorrow`, `Monday`, `End of month`) keep the name as
//!   the primary and put the concrete date in the secondary.
//!
//! The spec's example table renders the secondary as a weekday even for the far
//! dates (`2026-09-12` → "Sat", `Sept 2028` → "Wed"), which contradicts the
//! prose one paragraph above it; the prose wins here, and its own example ("in
//! 5 weeks") is exactly the distance from 2026-08-06 to 2026-09-12, so the
//! table rows are the stale half of the spec.

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Weekday};

/// Time given to a reminder whose input named a day but no time (spec §5.3).
pub const DEFAULT_REMINDER_TIME: NaiveTime = match NaiveTime::from_hms_opt(9, 0, 0) {
    Some(time) => time,
    None => panic!("09:00 is a valid time"),
};

/// Shortest fragment that may match a keyword, month or weekday name.
///
/// One letter matches almost everything (`s` alone hits Saturday, Sunday,
/// September and "someday"), which turns the suggestion list into a menu of the
/// vocabulary rather than an answer. Two is enough to be an intent, and it is
/// also the length of the German weekday abbreviations (`Mo`, `Di`, `Mi`) that
/// have to keep working.
const MIN_PREFIX: usize = 2;

/// Distance at which the weekday stops being a useful secondary label.
///
/// Slightly more than a month: "Thu" tells you something about a date four
/// weeks out, and nothing at all about one five weeks out.
const NEAR_DATE_DAYS: i64 = 31;

/// Largest offset accepted for the "in n days" reading of a bare number.
///
/// A bare `2028` is a year, not a request to schedule 2028 days out, and
/// offering the latter would bury the reading the user actually wanted.
const MAX_BARE_DAY_OFFSET: u32 = 366;

/// Weekdays in the order the locale tables below are indexed (Monday first).
const WEEKDAYS: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

/// Language used for both parsing and rendering.
///
/// This is deliberately a two-value enum rather than a general locale: the
/// numeric date order and the month/weekday vocabulary are the only things that
/// vary, and both are enumerated here. Adding a language means adding a variant
/// and filling in the tables, with the compiler pointing at every table that
/// still needs a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateLocale {
    #[default]
    English,
    German,
}

/// One reading of the user's input.
///
/// `time` is `None` for date-only suggestions ([`suggest_dates`]) and always
/// `Some` for reminder suggestions ([`suggest_datetimes`]), so a caller can
/// tell "the user named a day" from "the user named a day and a time" without
/// re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateSuggestion {
    /// The resolved local civil date.
    pub date: NaiveDate,
    /// The resolved local wall-clock time, when the entry point produces one.
    pub time: Option<NaiveTime>,
    /// Left-aligned label, e.g. "Tomorrow" or "September 12, 2028".
    pub primary: String,
    /// Right-aligned label in a lighter colour, e.g. "Aug 7" or "in 5 weeks".
    pub secondary: String,
}

impl DateSuggestion {
    /// The suggestion as a civil date and time, defaulting to 09:00.
    pub fn datetime(&self) -> NaiveDateTime {
        self.date
            .and_time(self.time.unwrap_or(DEFAULT_REMINDER_TIME))
    }
}

/// Suggestions for a date-only picker (postpone and due dates).
///
/// `today` is the local civil date, injected so callers under test can pin it.
pub fn suggest_dates(input: &str, today: NaiveDate, locale: DateLocale) -> Vec<DateSuggestion> {
    let ctx = Ctx { today, locale };
    collect(&normalize(input), &ctx)
}

/// Suggestions for a reminder input, which additionally accepts a time.
///
/// Accepts everything [`suggest_dates`] does plus a trailing or leading time
/// (`tomorrow 9am`, `9:30 next tuesday`), a bare time (`14:00`, which means
/// today if that time is still ahead and tomorrow otherwise) and hour offsets
/// (`in 2 hours`), which only mean anything when there is a reference time to
/// offset from. Suggestions without an explicit time get
/// [`DEFAULT_REMINDER_TIME`].
///
/// Hour offsets are applied to the civil clock, because that is what a reminder
/// stores (spec §5: "a local date **and** time"). Across a DST transition the
/// wall clock and the elapsed duration cannot both be honoured, and a reminder
/// is a wall-clock promise.
pub fn suggest_datetimes(
    input: &str,
    now: NaiveDateTime,
    locale: DateLocale,
) -> Vec<DateSuggestion> {
    let ctx = Ctx {
        today: now.date(),
        locale,
    };
    let norm = normalize(input);
    if norm.is_empty() {
        return Vec::new();
    }

    // An hour offset resolves to a date *and* a time at once, so it cannot go
    // through the date/time split below.
    if let Some(offset) = parse_offset(&norm, locale)
        && offset.unit == Unit::Hour
        && let Some(target) = now.checked_add_signed(TimeDelta::hours(offset.amount))
    {
        return vec![DateSuggestion {
            date: target.date(),
            time: Some(target.time()),
            primary: offset_phrase(offset.amount, Unit::Hour, locale),
            secondary: format!(
                "{}, {}",
                format_short_date(target.date(), &ctx),
                format_time(target.time())
            ),
        }];
    }

    let (date_part, time) = split_time(&norm, locale);

    // A bare time means the next occurrence of that wall-clock time: today
    // while it is still ahead, tomorrow once it has passed. Offering a reminder
    // for 09:00 this morning at 14:00 would fire it instantly.
    if date_part.is_empty() {
        let Some(time) = time else {
            return Vec::new();
        };
        let (date, named) = if now.time() <= time {
            (ctx.today, Keyword::Today)
        } else {
            match ctx.today.checked_add_days(Days::new(1)) {
                Some(date) => (date, Keyword::Tomorrow),
                None => return Vec::new(),
            }
        };
        return vec![DateSuggestion {
            date,
            time: Some(time),
            primary: format!("{} {}", named.label(locale), format_time(time)),
            secondary: format_short_date(date, &ctx),
        }];
    }

    let time = time.unwrap_or(DEFAULT_REMINDER_TIME);
    collect(&date_part, &ctx)
        .into_iter()
        .map(|mut suggestion| {
            suggestion.primary = format!("{} {}", suggestion.primary, format_time(time));
            suggestion.time = Some(time);
            suggestion
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Against the day of a recurring task (todo-spec §32)
// ---------------------------------------------------------------------------

/// How far from its day something on a recurring task is, in words: "On the
/// day", "2 days after", "1 Tag vorher".
pub fn series_distance(days: i64, locale: DateLocale) -> String {
    let amount = days.abs();
    match (locale, days) {
        (DateLocale::English, 0) => "On the day".to_string(),
        (DateLocale::German, 0) => "Am Tag".to_string(),
        (DateLocale::English, _) => format!(
            "{amount} {} {}",
            if amount == 1 { "day" } else { "days" },
            if days < 0 { "before" } else { "after" }
        ),
        (DateLocale::German, _) => format!(
            "{amount} {} {}",
            if amount == 1 { "Tag" } else { "Tage" },
            if days < 0 { "vorher" } else { "später" }
        ),
    }
}

/// [`suggest_dates`] for the deadline of a recurring task whose occurrence is
/// on `day`.
///
/// What is typed may say where the deadline is against the day — `on the
/// day`, `2 days after`, `+3`, `am tag`, `1 woche später` — and such a reading
/// stands alone. A date said any other way is read as ever, and labelled with
/// how far from the day it is, since that distance is what the series keeps.
/// Nothing before the day is offered: a task is not due before it starts.
pub fn suggest_dates_in_series(
    input: &str,
    today: NaiveDate,
    day: NaiveDate,
    locale: DateLocale,
) -> Vec<DateSuggestion> {
    let ctx = Ctx { today, locale };
    let norm = normalize(input);
    let found = match parse_series_distance(&norm, locale) {
        Some(days) => in_series(day, days, None, &ctx).into_iter().collect(),
        None => relabelled(collect(&norm, &ctx), day, locale),
    };
    found.into_iter().filter(|s| s.date >= day).collect()
}

/// [`suggest_datetimes`] for a reminder of a recurring task whose occurrence
/// is on `day`: `on the day 9am`, `day before 18:00`, `-2 14:00`, `am vortag
/// 18 uhr`, and a time alone, which is that time on the day.
pub fn suggest_datetimes_in_series(
    input: &str,
    now: NaiveDateTime,
    day: NaiveDate,
    locale: DateLocale,
) -> Vec<DateSuggestion> {
    let ctx = Ctx {
        today: now.date(),
        locale,
    };
    let norm = normalize(input);
    let (date_part, time) = split_time(&norm, locale);
    let days = if date_part.is_empty() && time.is_some() {
        Some(0)
    } else {
        parse_series_distance(&date_part, locale)
    };
    match days {
        Some(days) => in_series(day, days, Some(time.unwrap_or(DEFAULT_REMINDER_TIME)), &ctx)
            .into_iter()
            .collect(),
        None => relabelled(suggest_datetimes(input, now, locale), day, locale),
    }
}

/// The suggestion `days` from the day of a series, named by that distance.
fn in_series(
    day: NaiveDate,
    days: i64,
    time: Option<NaiveTime>,
    ctx: &Ctx,
) -> Option<DateSuggestion> {
    let date = day.checked_add_signed(TimeDelta::try_days(days)?)?;
    let distance = series_distance(days, ctx.locale);
    Some(DateSuggestion {
        date,
        time,
        primary: match time {
            Some(time) => format!("{distance} {}", format_time(time)),
            None => distance,
        },
        secondary: format!(
            "{} {}",
            weekday_abbrev(date, ctx.locale),
            format_short_date(date, ctx)
        ),
    })
}

/// Suggestions read as plain dates, saying how far from the day each is.
fn relabelled(
    mut suggestions: Vec<DateSuggestion>,
    day: NaiveDate,
    locale: DateLocale,
) -> Vec<DateSuggestion> {
    for suggestion in &mut suggestions {
        suggestion.secondary = series_distance((suggestion.date - day).num_days(), locale);
    }
    suggestions
}

/// `on the day`, `day before`, `2 days after`, `1 week earlier`, `+3`, `-1`,
/// and in German `am tag`, `vortag`, `2 tage später`, `1 woche vorher`: whole
/// days from the day of a series. In either language whatever the locale, as
/// the words do not collide.
fn parse_series_distance(norm: &str, locale: DateLocale) -> Option<i64> {
    let mut parts = tokens(norm);
    while let Some(first) = parts.first()
        && matches!(*first, "on" | "the" | "am" | "an" | "dem" | "at")
    {
        parts.remove(0);
    }
    let before = |word: &str| {
        matches!(
            word,
            "before" | "earlier" | "prior" | "vorher" | "davor" | "zuvor" | "frueher"
        )
    };
    let after = |word: &str| {
        matches!(
            word,
            "after" | "later" | "danach" | "spaeter" | "nachher" | "darauf"
        )
    };
    let the_day = |word: &str| matches!(word, "day" | "tag");
    match parts.as_slice() {
        [day] if the_day(day) => return Some(0),
        ["same" | "selben" | "gleichen", day] | [day, "of" | "itself" | "selbst"]
            if the_day(day) =>
        {
            return Some(0);
        }
        ["vortag" | "eve"] => return Some(-1),
        ["folgetag"] => return Some(1),
        [day, direction] if the_day(day) && before(direction) => return Some(-1),
        [day, direction] if the_day(day) && after(direction) => return Some(1),
        _ => {}
    }

    // A signed number: `+3`, `-1`, `+2w`.
    if let [signed] = parts.as_slice()
        && let Some(sign) = signed.chars().next().filter(|c| matches!(c, '+' | '-'))
    {
        let rest = &signed[1..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let amount: i64 = digits.parse().ok()?;
        let days = match &rest[digits.len()..] {
            "" => amount,
            unit => match parse_unit(unit, locale)? {
                Unit::Day => amount,
                Unit::Week => amount.checked_mul(7)?,
                _ => return None,
            },
        };
        return Some(if sign == '-' { -days } else { days });
    }

    // A number, a unit and a direction: `2 days after`, `1 woche vorher`.
    let [amount, unit, direction] = parts.as_slice() else {
        return None;
    };
    let amount: i64 = amount.parse().ok()?;
    let unit = [DateLocale::English, DateLocale::German]
        .into_iter()
        .find_map(|locale| parse_unit(unit, locale))?;
    let days = match unit {
        Unit::Day => amount,
        Unit::Week => amount.checked_mul(7)?,
        _ => return None,
    };
    if before(direction) {
        Some(-days)
    } else if after(direction) {
        Some(days)
    } else {
        None
    }
}

/// Everything the producers need to resolve and render a suggestion.
struct Ctx {
    today: NaiveDate,
    locale: DateLocale,
}

/// Which of the two labels carries the "how far away is this" information.
#[derive(Clone, Copy)]
enum Secondary {
    /// The concrete date, for suggestions whose primary is a name.
    ShortDate,
    /// The weekday, for suggestions whose primary is a date the user described
    /// relatively and therefore already knows the distance to.
    Weekday,
    /// The weekday while the date is near, the distance once it is not.
    Distance,
}

/// Run every producer in ranking order.
///
/// The order *is* the ranking: producers are tried most-specific-intent first,
/// and within a producer the readings are pushed most-likely first. Nothing is
/// re-sorted afterwards, because a global sort by date would put "in 3 days"
/// above "September 3" for the input `3` and the spec wants the day-of-month
/// reading first.
fn collect(norm: &str, ctx: &Ctx) -> Vec<DateSuggestion> {
    let mut out = Vec::new();
    if norm.is_empty() {
        return out;
    }

    push_weekdays(norm, ctx, &mut out);
    push_keywords(norm, ctx, &mut out);
    push_offset(norm, ctx, &mut out);
    push_month_forms(norm, ctx, &mut out);
    push_bare_number(norm, ctx, &mut out);
    push_numeric_date(norm, ctx, &mut out);
    out
}

/// Push a suggestion unless the same instant is already suggested.
///
/// Two producers can legitimately reach the same date from one input (a
/// weekday name that is also the end of the month, say). Showing it twice under
/// two different names reads as a bug, and the first producer to reach it is by
/// construction the better-ranked reading.
fn push(
    out: &mut Vec<DateSuggestion>,
    date: NaiveDate,
    primary: String,
    sec: Secondary,
    ctx: &Ctx,
) {
    if out.iter().any(|existing| existing.date == date) {
        return;
    }
    let secondary = match sec {
        Secondary::ShortDate => format_short_date(date, ctx),
        Secondary::Weekday => weekday_abbrev(date, ctx.locale).to_string(),
        Secondary::Distance => {
            if (date - ctx.today).num_days().abs() <= NEAR_DATE_DAYS {
                weekday_abbrev(date, ctx.locale).to_string()
            } else {
                relative_distance(date, ctx.today, ctx.locale)
            }
        }
    };
    out.push(DateSuggestion {
        date,
        time: None,
        primary,
        secondary,
    });
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// Fold the input into the shape the tables are written in.
///
/// Lowercased, whitespace collapsed, commas dropped so that a suggestion's own
/// label ("September 12, 2028") parses back to the date it came from, and
/// umlauts expanded to their `ae`/`oe`/`ue` spellings so `märz` and `maerz` are
/// the same token. The folding is one-directional on purpose: mapping `ue` to
/// `u` instead would eat the middle of "t**ue**sday".
pub(crate) fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.trim().chars() {
        match ch {
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            ',' => {}
            c if c.is_whitespace() => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            c => out.extend(c.to_lowercase()),
        }
    }
    out.trim_end().to_string()
}

fn tokens(norm: &str) -> Vec<&str> {
    norm.split(' ').filter(|t| !t.is_empty()).collect()
}

/// Whether `fragment` is an acceptable abbreviation of `alias`.
fn prefix_match(alias: &str, fragment: &str, min_len: usize) -> bool {
    fragment.len() >= min_len && alias.starts_with(fragment)
}

// ---------------------------------------------------------------------------
// Keywords
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Today,
    Tomorrow,
    NextWeek,
    NextMonth,
    EndOfMonth,
    Weekend,
}

impl Keyword {
    const ALL: [Keyword; 6] = [
        Keyword::Today,
        Keyword::Tomorrow,
        Keyword::NextWeek,
        Keyword::NextMonth,
        Keyword::EndOfMonth,
        Keyword::Weekend,
    ];

    /// Spellings accepted for this keyword, already normalised.
    fn aliases(self, locale: DateLocale) -> &'static [&'static str] {
        match (locale, self) {
            (DateLocale::English, Keyword::Today) => &["today"],
            (DateLocale::English, Keyword::Tomorrow) => &["tomorrow"],
            (DateLocale::English, Keyword::NextWeek) => &["next week"],
            (DateLocale::English, Keyword::NextMonth) => &["next month"],
            (DateLocale::English, Keyword::EndOfMonth) => &["eom", "end of month"],
            (DateLocale::English, Keyword::Weekend) => &["weekend", "this weekend"],
            (DateLocale::German, Keyword::Today) => &["heute"],
            (DateLocale::German, Keyword::Tomorrow) => &["morgen"],
            (DateLocale::German, Keyword::NextWeek) => &[
                "naechste woche",
                "nachste woche",
                "kommende woche",
                "naechsten woche",
            ],
            (DateLocale::German, Keyword::NextMonth) => &[
                "naechster monat",
                "naechsten monat",
                "nachster monat",
                "kommender monat",
            ],
            (DateLocale::German, Keyword::EndOfMonth) => &["monatsende", "ende des monats", "eom"],
            (DateLocale::German, Keyword::Weekend) => &["wochenende"],
        }
    }

    /// The label used when the keyword itself is the primary label.
    fn label(self, locale: DateLocale) -> &'static str {
        match (locale, self) {
            (DateLocale::English, Keyword::Today) => "Today",
            (DateLocale::English, Keyword::Tomorrow) => "Tomorrow",
            (DateLocale::English, Keyword::EndOfMonth) => "End of month",
            (DateLocale::German, Keyword::Today) => "Heute",
            (DateLocale::German, Keyword::Tomorrow) => "Morgen",
            (DateLocale::German, Keyword::EndOfMonth) => "Monatsende",
            // The remaining keywords resolve to a named day or a plain date and
            // never render their own name.
            _ => "",
        }
    }
}

fn push_keywords(norm: &str, ctx: &Ctx, out: &mut Vec<DateSuggestion>) {
    for keyword in Keyword::ALL {
        if !keyword
            .aliases(ctx.locale)
            .iter()
            .any(|alias| prefix_match(alias, norm, MIN_PREFIX))
        {
            continue;
        }
        match keyword {
            Keyword::Today => push(
                out,
                ctx.today,
                keyword.label(ctx.locale).to_string(),
                Secondary::ShortDate,
                ctx,
            ),
            Keyword::Tomorrow => {
                if let Some(date) = ctx.today.checked_add_days(Days::new(1)) {
                    push(
                        out,
                        date,
                        keyword.label(ctx.locale).to_string(),
                        Secondary::ShortDate,
                        ctx,
                    );
                }
            }
            // "Next week" is Monday, the day a week starts on for planning
            // purposes regardless of what the calendar widget draws first.
            Keyword::NextWeek => {
                let date = upcoming_weekday(ctx.today, Weekday::Mon, false);
                push(
                    out,
                    date,
                    weekday_name(date, ctx.locale).to_string(),
                    Secondary::ShortDate,
                    ctx,
                );
            }
            Keyword::NextMonth => {
                if let Some(date) = first_of_next_month(ctx.today) {
                    push(
                        out,
                        date,
                        format_long_date(date, ctx),
                        Secondary::Weekday,
                        ctx,
                    );
                }
            }
            Keyword::EndOfMonth => {
                if let Some(date) = end_of_month(ctx.today) {
                    push(
                        out,
                        date,
                        keyword.label(ctx.locale).to_string(),
                        Secondary::ShortDate,
                        ctx,
                    );
                }
            }
            // The weekend starts on Saturday; typed *on* a Saturday it means
            // today, which is why `allow_today` is set here and not for the
            // bare weekday reading.
            Keyword::Weekend => {
                let date = upcoming_weekday(ctx.today, Weekday::Sat, true);
                push(
                    out,
                    date,
                    weekday_name(date, ctx.locale).to_string(),
                    Secondary::ShortDate,
                    ctx,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Weekdays
// ---------------------------------------------------------------------------

/// Words that turn a weekday into "next <weekday>".
fn next_words(locale: DateLocale) -> &'static [&'static str] {
    match locale {
        DateLocale::English => &["next"],
        DateLocale::German => &[
            "naechste",
            "naechsten",
            "naechster",
            "nachste",
            "nachsten",
            "kommende",
            "kommenden",
        ],
    }
}

fn weekday_matches(fragment: &str, locale: DateLocale, min_len: usize) -> Vec<Weekday> {
    let mut found = Vec::new();
    for (index, aliases) in weekday_aliases(locale).iter().enumerate() {
        if aliases
            .iter()
            .any(|alias| prefix_match(alias, fragment, min_len))
        {
            found.push(WEEKDAYS[index]);
        }
    }
    found
}

/// Weekday names, bare and `next`-prefixed.
///
/// A bare weekday means the next one, and "next tuesday" means the same day —
/// the two forms only diverge when today *is* that weekday, where "tuesday"
/// could plausibly mean either today or this day next week. Both are then
/// offered, a week out first: someone who means today reaches for "today", so
/// naming the weekday on that weekday is much more often the next one.
fn push_weekdays(norm: &str, ctx: &Ctx, out: &mut Vec<DateSuggestion>) {
    let parts = tokens(norm);
    let (fragment, explicit_next, min_len) = match parts.as_slice() {
        [single] => (*single, false, MIN_PREFIX),
        [first, second]
            if next_words(ctx.locale)
                .iter()
                .any(|word| prefix_match(word, first, MIN_PREFIX)) =>
        {
            // "next " has already established the intent, so a single letter is
            // enough to pick the weekday out.
            (*second, true, 1)
        }
        _ => return,
    };

    for weekday in weekday_matches(fragment, ctx.locale, min_len) {
        let is_today = ctx.today.weekday() == weekday;
        let week_out = upcoming_weekday(ctx.today, weekday, false);

        if explicit_next {
            // "next tuesday" is the coming Tuesday, or a week out when today is
            // Tuesday — which `upcoming_weekday` already produces by refusing
            // to return today.
            push(
                out,
                week_out,
                format_long_date(week_out, ctx),
                Secondary::Weekday,
                ctx,
            );
            continue;
        }

        push(
            out,
            week_out,
            weekday_name(week_out, ctx.locale).to_string(),
            Secondary::ShortDate,
            ctx,
        );
        if is_today {
            // Rendered as a plain date rather than a second "Tuesday", which
            // next to the first suggestion would look like a duplicate.
            push(
                out,
                ctx.today,
                format_long_date(ctx.today, ctx),
                Secondary::Distance,
                ctx,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Relative offsets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Day,
    Week,
    Month,
    Year,
    Hour,
}

impl Unit {
    const ALL: [Unit; 5] = [Unit::Day, Unit::Week, Unit::Month, Unit::Year, Unit::Hour];

    fn aliases(self, locale: DateLocale) -> &'static [&'static str] {
        match (locale, self) {
            (DateLocale::English, Unit::Day) => &["d", "day", "days"],
            (DateLocale::English, Unit::Week) => &["w", "week", "weeks"],
            (DateLocale::English, Unit::Month) => &["m", "mo", "month", "months"],
            (DateLocale::English, Unit::Year) => &["y", "yr", "year", "years"],
            (DateLocale::English, Unit::Hour) => &["h", "hr", "hour", "hours"],
            (DateLocale::German, Unit::Day) => &["t", "tag", "tage", "tagen"],
            (DateLocale::German, Unit::Week) => &["w", "woche", "wochen"],
            (DateLocale::German, Unit::Month) => &["m", "monat", "monate", "monaten"],
            (DateLocale::German, Unit::Year) => &["j", "jahr", "jahre", "jahren"],
            (DateLocale::German, Unit::Hour) => &["h", "std", "stunde", "stunden"],
        }
    }

    /// Singular and plural forms used when rendering a distance.
    fn words(self, locale: DateLocale) -> (&'static str, &'static str) {
        match (locale, self) {
            (DateLocale::English, Unit::Day) => ("day", "days"),
            (DateLocale::English, Unit::Week) => ("week", "weeks"),
            (DateLocale::English, Unit::Month) => ("month", "months"),
            (DateLocale::English, Unit::Year) => ("year", "years"),
            (DateLocale::English, Unit::Hour) => ("hour", "hours"),
            // German renders these in the dative ("in 3 Tagen", "vor 3 Tagen"),
            // which is the only case the phrase ever appears in.
            (DateLocale::German, Unit::Day) => ("Tag", "Tagen"),
            (DateLocale::German, Unit::Week) => ("Woche", "Wochen"),
            (DateLocale::German, Unit::Month) => ("Monat", "Monaten"),
            (DateLocale::German, Unit::Year) => ("Jahr", "Jahren"),
            (DateLocale::German, Unit::Hour) => ("Stunde", "Stunden"),
        }
    }
}

fn parse_unit(fragment: &str, locale: DateLocale) -> Option<Unit> {
    Unit::ALL.into_iter().find(|unit| {
        unit.aliases(locale)
            .iter()
            .any(|alias| prefix_match(alias, fragment, 1))
    })
}

struct Offset {
    amount: i64,
    unit: Unit,
    /// True for the `+3` form, which keeps "in 3 days" as its primary label
    /// rather than resolving to a date. The plus sign is a request for a
    /// distance, so the distance is what gets echoed.
    keep_phrase: bool,
}

/// `in 3 days`, `3 days`, `3d`, `+3`, and the German equivalents.
fn parse_offset(norm: &str, locale: DateLocale) -> Option<Offset> {
    let mut parts = tokens(norm);
    if parts.is_empty() {
        return None;
    }
    // "in" is the same word in both languages, which is a coincidence worth
    // taking.
    let had_in = parts[0] == "in";
    if had_in {
        parts.remove(0);
        if parts.is_empty() {
            return None;
        }
    }

    let rest = parts.concat();
    let (rest, keep_phrase) = match rest.strip_prefix('+') {
        Some(rest) => (rest, true),
        None => (rest.as_str(), false),
    };

    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let amount: i64 = digits.parse().ok()?;
    // A zero offset is a slip of the fingers, not a request: "today" is shorter
    // to type than "0d", and resolving it anyway would put a suggestion under a
    // half-typed number.
    if amount == 0 {
        return None;
    }
    let unit_fragment = &rest[digits.len()..];

    // A bare number on its own is a day of the month first and an offset
    // second; that reading belongs to `push_bare_number`, which offers both.
    if unit_fragment.is_empty() && !had_in && !keep_phrase {
        return None;
    }

    let unit = if unit_fragment.is_empty() {
        Unit::Day
    } else {
        parse_unit(unit_fragment, locale)?
    };

    Some(Offset {
        amount,
        unit,
        keep_phrase,
    })
}

fn push_offset(norm: &str, ctx: &Ctx, out: &mut Vec<DateSuggestion>) {
    let Some(offset) = parse_offset(norm, ctx.locale) else {
        return;
    };
    if offset.unit == Unit::Hour {
        // An hour offset is an instant, not a date, so it is resolved by
        // `suggest_datetimes` against its reference time and never here. The
        // date picker has no time to offset from, and rounding "in 2 hours"
        // to a bare date would answer a question nobody asked.
        return;
    }
    let Some(date) = shift(ctx.today, offset.amount, offset.unit) else {
        return;
    };

    if offset.keep_phrase {
        push(
            out,
            date,
            offset_phrase(offset.amount, offset.unit, ctx.locale),
            Secondary::ShortDate,
            ctx,
        );
    } else {
        push(
            out,
            date,
            format_long_date(date, ctx),
            Secondary::Weekday,
            ctx,
        );
    }
}

/// Move a civil date by whole days, weeks, months or years.
///
/// Month and year steps clamp (31 January plus one month is 28 February), which
/// is what `checked_add_months` does and what every calendar app does.
fn shift(from: NaiveDate, amount: i64, unit: Unit) -> Option<NaiveDate> {
    let amount_u32 = u32::try_from(amount).ok()?;
    match unit {
        Unit::Day => from.checked_add_days(Days::new(u64::try_from(amount).ok()?)),
        Unit::Week => from.checked_add_days(Days::new(u64::try_from(amount).ok()?.checked_mul(7)?)),
        Unit::Month => from.checked_add_months(Months::new(amount_u32)),
        Unit::Year => from.checked_add_months(Months::new(amount_u32.checked_mul(12)?)),
        // Deliberately not "about a day": an hour offset has no civil-date
        // answer, and the caller that can answer it does so itself.
        Unit::Hour => None,
    }
}

fn offset_phrase(amount: i64, unit: Unit, locale: DateLocale) -> String {
    let (singular, plural) = unit.words(locale);
    let word = if amount == 1 { singular } else { plural };
    format!("in {amount} {word}")
}

// ---------------------------------------------------------------------------
// Bare numbers
// ---------------------------------------------------------------------------

/// A bare number has two readings and the spec wants both.
///
/// Day-of-month first — typing `3` on the 6th almost always means "the third",
/// and the "in 3 days" reading is a convenience for the small numbers where it
/// is plausible.
fn push_bare_number(norm: &str, ctx: &Ctx, out: &mut Vec<DateSuggestion>) {
    if norm.is_empty() || !norm.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    let Ok(number) = norm.parse::<u32>() else {
        return;
    };

    if (1..=31).contains(&number)
        && let Some(date) = next_day_of_month(ctx.today, number)
    {
        push(
            out,
            date,
            format_long_date(date, ctx),
            Secondary::Distance,
            ctx,
        );
    }

    if (1..=MAX_BARE_DAY_OFFSET).contains(&number)
        && let Some(date) = ctx.today.checked_add_days(Days::new(u64::from(number)))
    {
        push(
            out,
            date,
            offset_phrase(i64::from(number), Unit::Day, ctx.locale),
            Secondary::ShortDate,
            ctx,
        );
    }
}

// ---------------------------------------------------------------------------
// Month names
// ---------------------------------------------------------------------------

/// `sept`, `sept 12`, `12. september`, `sept 2028`, `sept 2028 12`.
///
/// Exactly one alphabetic token is allowed, and every other token has to be a
/// number: a number below 32 is a day, a four-digit number is a year, and
/// anything else makes the whole input unparseable rather than half-understood.
fn push_month_forms(norm: &str, ctx: &Ctx, out: &mut Vec<DateSuggestion>) {
    let mut month_fragment: Option<&str> = None;
    let mut day: Option<u32> = None;
    let mut year: Option<i32> = None;

    for token in tokens(norm) {
        // German writes ordinals with a trailing dot ("12. September").
        let token = token.trim_end_matches('.');
        if token.is_empty() {
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            let Ok(number) = token.parse::<u32>() else {
                return;
            };
            if token.len() >= 4 {
                if year.replace(number as i32).is_some() {
                    return;
                }
            } else if (1..=31).contains(&number) {
                if day.replace(number).is_some() {
                    return;
                }
            } else {
                return;
            }
        } else if token.chars().all(char::is_alphabetic) {
            if month_fragment.replace(token).is_some() {
                return;
            }
        } else {
            return;
        }
    }

    let Some(fragment) = month_fragment else {
        return;
    };

    for month in month_matches(fragment, ctx.locale) {
        // Without a year, roll forward to the next occurrence: a month that has
        // already gone by this year is never what someone is scheduling. With a
        // year, the user said what they meant, including a past date.
        let resolved = match year {
            Some(year) => NaiveDate::from_ymd_opt(year, month, day.unwrap_or(1)),
            None => next_occurrence(ctx.today, month, day.unwrap_or(1)),
        };
        if let Some(date) = resolved {
            push(
                out,
                date,
                format_long_date(date, ctx),
                Secondary::Distance,
                ctx,
            );
        }
    }
}

fn month_matches(fragment: &str, locale: DateLocale) -> Vec<u32> {
    let mut found = Vec::new();
    for (index, aliases) in month_aliases(locale).iter().enumerate() {
        if aliases
            .iter()
            .any(|alias| prefix_match(alias, fragment, MIN_PREFIX))
        {
            found.push(index as u32 + 1);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Numeric dates
// ---------------------------------------------------------------------------

/// `2026-09-12`, `12.9.`, `12.9.2026`, `12/9`, `9/12/2026`.
///
/// The separator decides the field order, not the locale alone:
///
/// - `-` is ISO and therefore always year-month-day;
/// - `.` is the European dotted form and therefore always day-first — nobody
///   writes `9.12.` meaning December 9;
/// - `/` is the genuinely ambiguous one. The locale picks the preferred
///   reading, and the other one is offered as well whenever it is a valid and
///   different date, because `12/9` means opposite things on opposite sides of
///   an ocean and guessing loses.
fn push_numeric_date(norm: &str, ctx: &Ctx, out: &mut Vec<DateSuggestion>) {
    let compact: String = norm.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty()
        || !compact
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '.' | '/'))
    {
        return;
    }

    let separator = match (
        compact.contains('-'),
        compact.contains('.'),
        compact.contains('/'),
    ) {
        (true, false, false) => '-',
        (false, true, false) => '.',
        (false, false, true) => '/',
        _ => return,
    };

    let fields: Vec<&str> = compact
        .split(separator)
        .filter(|field| !field.is_empty())
        .collect();
    if !(2..=3).contains(&fields.len()) {
        return;
    }
    let numbers: Vec<u32> = match fields.iter().map(|f| f.parse::<u32>()).collect() {
        Ok(numbers) => numbers,
        Err(_) => return,
    };

    match separator {
        '-' => {
            // ISO only: a four-digit leading year is the whole point of the
            // format, and `9-12` is not a date anyone typed on purpose.
            if numbers.len() == 3
                && fields[0].len() == 4
                && let Some(date) =
                    NaiveDate::from_ymd_opt(numbers[0] as i32, numbers[1], numbers[2])
            {
                push(
                    out,
                    date,
                    format_long_date(date, ctx),
                    Secondary::Distance,
                    ctx,
                );
            }
        }
        '.' => push_day_month(out, ctx, numbers[0], numbers[1], numbers.get(2).copied()),
        _ => {
            let (day, month) = if ctx.locale == DateLocale::German {
                (numbers[0], numbers[1])
            } else {
                (numbers[1], numbers[0])
            };
            push_day_month(out, ctx, day, month, numbers.get(2).copied());
            // The other reading, for the days when the input came from someone
            // on the other convention.
            push_day_month(out, ctx, month, day, numbers.get(2).copied());
        }
    }
}

fn push_day_month(
    out: &mut Vec<DateSuggestion>,
    ctx: &Ctx,
    day: u32,
    month: u32,
    year: Option<u32>,
) {
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return;
    }
    let date = match year {
        // Two digits are this century: 26 is 2026, not 26 AD.
        Some(year) if year < 100 => NaiveDate::from_ymd_opt(2000 + year as i32, month, day),
        Some(year) => NaiveDate::from_ymd_opt(year as i32, month, day),
        None => next_occurrence(ctx.today, month, day),
    };
    if let Some(date) = date {
        push(
            out,
            date,
            format_long_date(date, ctx),
            Secondary::Distance,
            ctx,
        );
    }
}

// ---------------------------------------------------------------------------
// Times
// ---------------------------------------------------------------------------

/// Peel a time off either end of the input.
///
/// Tried as a suffix first ("tomorrow 9am") and then as a prefix ("9am
/// tomorrow"), one and two tokens at a time so that "9 am" and "14 uhr" work.
/// A prefix time is only accepted when something is left over, otherwise a bare
/// "9am" would be split into a time plus an empty date twice.
fn split_time(norm: &str, locale: DateLocale) -> (String, Option<NaiveTime>) {
    let parts = tokens(norm);
    for take in [2usize, 1] {
        if parts.len() >= take
            && let Some(time) = parse_time(&parts[parts.len() - take..].join(" "), locale)
        {
            return (parts[..parts.len() - take].join(" "), Some(time));
        }
    }
    for take in [2usize, 1] {
        if parts.len() > take
            && let Some(time) = parse_time(&parts[..take].join(" "), locale)
        {
            return (parts[take..].join(" "), Some(time));
        }
    }
    (norm.to_string(), None)
}

/// `14:00`, `9am`, `9:30 pm`, `14 uhr`.
///
/// A bare number is never a time. `12` in a task input is the twelfth far more
/// often than it is noon, and the day-of-month reading is the one that would be
/// silently lost.
fn parse_time(fragment: &str, locale: DateLocale) -> Option<NaiveTime> {
    let compact: String = fragment.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }

    let mut explicit = false;
    let mut half: Option<bool> = None;
    let mut rest = compact.as_str();
    if let Some(stripped) = rest.strip_suffix("am") {
        half = Some(false);
        rest = stripped;
    } else if let Some(stripped) = rest.strip_suffix("pm") {
        half = Some(true);
        rest = stripped;
    } else if locale == DateLocale::German
        && let Some(stripped) = rest.strip_suffix("uhr")
    {
        explicit = true;
        rest = stripped;
    }
    let rest = rest.trim_end_matches(['.', ':']);

    // The German "14.00 Uhr" spelling only becomes unambiguous once "Uhr" is
    // there; without it a dot is a date separator.
    let separator = if rest.contains(':') {
        Some(':')
    } else if explicit && rest.contains('.') {
        Some('.')
    } else {
        None
    };

    let (hour_text, minute_text) = match separator {
        Some(separator) => {
            let (hour, minute) = rest.split_once(separator)?;
            (hour, minute)
        }
        None => (rest, "0"),
    };
    if separator.is_none() && half.is_none() && !explicit {
        return None;
    }
    if hour_text.is_empty() || !hour_text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if minute_text.is_empty() || !minute_text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let mut hour: u32 = hour_text.parse().ok()?;
    let minute: u32 = minute_text.parse().ok()?;
    match half {
        Some(_) if !(1..=12).contains(&hour) => return None,
        Some(true) if hour < 12 => hour += 12,
        Some(false) if hour == 12 => hour = 0,
        _ => {}
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn format_time(time: NaiveTime) -> String {
    time.format("%H:%M").to_string()
}

// ---------------------------------------------------------------------------
// Civil date arithmetic
// ---------------------------------------------------------------------------

/// The next `weekday` strictly after `from`, or `from` itself when
/// `allow_today` and it already is that weekday.
fn upcoming_weekday(from: NaiveDate, weekday: Weekday, allow_today: bool) -> NaiveDate {
    let delta = (i64::from(weekday.num_days_from_monday())
        - i64::from(from.weekday().num_days_from_monday()))
    .rem_euclid(7);
    let delta = if delta == 0 && !allow_today { 7 } else { delta };
    from + Days::new(delta as u64)
}

/// The next date with this day of the month, today included.
///
/// Skips months that are too short, so `31` in February is 31 March and not a
/// clamped 28 February — the user named a specific day, and clamping would
/// silently produce a different one.
fn next_day_of_month(today: NaiveDate, day: u32) -> Option<NaiveDate> {
    let mut anchor = today.with_day(1)?;
    for _ in 0..14 {
        if let Some(date) = anchor.with_day(day)
            && date >= today
        {
            return Some(date);
        }
        anchor = anchor.checked_add_months(Months::new(1))?;
    }
    None
}

/// The next occurrence of a month and day, today included.
///
/// Walks forward year by year rather than adding one, so 29 February resolves
/// to the next leap year instead of vanishing.
fn next_occurrence(today: NaiveDate, month: u32, day: u32) -> Option<NaiveDate> {
    for offset in 0..8 {
        if let Some(date) = NaiveDate::from_ymd_opt(today.year() + offset, month, day)
            && date >= today
        {
            return Some(date);
        }
    }
    None
}

fn first_of_next_month(today: NaiveDate) -> Option<NaiveDate> {
    today.with_day(1)?.checked_add_months(Months::new(1))
}

fn end_of_month(today: NaiveDate) -> Option<NaiveDate> {
    first_of_next_month(today)?.pred_opt()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub(crate) fn month_aliases(locale: DateLocale) -> [&'static [&'static str]; 12] {
    match locale {
        DateLocale::English => [
            &["january"],
            &["february"],
            &["march"],
            &["april"],
            &["may"],
            &["june"],
            &["july"],
            &["august"],
            &["september"],
            &["october"],
            &["november"],
            &["december"],
        ],
        DateLocale::German => [
            &["januar", "jaenner"],
            &["februar"],
            // "maerz" is the normalised spelling of "März"; "marz" catches the
            // umlaut-less typing people fall back on.
            &["maerz", "marz"],
            &["april"],
            &["mai"],
            &["juni"],
            &["juli"],
            &["august"],
            &["september"],
            &["oktober"],
            &["november"],
            &["dezember"],
        ],
    }
}

fn month_name(month: u32, locale: DateLocale) -> &'static str {
    let index = (month as usize).saturating_sub(1).min(11);
    match locale {
        DateLocale::English => [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ][index],
        DateLocale::German => [
            "Januar",
            "Februar",
            "März",
            "April",
            "Mai",
            "Juni",
            "Juli",
            "August",
            "September",
            "Oktober",
            "November",
            "Dezember",
        ][index],
    }
}

fn month_abbrev(month: u32, locale: DateLocale) -> &'static str {
    let index = (month as usize).saturating_sub(1).min(11);
    match locale {
        DateLocale::English => [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ][index],
        DateLocale::German => [
            "Jan", "Feb", "Mär", "Apr", "Mai", "Jun", "Jul", "Aug", "Sep", "Okt", "Nov", "Dez",
        ][index],
    }
}

pub(crate) fn weekday_aliases(locale: DateLocale) -> [&'static [&'static str]; 7] {
    match locale {
        DateLocale::English => [
            &["monday"],
            &["tuesday"],
            &["wednesday"],
            &["thursday"],
            &["friday"],
            &["saturday"],
            &["sunday"],
        ],
        DateLocale::German => [
            &["montag"],
            &["dienstag"],
            &["mittwoch"],
            &["donnerstag"],
            &["freitag"],
            &["samstag"],
            &["sonntag"],
        ],
    }
}

fn weekday_name(date: NaiveDate, locale: DateLocale) -> &'static str {
    let index = date.weekday().num_days_from_monday() as usize;
    match locale {
        DateLocale::English => [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ][index],
        DateLocale::German => [
            "Montag",
            "Dienstag",
            "Mittwoch",
            "Donnerstag",
            "Freitag",
            "Samstag",
            "Sonntag",
        ][index],
    }
}

fn weekday_abbrev(date: NaiveDate, locale: DateLocale) -> &'static str {
    let index = date.weekday().num_days_from_monday() as usize;
    match locale {
        DateLocale::English => ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][index],
        DateLocale::German => ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"][index],
    }
}

/// Whether a rendered date has to spell out its year.
///
/// The year is dropped for the common case — a date later this year — so the
/// label stays short and the presence of a year is itself the signal that the
/// date is unusual. It comes back for anything in the past, including earlier
/// this year: "January 3" in December is both misleading and no longer valid
/// input, since re-typing it would resolve to next January.
fn needs_year(date: NaiveDate, today: NaiveDate) -> bool {
    date.year() != today.year() || date < today
}

/// "September 12", "September 12, 2028" — or "12. September 2028" in German.
fn format_long_date(date: NaiveDate, ctx: &Ctx) -> String {
    let month = month_name(date.month(), ctx.locale);
    let same_year = !needs_year(date, ctx.today);
    match ctx.locale {
        DateLocale::English if same_year => format!("{month} {}", date.day()),
        DateLocale::English => format!("{month} {}, {}", date.day(), date.year()),
        DateLocale::German if same_year => format!("{}. {month}", date.day()),
        DateLocale::German => format!("{}. {month} {}", date.day(), date.year()),
    }
}

/// "Aug 7", "Aug 7, 2027" — or "7. Aug 2027" in German.
fn format_short_date(date: NaiveDate, ctx: &Ctx) -> String {
    let month = month_abbrev(date.month(), ctx.locale);
    let same_year = !needs_year(date, ctx.today);
    match ctx.locale {
        DateLocale::English if same_year => format!("{month} {}", date.day()),
        DateLocale::English => format!("{month} {}, {}", date.day(), date.year()),
        DateLocale::German if same_year => format!("{}. {month}", date.day()),
        DateLocale::German => format!("{}. {month} {}", date.day(), date.year()),
    }
}

/// "in 5 weeks", "in 3 months", "2 years ago".
///
/// The unit is chosen by magnitude: exact days while they are still countable,
/// then weeks, then months, then years. The point is a sense of scale, not
/// precision — the exact date is already in the primary label.
fn relative_distance(date: NaiveDate, today: NaiveDate, locale: DateLocale) -> String {
    let days = (date - today).num_days();
    let magnitude = days.abs();
    let (amount, unit) = if magnitude < 7 {
        (magnitude, Unit::Day)
    } else if magnitude < 45 {
        ((magnitude as f64 / 7.0).round() as i64, Unit::Week)
    } else if magnitude < 365 {
        ((magnitude as f64 / 30.44).round() as i64, Unit::Month)
    } else {
        ((magnitude as f64 / 365.25).round() as i64, Unit::Year)
    };

    let (singular, plural) = unit.words(locale);
    let word = if amount == 1 { singular } else { plural };
    match (locale, days < 0) {
        (_, false) => format!("in {amount} {word}"),
        (DateLocale::English, true) => format!("{amount} {word} ago"),
        (DateLocale::German, true) => format!("vor {amount} {word}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The date the spec's example table is written against: a Thursday.
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 6).unwrap()
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn time(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn en(input: &str) -> Vec<DateSuggestion> {
        suggest_dates(input, today(), DateLocale::English)
    }

    fn de(input: &str) -> Vec<DateSuggestion> {
        suggest_dates(input, today(), DateLocale::German)
    }

    /// (date, primary, secondary) of every suggestion, for terse assertions.
    fn triples(suggestions: &[DateSuggestion]) -> Vec<(NaiveDate, String, String)> {
        suggestions
            .iter()
            .map(|s| (s.date, s.primary.clone(), s.secondary.clone()))
            .collect()
    }

    fn dates(suggestions: &[DateSuggestion]) -> Vec<NaiveDate> {
        suggestions.iter().map(|s| s.date).collect()
    }

    // -- against the day of a recurring task ------------------------------

    /// The Monday after the spec's Thursday.
    fn the_day() -> NaiveDate {
        date(2026, 8, 10)
    }

    fn due(input: &str, locale: DateLocale) -> Vec<(NaiveDate, String, String)> {
        triples(&suggest_dates_in_series(input, today(), the_day(), locale))
    }

    fn remind(input: &str, locale: DateLocale) -> Vec<(NaiveDate, Option<NaiveTime>, String)> {
        suggest_datetimes_in_series(
            input,
            today().and_hms_opt(12, 0, 0).unwrap(),
            the_day(),
            locale,
        )
        .into_iter()
        .map(|s| (s.date, s.time, s.primary))
        .collect()
    }

    #[test]
    fn a_deadline_is_said_against_the_day_of_the_series() {
        let en = DateLocale::English;
        assert_eq!(
            due("on the day", en),
            vec![(the_day(), "On the day".into(), "Mon Aug 10".into())]
        );
        for input in ["day", "same day", "the day itself"] {
            assert_eq!(due(input, en)[0].0, the_day(), "{input}");
        }
        assert_eq!(
            due("2 days after", en),
            vec![(
                date(2026, 8, 12),
                "2 days after".into(),
                "Wed Aug 12".into()
            )]
        );
        assert_eq!(due("+3", en)[0].0, date(2026, 8, 13));
        assert_eq!(due("day after", en)[0].1, "1 day after");
        assert_eq!(due("1 week later", en)[0].0, date(2026, 8, 17));
        assert_eq!(due("+2w", en)[0].1, "14 days after");

        let de = DateLocale::German;
        assert_eq!(
            due("am Tag", de),
            vec![(the_day(), "Am Tag".into(), "Mo 10. Aug".into())]
        );
        assert_eq!(due("2 Tage später", de)[0].1, "2 Tage später");
        assert_eq!(due("1 tag danach", de)[0].1, "1 Tag später");
        assert_eq!(due("1 Woche später", de)[0].0, date(2026, 8, 17));
    }

    #[test]
    fn a_deadline_said_as_a_date_is_labelled_with_its_distance_from_the_day() {
        // Thursday's "friday" is tomorrow, before the day, and so no deadline.
        assert!(due("friday", DateLocale::English).is_empty());
        assert_eq!(
            due("12.8.", DateLocale::German),
            vec![(
                date(2026, 8, 12),
                "12. August".into(),
                "2 Tage später".into()
            )]
        );
        // Nothing before the day, however it is said.
        assert!(due("day before", DateLocale::English).is_empty());
        assert!(due("-1", DateLocale::English).is_empty());
    }

    #[test]
    fn a_reminder_is_said_against_the_day_of_the_series() {
        let en = DateLocale::English;
        assert_eq!(
            remind("on the day 9am", en),
            vec![(the_day(), Some(time(9, 0)), "On the day 09:00".into())]
        );
        assert_eq!(
            remind("day before 18:00", en),
            vec![(
                date(2026, 8, 9),
                Some(time(18, 0)),
                "1 day before 18:00".into()
            )]
        );
        assert_eq!(remind("-2 14:00", en)[0].0, date(2026, 8, 8));
        assert_eq!(remind("2 days before", en)[0].1, Some(time(9, 0)));
        // A time alone is that time on the day, not today or tomorrow.
        assert_eq!(
            remind("7:30", en),
            vec![(the_day(), Some(time(7, 30)), "On the day 07:30".into())]
        );

        let de = DateLocale::German;
        assert_eq!(
            remind("am Vortag 18 Uhr", de),
            vec![(
                date(2026, 8, 9),
                Some(time(18, 0)),
                "1 Tag vorher 18:00".into()
            )]
        );
        assert_eq!(remind("am Tag 9:00", de)[0].2, "Am Tag 09:00");
        assert_eq!(remind("1 Tag vorher 18:00", de)[0].0, date(2026, 8, 9));
        assert_eq!(remind("2 Tage früher 8 Uhr", de)[0].0, date(2026, 8, 8));
    }

    #[test]
    fn a_reminder_said_as_a_date_keeps_its_reading_and_names_its_distance() {
        let found = suggest_datetimes_in_series(
            "tomorrow 9am",
            today().and_hms_opt(12, 0, 0).unwrap(),
            the_day(),
            DateLocale::English,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].date, date(2026, 8, 7));
        assert_eq!(found[0].primary, "Tomorrow 09:00");
        assert_eq!(found[0].secondary, "3 days before");
    }

    #[test]
    fn what_is_no_distance_from_the_day_is_not_read_as_one() {
        for input in ["day by day", "2 months after", "+x", "after", "3 days"] {
            assert_eq!(
                parse_series_distance(&normalize(input), DateLocale::English),
                None,
                "{input}"
            );
        }
    }

    #[test]
    fn today_is_a_thursday() {
        // Every expectation in the spec table depends on this.
        assert_eq!(today().weekday(), Weekday::Thu);
    }

    // -- the spec's example table (§12.1), row by row ----------------------

    #[test]
    fn spec_table_named_days() {
        assert_eq!(
            triples(&en("tomorr")),
            vec![(date(2026, 8, 7), "Tomorrow".into(), "Aug 7".into())]
        );
        assert_eq!(
            triples(&en("toda")),
            vec![(date(2026, 8, 6), "Today".into(), "Aug 6".into())]
        );
        assert_eq!(
            triples(&en("eom")),
            vec![(date(2026, 8, 31), "End of month".into(), "Aug 31".into())]
        );
        assert_eq!(
            triples(&en("weekend")),
            vec![(date(2026, 8, 8), "Saturday".into(), "Aug 8".into())]
        );
        assert_eq!(
            triples(&en("next week")),
            vec![(date(2026, 8, 10), "Monday".into(), "Aug 10".into())]
        );
        assert_eq!(
            triples(&en("next month")),
            vec![(date(2026, 9, 1), "September 1".into(), "Tue".into())]
        );
    }

    #[test]
    fn spec_table_bare_number_offers_both_readings() {
        assert_eq!(
            triples(&en("3")),
            vec![
                (date(2026, 9, 3), "September 3".into(), "Thu".into()),
                (date(2026, 8, 9), "in 3 days".into(), "Aug 9".into()),
            ]
        );
    }

    #[test]
    fn spec_table_month_names() {
        assert_eq!(
            triples(&en("Sept")),
            vec![(date(2026, 9, 1), "September 1".into(), "Tue".into())]
        );
        // The far-out rows: the spec's prose ("more than a month out shows the
        // relative distance") wins over its own table, which still shows a
        // weekday here.
        assert_eq!(
            triples(&en("Sept 2028")),
            vec![(
                date(2028, 9, 1),
                "September 1, 2028".into(),
                "in 2 years".into()
            )]
        );
        assert_eq!(
            triples(&en("Sept 2028 12")),
            vec![(
                date(2028, 9, 12),
                "September 12, 2028".into(),
                "in 2 years".into()
            )]
        );
    }

    #[test]
    fn spec_table_relative_offsets() {
        assert_eq!(
            triples(&en("in 3 days")),
            vec![(date(2026, 8, 9), "August 9".into(), "Sun".into())]
        );
        assert_eq!(
            triples(&en("in 2 weeks")),
            vec![(date(2026, 8, 20), "August 20".into(), "Thu".into())]
        );
        assert_eq!(
            triples(&en("in 3 months")),
            vec![(date(2026, 11, 6), "November 6".into(), "Fri".into())]
        );
        assert_eq!(
            triples(&en("in 1 year")),
            vec![(date(2027, 8, 6), "August 6, 2027".into(), "Fri".into())]
        );
    }

    #[test]
    fn spec_table_short_offsets_match_the_spelled_out_forms() {
        assert_eq!(triples(&en("3d")), triples(&en("in 3 days")));
        assert_eq!(triples(&en("2w")), triples(&en("in 2 weeks")));
        assert_eq!(triples(&en("3m")), triples(&en("in 3 months")));
        assert_eq!(triples(&en("1y")), triples(&en("in 1 year")));
        // Singular and plural, with and without the leading "in".
        assert_eq!(dates(&en("3 day")), dates(&en("in 3 days")));
        assert_eq!(dates(&en("1 week")), dates(&en("in 1 weeks")));
    }

    #[test]
    fn spec_table_plus_form_keeps_the_distance_as_the_label() {
        assert_eq!(
            triples(&en("+3")),
            vec![(date(2026, 8, 9), "in 3 days".into(), "Aug 9".into())]
        );
    }

    #[test]
    fn spec_table_weekdays() {
        assert_eq!(
            triples(&en("next tuesday")),
            vec![(date(2026, 8, 11), "August 11".into(), "Tue".into())]
        );
        assert_eq!(
            triples(&en("tue")),
            vec![(date(2026, 8, 11), "Tuesday".into(), "Aug 11".into())]
        );
    }

    #[test]
    fn spec_table_explicit_dates() {
        assert_eq!(
            triples(&en("2026-09-12")),
            vec![(
                date(2026, 9, 12),
                "September 12".into(),
                "in 5 weeks".into()
            )]
        );
        // The dotted form is unambiguously day-first, so it has one reading.
        assert_eq!(dates(&en("12.9.")), vec![date(2026, 9, 12)]);
        // `12/9` is the ambiguous one: an English locale prefers December 9 but
        // still offers the day-first reading the spec's table shows, and a
        // German locale offers the same pair the other way round.
        assert_eq!(
            dates(&en("12/9")),
            vec![date(2026, 12, 9), date(2026, 9, 12)]
        );
        assert_eq!(
            dates(&de("12/9")),
            vec![date(2026, 9, 12), date(2026, 12, 9)]
        );
    }

    // -- prefix matching ---------------------------------------------------

    #[test]
    fn keywords_match_on_a_prefix_but_not_on_a_single_letter() {
        assert_eq!(dates(&en("to")), vec![date(2026, 8, 6), date(2026, 8, 7)]);
        assert_eq!(dates(&en("tom")), vec![date(2026, 8, 7)]);
        assert!(en("t").is_empty());
        assert!(en("").is_empty());
        assert!(en("   ").is_empty());
    }

    #[test]
    fn ambiguous_prefixes_offer_every_reading() {
        // "we" is both Wednesday and the weekend.
        assert_eq!(dates(&en("we")), vec![date(2026, 8, 12), date(2026, 8, 8)]);
        // "ju" is June and July, both of which have gone by, so both roll into
        // next year rather than suggesting a date in the past.
        assert_eq!(dates(&en("ju")), vec![date(2027, 6, 1), date(2027, 7, 1)]);
    }

    #[test]
    fn a_half_typed_keyword_still_matches_every_keyword_it_could_become() {
        // "next" is the shared prefix of two keywords, and the user is still
        // typing; both stay on offer until the next keystroke decides.
        assert_eq!(
            dates(&en("next")),
            vec![date(2026, 8, 10), date(2026, 9, 1)]
        );
    }

    #[test]
    fn next_prefixed_weekday_accepts_a_single_letter() {
        assert_eq!(dates(&en("next f")), vec![date(2026, 8, 7)]);
        assert_eq!(dates(&en("next tu")), vec![date(2026, 8, 11)]);
    }

    // -- the weekday rules -------------------------------------------------

    #[test]
    fn bare_weekday_resolves_to_the_next_one() {
        // Thursday today: Friday is tomorrow, Wednesday is six days out.
        assert_eq!(dates(&en("friday")), vec![date(2026, 8, 7)]);
        assert_eq!(dates(&en("wednesday")), vec![date(2026, 8, 12)]);
    }

    #[test]
    fn weekday_typed_on_that_weekday_offers_both_readings_week_out_first() {
        let suggestions = triples(&en("thursday"));
        assert_eq!(
            suggestions,
            vec![
                (date(2026, 8, 13), "Thursday".into(), "Aug 13".into()),
                (date(2026, 8, 6), "August 6".into(), "Thu".into()),
            ]
        );
    }

    #[test]
    fn next_weekday_on_that_weekday_means_a_week_out_only() {
        assert_eq!(dates(&en("next thursday")), vec![date(2026, 8, 13)]);
    }

    #[test]
    fn weekend_typed_on_a_saturday_means_today() {
        let saturday = date(2026, 8, 8);
        assert_eq!(
            dates(&suggest_dates("weekend", saturday, DateLocale::English)),
            vec![saturday]
        );
    }

    // -- day-of-month and month forms --------------------------------------

    #[test]
    fn day_of_month_skips_months_that_are_too_short() {
        let feb = date(2026, 2, 5);
        assert_eq!(
            suggest_dates("31", feb, DateLocale::English)[0].date,
            date(2026, 3, 31)
        );
    }

    #[test]
    fn day_of_month_includes_today() {
        assert_eq!(en("6")[0].date, today());
    }

    #[test]
    fn month_and_day_in_either_order() {
        assert_eq!(dates(&en("sept 12")), vec![date(2026, 9, 12)]);
        assert_eq!(dates(&en("12 sept")), vec![date(2026, 9, 12)]);
        assert_eq!(dates(&de("12. september")), vec![date(2026, 9, 12)]);
    }

    #[test]
    fn a_month_that_has_passed_rolls_into_next_year() {
        assert_eq!(dates(&en("march")), vec![date(2027, 3, 1)]);
        assert_eq!(dates(&en("feb 29")), vec![date(2028, 2, 29)]);
    }

    #[test]
    fn out_of_year_suggestions_only_appear_when_a_year_is_typed() {
        // "Sept" must offer September 2026 and nothing else — no 2027, no 2028.
        let suggestions = en("Sept");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].date.year(), 2026);

        let with_year = en("sept 2028");
        assert_eq!(with_year.len(), 1);
        assert_eq!(with_year[0].date.year(), 2028);
    }

    #[test]
    fn month_forms_reject_junk() {
        assert!(en("sept 12 13").is_empty());
        assert!(en("sept 99").is_empty());
        assert!(en("sept sept").is_empty());
        assert!(en("qq").is_empty());
        assert!(en("asdf 12").is_empty());
    }

    // -- numeric dates -----------------------------------------------------

    #[test]
    fn numeric_date_variants() {
        assert_eq!(dates(&en("12.9.2026")), vec![date(2026, 9, 12)]);
        assert_eq!(dates(&en("12.9.26")), vec![date(2026, 9, 12)]);
        assert_eq!(dates(&de("1.3.")), vec![date(2027, 3, 1)]);
        assert_eq!(
            dates(&en("9/12/2026")),
            vec![date(2026, 9, 12), date(2026, 12, 9)]
        );
        // Only one reading is valid here, so only one is offered.
        assert_eq!(dates(&en("25/9")), vec![date(2026, 9, 25)]);
    }

    #[test]
    fn an_explicit_year_may_be_in_the_past() {
        assert_eq!(dates(&en("2020-01-01")), vec![date(2020, 1, 1)]);
        assert_eq!(en("2020-01-01")[0].secondary, "7 years ago");
        // A past date always spells out its year, so the label stays valid
        // input rather than re-parsing forward into next year.
        assert_eq!(
            en("2026-01-03")[0].primary,
            "January 3, 2026",
            "a past date earlier this year still names the year"
        );
    }

    #[test]
    fn malformed_numeric_dates_yield_nothing() {
        assert!(en("2026-13-01").is_empty());
        assert!(en("32/1").is_empty());
        assert!(en("2026-09").is_empty());
        assert!(en("1-2-3-4").is_empty());
        assert!(en("--").is_empty());
        assert!(en("12.9/3").is_empty());
    }

    // -- German ------------------------------------------------------------

    #[test]
    fn german_keywords_and_labels() {
        assert_eq!(
            triples(&de("heute")),
            vec![(date(2026, 8, 6), "Heute".into(), "6. Aug".into())]
        );
        assert_eq!(
            triples(&de("morgen")),
            vec![(date(2026, 8, 7), "Morgen".into(), "7. Aug".into())]
        );
        assert_eq!(
            triples(&de("monatsende")),
            vec![(date(2026, 8, 31), "Monatsende".into(), "31. Aug".into())]
        );
        assert_eq!(
            triples(&de("naechste woche")),
            vec![(date(2026, 8, 10), "Montag".into(), "10. Aug".into())]
        );
        assert_eq!(
            triples(&de("nächste woche")),
            triples(&de("naechste woche"))
        );
        assert_eq!(
            triples(&de("wochenende")),
            vec![(date(2026, 8, 8), "Samstag".into(), "8. Aug".into())]
        );
    }

    #[test]
    fn german_weekdays_months_and_offsets() {
        assert_eq!(
            triples(&de("di")),
            vec![(date(2026, 8, 11), "Dienstag".into(), "11. Aug".into())]
        );
        assert_eq!(
            triples(&de("in 3 tagen")),
            vec![(date(2026, 8, 9), "9. August".into(), "So".into())]
        );
        assert_eq!(dates(&de("2w")), vec![date(2026, 8, 20)]);
        assert_eq!(dates(&de("1j")), vec![date(2027, 8, 6)]);
        assert_eq!(dates(&de("märz")), vec![date(2027, 3, 1)]);
        assert_eq!(dates(&de("maerz")), vec![date(2027, 3, 1)]);
        assert_eq!(dates(&de("marz")), vec![date(2027, 3, 1)]);
        assert_eq!(
            triples(&de("in 3 monaten")),
            vec![(date(2026, 11, 6), "6. November".into(), "Fr".into())]
        );
        assert_eq!(
            triples(&de("+3")),
            vec![(date(2026, 8, 9), "in 3 Tagen".into(), "9. Aug".into())]
        );
        assert_eq!(
            de("sept 2028")[0].secondary,
            "in 2 Jahren",
            "far dates render their distance in German too"
        );
    }

    #[test]
    fn german_locale_reads_slashes_day_first() {
        // March 9 has already gone by, so that reading rolls into next year.
        assert_eq!(dates(&de("3/9")), vec![date(2026, 9, 3), date(2027, 3, 9)]);
        assert_eq!(dates(&en("3/9")), vec![date(2027, 3, 9), date(2026, 9, 3)]);
    }

    // -- secondary label rule ----------------------------------------------

    #[test]
    fn secondary_switches_from_weekday_to_distance_past_a_month() {
        // 30 days out: still a weekday.
        assert_eq!(en("2026-09-05")[0].secondary, "Sat");
        // 37 days out: the distance, which is what the spec's "in 5 weeks"
        // example is.
        assert_eq!(en("2026-09-12")[0].secondary, "in 5 weeks");
        assert_eq!(en("2027-08-06")[0].secondary, "in 1 year");
        assert_eq!(en("2026-11-06")[0].secondary, "in 3 months");
    }

    #[test]
    fn relative_input_never_echoes_its_own_distance() {
        // Three months out, but the user typed the distance, so the label that
        // adds information is the weekday.
        assert_eq!(en("in 3 months")[0].secondary, "Fri");
        assert_eq!(en("in 1 year")[0].secondary, "Fri");
        assert_eq!(en("in 10 years")[0].secondary, "Wed");
    }

    // -- reminders (times) -------------------------------------------------

    fn now(hour: u32, minute: u32) -> NaiveDateTime {
        today().and_time(time(hour, minute))
    }

    fn en_dt(input: &str, at: NaiveDateTime) -> Vec<DateSuggestion> {
        suggest_datetimes(input, at, DateLocale::English)
    }

    #[test]
    fn reminder_times_from_the_spec() {
        let suggestions = en_dt("tomorrow 9am", now(10, 0));
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].date, date(2026, 8, 7));
        assert_eq!(suggestions[0].time, Some(time(9, 0)));
        assert_eq!(suggestions[0].primary, "Tomorrow 09:00");

        let suggestions = en_dt("next tuesday 14:00", now(10, 0));
        assert_eq!(
            suggestions[0].datetime(),
            date(2026, 8, 11).and_hms_opt(14, 0, 0).unwrap()
        );
        assert_eq!(suggestions[0].primary, "August 11 14:00");

        let suggestions = en_dt("in 2 hours", now(10, 30));
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].datetime(), now(12, 30));
        assert_eq!(suggestions[0].primary, "in 2 hours");
        assert_eq!(suggestions[0].secondary, "Aug 6, 12:30");
    }

    #[test]
    fn reminders_default_to_nine_in_the_morning() {
        let suggestions = en_dt("tomorrow", now(15, 0));
        assert_eq!(suggestions[0].time, Some(DEFAULT_REMINDER_TIME));
        assert_eq!(suggestions[0].primary, "Tomorrow 09:00");
        assert_eq!(
            suggestions[0].datetime(),
            date(2026, 8, 7).and_hms_opt(9, 0, 0).unwrap()
        );
    }

    #[test]
    fn hour_offsets_only_exist_where_there_is_a_time_to_offset_from() {
        assert!(en("in 2 hours").is_empty());
        assert!(en("2h").is_empty());
    }

    #[test]
    fn time_forms() {
        for (input, expected) in [
            ("tomorrow 9am", time(9, 0)),
            ("tomorrow 9 am", time(9, 0)),
            ("tomorrow 9pm", time(21, 0)),
            ("tomorrow 12am", time(0, 0)),
            ("tomorrow 12pm", time(12, 0)),
            ("tomorrow 9:30pm", time(21, 30)),
            ("tomorrow 14:00", time(14, 0)),
            ("tomorrow 7:05", time(7, 5)),
            ("9:30 tomorrow", time(9, 30)),
        ] {
            let suggestions = en_dt(input, now(8, 0));
            assert_eq!(
                suggestions.first().and_then(|s| s.time),
                Some(expected),
                "{input}"
            );
            assert_eq!(suggestions[0].date, date(2026, 8, 7), "{input}");
        }
        assert_eq!(
            suggest_datetimes("morgen 14 uhr", now(8, 0), DateLocale::German)[0].time,
            Some(time(14, 0))
        );
        assert_eq!(
            suggest_datetimes("morgen 14.30 uhr", now(8, 0), DateLocale::German)[0].time,
            Some(time(14, 30))
        );
    }

    #[test]
    fn a_bare_time_rolls_over_to_tomorrow_once_it_has_passed() {
        let suggestions = en_dt("9:00", now(8, 0));
        assert_eq!(suggestions[0].date, today());
        assert_eq!(suggestions[0].primary, "Today 09:00");

        let suggestions = en_dt("9:00", now(14, 0));
        assert_eq!(suggestions[0].date, date(2026, 8, 7));
        assert_eq!(suggestions[0].primary, "Tomorrow 09:00");
    }

    #[test]
    fn a_bare_number_is_never_read_as_a_time() {
        // "12" must stay the twelfth, not noon.
        let suggestions = en_dt("12", now(8, 0));
        assert_eq!(suggestions[0].date, date(2026, 8, 12));
        assert_eq!(suggestions[0].time, Some(DEFAULT_REMINDER_TIME));
        // And a dotted date is not a time either.
        assert_eq!(en_dt("12.9.", now(8, 0))[0].date, date(2026, 9, 12));
    }

    #[test]
    fn unparseable_reminder_input_yields_nothing() {
        assert!(en_dt("qq 9am", now(8, 0)).is_empty());
        assert!(en_dt("", now(8, 0)).is_empty());
        assert!(en_dt("25:00", now(8, 0)).is_empty());
        assert!(en_dt("13pm", now(8, 0)).is_empty());
    }

    // -- DST -----------------------------------------------------------------

    #[test]
    fn arithmetic_crosses_dst_transitions_on_the_civil_calendar() {
        // 2026-03-29 is the European spring-forward and 2026-10-25 the
        // fall-back; both are ordinary civil days as far as this module is
        // concerned, which is the whole point of never touching a timezone.
        let before_spring = date(2026, 3, 28);
        assert_eq!(
            dates(&suggest_dates(
                "tomorrow",
                before_spring,
                DateLocale::English
            )),
            vec![date(2026, 3, 29)]
        );
        assert_eq!(
            suggest_dates("in 1 day", date(2026, 3, 29), DateLocale::English)[0].date,
            date(2026, 3, 30)
        );
        assert_eq!(
            suggest_dates("in 1 week", before_spring, DateLocale::English)[0].date,
            date(2026, 4, 4)
        );
        let before_fall = date(2026, 10, 24);
        assert_eq!(
            suggest_dates("in 1 day", before_fall, DateLocale::English)[0].date,
            date(2026, 10, 25)
        );
        assert_eq!(
            suggest_dates("in 1 week", before_fall, DateLocale::English)[0].date,
            date(2026, 10, 31)
        );
        // A year out from a DST day lands on the same civil date.
        assert_eq!(
            suggest_dates("in 1 year", date(2026, 3, 29), DateLocale::English)[0].date,
            date(2027, 3, 29)
        );
    }

    #[test]
    fn late_evening_does_not_shift_the_civil_day() {
        // The classic `and_utc()` bug: at 23:30 in CET, "tomorrow" computed
        // through UTC is two days out. Here it is not.
        let late = date(2026, 3, 28).and_hms_opt(23, 30, 0).unwrap();
        assert_eq!(en_dt("tomorrow", late)[0].date, date(2026, 3, 29));
        assert_eq!(en_dt("today", late)[0].date, date(2026, 3, 28));
    }

    #[test]
    fn hour_offsets_across_a_spring_forward_stay_on_the_wall_clock() {
        // 02:00 does not exist in CET on 2026-03-29; a reminder is a wall-clock
        // promise, so the civil value is produced as typed and the notification
        // layer is what maps it onto the timezone.
        let before = date(2026, 3, 29).and_hms_opt(1, 30, 0).unwrap();
        let suggestions = en_dt("in 1 hour", before);
        assert_eq!(
            suggestions[0].datetime(),
            date(2026, 3, 29).and_hms_opt(2, 30, 0).unwrap()
        );
        // And crossing midnight moves the date.
        let midnight = date(2026, 10, 24).and_hms_opt(23, 30, 0).unwrap();
        assert_eq!(
            en_dt("in 2 hours", midnight)[0].datetime(),
            date(2026, 10, 25).and_hms_opt(1, 30, 0).unwrap()
        );
    }

    #[test]
    fn month_steps_clamp_to_the_end_of_a_short_month() {
        assert_eq!(
            suggest_dates("in 1 month", date(2026, 1, 31), DateLocale::English)[0].date,
            date(2026, 2, 28)
        );
        assert_eq!(
            suggest_dates("in 1 year", date(2028, 2, 29), DateLocale::English)[0].date,
            date(2029, 2, 28)
        );
    }

    // -- properties ----------------------------------------------------------

    /// Inputs exercising every production, for the round-trip property.
    const CORPUS: &[&str] = &[
        "tomorr",
        "toda",
        "3",
        "17",
        "31",
        "Sept",
        "Sept 2028",
        "Sept 2028 12",
        "in 3 days",
        "in 2 weeks",
        "in 3 months",
        "in 1 year",
        "next tuesday",
        "tue",
        "thursday",
        "next week",
        "next month",
        "eom",
        "weekend",
        "3d",
        "2w",
        "3m",
        "1y",
        "+3",
        "2026-09-12",
        "12.9.",
        "12/9",
        "feb 29",
        "march",
        "we",
        "ju",
    ];

    const CORPUS_DE: &[&str] = &[
        "morgen",
        "heute",
        "3",
        "sept",
        "sept 2028",
        "in 3 tagen",
        "in 2 wochen",
        "naechste woche",
        "naechster monat",
        "monatsende",
        "wochenende",
        "di",
        "naechsten dienstag",
        "12.9.",
        "märz",
        "+3",
    ];

    /// Every suggestion's primary label must parse back to the same date.
    ///
    /// This is what makes the list self-consistent: whatever the picker shows
    /// is itself valid input, so accepting a suggestion and re-typing what it
    /// said can never disagree.
    #[test]
    fn every_primary_label_reparses_to_its_own_date() {
        for (locale, corpus) in [
            (DateLocale::English, CORPUS),
            (DateLocale::German, CORPUS_DE),
        ] {
            // Several reference days, so the labels are exercised in and out of
            // the current year and on a weekday that collides with the input.
            for start in [
                date(2026, 8, 6),
                date(2026, 1, 1),
                date(2026, 12, 28),
                date(2028, 2, 29),
            ] {
                for input in corpus {
                    for suggestion in suggest_dates(input, start, locale) {
                        let again = suggest_dates(&suggestion.primary, start, locale);
                        assert!(
                            again.iter().any(|s| s.date == suggestion.date),
                            "{input:?} suggested {:?} labelled {:?}, which re-parsed to {:?}",
                            suggestion.date,
                            suggestion.primary,
                            dates(&again)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_reminder_label_reparses_to_its_own_instant() {
        let at = today().and_hms_opt(10, 30, 0).unwrap();
        for input in CORPUS
            .iter()
            .chain(["tomorrow 9am", "in 2 hours", "9:30"].iter())
        {
            for suggestion in en_dt(input, at) {
                let again = en_dt(&suggestion.primary, at);
                assert!(
                    again.iter().any(|s| s.datetime() == suggestion.datetime()),
                    "{input:?} suggested {:?} labelled {:?}",
                    suggestion.datetime(),
                    suggestion.primary
                );
            }
        }
    }

    /// Future-only phrases must never resolve into the past.
    ///
    /// Checked on every reference day of a leap year and the year after, which
    /// covers every weekday, every month length, both DST transitions and
    /// 29 February.
    #[test]
    fn future_only_phrases_never_resolve_to_a_past_date() {
        let future_only = [
            "today",
            "tomorrow",
            "in 1 day",
            "in 3 days",
            "in 2 weeks",
            "in 3 months",
            "in 1 year",
            "+3",
            "3d",
            "1y",
            "next week",
            "next month",
            "eom",
            "weekend",
            "monday",
            "tuesday",
            "wednesday",
            "thursday",
            "friday",
            "saturday",
            "sunday",
            "next monday",
            "next sunday",
            "1",
            "15",
            "29",
            "31",
            "jan",
            "feb 29",
            "sept",
            "december 31",
            "12.9.",
            "1.1.",
        ];
        let mut start = date(2028, 1, 1);
        while start < date(2030, 1, 1) {
            for input in future_only {
                for suggestion in suggest_dates(input, start, DateLocale::English) {
                    assert!(
                        suggestion.date >= start,
                        "{input:?} on {start} resolved to the past: {}",
                        suggestion.date
                    );
                }
            }
            start = start.succ_opt().unwrap();
        }
    }

    /// Nothing panics and nothing is invented, whatever gets typed.
    #[test]
    fn junk_input_is_rejected_rather_than_guessed() {
        for input in [
            "q",
            "qq",
            "zzz",
            "!!",
            "-",
            "/",
            ".",
            "..",
            "1/",
            "0",
            "0d",
            "99999999999999999999",
            "in",
            "in x days",
            "3x",
            "sept 12 sept",
            "2026-02-30",
            "31.2.",
            "tomorrow x",
        ] {
            let suggestions = en(input);
            assert!(
                suggestions.is_empty(),
                "{input:?} should not resolve, got {:?}",
                dates(&suggestions)
            );
        }
    }

    #[test]
    fn every_suggestion_carries_both_labels() {
        for input in CORPUS {
            for suggestion in en(input) {
                assert!(!suggestion.primary.is_empty(), "{input}");
                assert!(!suggestion.secondary.is_empty(), "{input}");
                assert_eq!(suggestion.time, None, "{input}");
            }
        }
    }

    #[test]
    fn suggestions_never_repeat_a_date() {
        for input in CORPUS {
            let suggestions = en(input);
            let mut seen = dates(&suggestions);
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), suggestions.len(), "{input}");
        }
    }
}
