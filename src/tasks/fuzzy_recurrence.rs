//! The fuzzy input behind `set-recurrence`: "every monday", "monthly on the
//! 1st", "every 2 weeks until June", in English or German, turned into the
//! `RRULE` subset of §8 — and a rule turned back into words for the detail
//! pane. Pure and clock-free, like the date parser it leans on: "today" is an
//! argument, which is what makes "until June" testable.

use chrono::{Datelike, NaiveDate, Weekday};

use super::dates::{DateLocale, month_aliases, normalize, suggest_dates, weekday_aliases};
use super::recurrence::{ByDay, Frequency, RecurrenceEnd, RecurrenceRule};

/// A reading of the input: the rule, and the rule in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceSuggestion {
    pub rule: RecurrenceRule,
    pub label: String,
}

/// The readings of a recurrence typed by a person, most likely first; empty
/// when the words make no rule.
///
/// Accepted, in either language: `daily`, `every day`, `every 3 days`,
/// `weekly`, `every week`, `every 2 weeks`, `every monday`, `every mon, wed`,
/// `every weekday`, `monthly`, `every month`, `monthly on the 15th`, `on the
/// last day`, `every 2nd tuesday`, `first monday of the month`, `yearly`,
/// `every year`; and after any of them `until <date>` or `<n> times`.
pub fn suggest(input: &str, today: NaiveDate, locale: DateLocale) -> Vec<RecurrenceSuggestion> {
    let text = normalize(input).to_lowercase();
    if text.is_empty() {
        return Vec::new();
    }
    // The end comes last, after any of the forms.
    let (text, end) = split_end(&text, today, locale);
    let Some(mut rule) = parse_body(&text, locale) else {
        return Vec::new();
    };
    rule.end = end;
    let label = describe(&rule, locale);
    vec![RecurrenceSuggestion { rule, label }]
}

/// `until <date>` and `<n> times` / `<n> mal`, taken off the end.
fn split_end(text: &str, today: NaiveDate, locale: DateLocale) -> (String, RecurrenceEnd) {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (at, word) in words.iter().enumerate() {
        if matches!(*word, "until" | "till" | "bis") && at + 1 < words.len() {
            let date_text = words[at + 1..].join(" ");
            if let Some(found) = suggest_dates(&date_text, today, locale).first() {
                return (words[..at].join(" "), RecurrenceEnd::Until(found.date));
            }
        }
    }
    if words.len() >= 2 {
        let last = words[words.len() - 1];
        let count = words[words.len() - 2];
        if matches!(last, "times" | "time" | "mal" | "x")
            && let Ok(n) = count.parse::<u32>()
            && n > 0
        {
            let head = words[..words.len() - 2].join(" ");
            // `for 3 times` as well as `3 times`.
            let head = head
                .strip_suffix(" for")
                .or_else(|| head.strip_suffix(" fuer"))
                .unwrap_or(&head)
                .to_string();
            return (head, RecurrenceEnd::Count(n));
        }
    }
    (text.to_string(), RecurrenceEnd::Never)
}

/// The rule without its end.
fn parse_body(text: &str, locale: DateLocale) -> Option<RecurrenceRule> {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    // A leading "every"/"each"/"jeden"/"jede"/"alle" is the usual opener.
    let opener = matches!(
        words[0],
        "every" | "each" | "jeden" | "jede" | "jedes" | "alle" | "all"
    );
    if opener {
        words.remove(0);
    }
    if words.is_empty() {
        return None;
    }

    // `every 2 weeks`, `alle 3 tage`: a number and a period.
    let (interval, rest) = match words[0].parse::<u32>() {
        Ok(n) if n > 0 => (n, &words[1..]),
        _ => (1, &words[..]),
    };
    if rest.is_empty() {
        return None;
    }

    // A whole period.
    if let Some(freq) = period(rest[0]) {
        let mut rule = RecurrenceRule::new(freq);
        rule.interval = interval;
        // `every month on the 15th`, `monthly on the last day`.
        if freq == Frequency::Monthly
            && let Some(day) = month_day(&rest[1..])
        {
            rule.by_month_day = vec![day];
        }
        return Some(rule);
    }

    // `every weekday` / `werktags`.
    if matches!(
        rest[0],
        "weekday" | "weekdays" | "werktag" | "werktags" | "wochentags"
    ) {
        let mut rule = RecurrenceRule::new(Frequency::Weekly);
        rule.interval = interval;
        rule.by_day = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ]
        .into_iter()
        .map(ByDay::every)
        .collect();
        return Some(rule);
    }

    // `first monday [of the month]`, `2nd tuesday`, `last friday`, and in
    // German `ersten montag`, `letzten freitag`. Before the day of the month,
    // since `last friday` and `last day` both begin with `last`.
    if let Some(ordinal) = ordinal(rest[0])
        && rest.len() >= 2
        && let Some(weekday) = weekday(rest[1], locale)
    {
        let mut rule = RecurrenceRule::new(Frequency::Monthly);
        rule.interval = interval;
        rule.by_day = vec![ByDay::nth(ordinal, weekday)];
        return Some(rule);
    }

    // `on the 15th`, `am 15.`: a day of the month on its own.
    if let Some(day) = month_day(rest) {
        let mut rule = RecurrenceRule::new(Frequency::Monthly);
        rule.interval = interval;
        rule.by_month_day = vec![day];
        return Some(rule);
    }

    // `monday`, `mon wed fri`, `montag und mittwoch`: a weekly rule on days.
    let mut days = Vec::new();
    for word in rest {
        if matches!(*word, "and" | "und" | "&" | "+") {
            continue;
        }
        days.push(weekday(word, locale)?);
    }
    if days.is_empty() {
        return None;
    }
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.interval = interval;
    rule.by_day = days.into_iter().map(ByDay::every).collect();
    Some(rule)
}

fn period(word: &str) -> Option<Frequency> {
    Some(match word {
        "day" | "days" | "daily" | "tag" | "tage" | "taeglich" => Frequency::Daily,
        "week" | "weeks" | "weekly" | "woche" | "wochen" | "woechentlich" => Frequency::Weekly,
        "month" | "months" | "monthly" | "monat" | "monate" | "monatlich" => Frequency::Monthly,
        "year" | "years" | "yearly" | "annually" | "jahr" | "jahre" | "jaehrlich" => {
            Frequency::Yearly
        }
        _ => return None,
    })
}

/// `on the 15th`, `on the last day`, `am 1.`, `am letzten tag`, or just
/// `15th`: a day of the month, counted from the end when it says `last`.
fn month_day(words: &[&str]) -> Option<i8> {
    let mut words: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| !matches!(*w, "on" | "the" | "am" | "of" | "des" | "monats" | "month"))
        .collect();
    if words.is_empty() {
        return None;
    }
    if matches!(words[0], "last" | "letzten" | "letzter" | "letzte") {
        return Some(-1);
    }
    let first = words.remove(0);
    if !words.is_empty() && !matches!(words[0], "day" | "tag") {
        return None;
    }
    let digits: String = first
        .trim_end_matches(['.', 't', 'h', 's', 'n', 'd', 'r'])
        .to_string();
    let day: i8 = digits.parse().ok()?;
    ((1..=31).contains(&day)).then_some(day)
}

fn ordinal(word: &str) -> Option<i8> {
    Some(match word.trim_end_matches('.') {
        "first" | "1st" | "1" | "ersten" | "erster" | "erste" => 1,
        "second" | "2nd" | "2" | "zweiten" | "zweiter" | "zweite" => 2,
        "third" | "3rd" | "3" | "dritten" | "dritter" | "dritte" => 3,
        "fourth" | "4th" | "4" | "vierten" | "vierter" | "vierte" => 4,
        "last" | "letzten" | "letzter" | "letzte" => -1,
        _ => return None,
    })
}

/// A weekday by its name or an abbreviation of at least two letters, in
/// either language whatever the locale.
fn weekday(word: &str, locale: DateLocale) -> Option<Weekday> {
    let word = word.trim_end_matches(['s', '.']);
    if word.len() < 2 {
        return None;
    }
    let order = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];
    for names in [weekday_aliases(locale), weekday_aliases(other(locale))] {
        for (at, aliases) in names.iter().enumerate() {
            if aliases.iter().any(|name| name.starts_with(word)) {
                return Some(order[at]);
            }
        }
    }
    None
}

fn other(locale: DateLocale) -> DateLocale {
    match locale {
        DateLocale::English => DateLocale::German,
        DateLocale::German => DateLocale::English,
    }
}

/// A rule in words: "Every 2 weeks on Monday and Wednesday, until 30 June
/// 2026", for the detail pane and the picker.
pub fn describe(rule: &RecurrenceRule, locale: DateLocale) -> String {
    let german = locale == DateLocale::German;
    let day_name = |weekday: Weekday| -> String {
        let aliases = weekday_aliases(locale);
        let name = aliases[weekday.num_days_from_monday() as usize][0];
        let mut chars = name.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    };
    let join = |names: Vec<String>| -> String {
        match names.len() {
            0 => String::new(),
            1 => names[0].clone(),
            n => format!(
                "{} {} {}",
                names[..n - 1].join(", "),
                if german { "und" } else { "and" },
                names[n - 1]
            ),
        }
    };

    let mut text = if german {
        match (rule.freq, rule.interval) {
            (Frequency::Daily, 1) => "Täglich".to_string(),
            (Frequency::Daily, n) => format!("Alle {n} Tage"),
            (Frequency::Weekly, 1) => "Wöchentlich".to_string(),
            (Frequency::Weekly, n) => format!("Alle {n} Wochen"),
            (Frequency::Monthly, 1) => "Monatlich".to_string(),
            (Frequency::Monthly, n) => format!("Alle {n} Monate"),
            (Frequency::Yearly, 1) => "Jährlich".to_string(),
            (Frequency::Yearly, n) => format!("Alle {n} Jahre"),
        }
    } else {
        match (rule.freq, rule.interval) {
            (Frequency::Daily, 1) => "Every day".to_string(),
            (Frequency::Daily, n) => format!("Every {n} days"),
            (Frequency::Weekly, 1) => "Every week".to_string(),
            (Frequency::Weekly, n) => format!("Every {n} weeks"),
            (Frequency::Monthly, 1) => "Every month".to_string(),
            (Frequency::Monthly, n) => format!("Every {n} months"),
            (Frequency::Yearly, 1) => "Every year".to_string(),
            (Frequency::Yearly, n) => format!("Every {n} years"),
        }
    };

    if !rule.by_day.is_empty() {
        let weekdays: Vec<Weekday> = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ]
        .to_vec();
        let plain: Vec<Weekday> = rule
            .by_day
            .iter()
            .filter(|d| d.ordinal.is_none())
            .map(|d| d.weekday)
            .collect();
        if plain.len() == 5 && weekdays.iter().all(|d| plain.contains(d)) {
            text.push_str(if german {
                " an Werktagen"
            } else {
                " on weekdays"
            });
        } else {
            let names: Vec<String> = rule
                .by_day
                .iter()
                .map(|d| match d.ordinal {
                    Some(-1) => format!(
                        "{} {}",
                        if german { "letzten" } else { "the last" },
                        day_name(d.weekday)
                    ),
                    Some(n) => format!("{} {}", ordinal_word(n, german), day_name(d.weekday)),
                    None => day_name(d.weekday),
                })
                .collect();
            text.push_str(if german { " am " } else { " on " });
            text.push_str(&join(names));
        }
    }
    if !rule.by_month_day.is_empty() {
        let days: Vec<String> = rule
            .by_month_day
            .iter()
            .map(|day| match *day {
                -1 => (if german {
                    "letzten Tag"
                } else {
                    "the last day"
                })
                .to_string(),
                n if n < 0 => {
                    if german {
                        format!("{}. letzten Tag", -n)
                    } else {
                        format!("the {}-to-last day", -n)
                    }
                }
                n => {
                    if german {
                        format!("{n}.")
                    } else {
                        format!("the {}", english_ordinal(n as u32))
                    }
                }
            })
            .collect();
        text.push_str(if german { " am " } else { " on " });
        text.push_str(&join(days));
    }
    match rule.end {
        RecurrenceEnd::Never => {}
        RecurrenceEnd::Until(date) => {
            let month = month_aliases(locale)[date.month0() as usize][0];
            let mut month = month.to_string();
            if let Some(first) = month.get(..1) {
                month = first.to_uppercase() + &month[1..];
            }
            if german {
                text.push_str(&format!(", bis {}. {} {}", date.day(), month, date.year()));
            } else {
                text.push_str(&format!(", until {} {} {}", date.day(), month, date.year()));
            }
        }
        RecurrenceEnd::Count(n) => {
            if german {
                text.push_str(&format!(", {n} Mal"));
            } else {
                text.push_str(&format!(", {n} times"));
            }
        }
    }
    text
}

fn ordinal_word(n: i8, german: bool) -> String {
    if german {
        return format!("{n}.");
    }
    match n {
        1 => "the first".to_string(),
        2 => "the second".to_string(),
        3 => "the third".to_string(),
        4 => "the fourth".to_string(),
        n => format!("the {}", english_ordinal(n as u32)),
    }
}

fn english_ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 6).unwrap()
    }

    fn rule(input: &str) -> String {
        suggest(input, today(), DateLocale::English)
            .first()
            .map(|s| s.rule.to_string())
            .unwrap_or_else(|| panic!("{input:?} makes no rule"))
    }

    #[test]
    fn the_spoken_forms_make_the_rules_the_spec_names() {
        assert_eq!(rule("daily"), "FREQ=DAILY");
        assert_eq!(rule("every day"), "FREQ=DAILY");
        assert_eq!(rule("every 3 days"), "FREQ=DAILY;INTERVAL=3");
        assert_eq!(rule("weekly"), "FREQ=WEEKLY");
        assert_eq!(rule("every 2 weeks"), "FREQ=WEEKLY;INTERVAL=2");
        assert_eq!(rule("every monday"), "FREQ=WEEKLY;BYDAY=MO");
        assert_eq!(rule("every mon, wed and fri"), "FREQ=WEEKLY;BYDAY=MO,WE,FR");
        assert_eq!(rule("every weekday"), "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR");
        assert_eq!(rule("monthly"), "FREQ=MONTHLY");
        assert_eq!(rule("monthly on the 1st"), "FREQ=MONTHLY;BYMONTHDAY=1");
        assert_eq!(
            rule("every month on the 15th"),
            "FREQ=MONTHLY;BYMONTHDAY=15"
        );
        assert_eq!(rule("on the last day"), "FREQ=MONTHLY;BYMONTHDAY=-1");
        assert_eq!(rule("first monday of the month"), "FREQ=MONTHLY;BYDAY=1MO");
        assert_eq!(rule("every 2nd tuesday"), "FREQ=MONTHLY;BYDAY=2TU");
        assert_eq!(rule("last friday"), "FREQ=MONTHLY;BYDAY=-1FR");
        assert_eq!(rule("yearly"), "FREQ=YEARLY");
        assert_eq!(rule("every year"), "FREQ=YEARLY");
        assert_eq!(
            rule("every 2 weeks until June"),
            "FREQ=WEEKLY;INTERVAL=2;UNTIL=20270601"
        );
        assert_eq!(
            rule("every week until 2026-09-12"),
            "FREQ=WEEKLY;UNTIL=20260912"
        );
        assert_eq!(rule("every day 5 times"), "FREQ=DAILY;COUNT=5");
        assert_eq!(rule("every day for 3 times"), "FREQ=DAILY;COUNT=3");
    }

    #[test]
    fn german_is_read_too() {
        let german = |input: &str| {
            suggest(input, today(), DateLocale::German)
                .first()
                .map(|s| s.rule.to_string())
                .unwrap_or_else(|| panic!("{input:?} makes no rule"))
        };
        assert_eq!(german("täglich"), "FREQ=DAILY");
        assert_eq!(german("jeden montag"), "FREQ=WEEKLY;BYDAY=MO");
        assert_eq!(german("alle 2 wochen"), "FREQ=WEEKLY;INTERVAL=2");
        assert_eq!(german("monatlich am 1."), "FREQ=MONTHLY;BYMONTHDAY=1");
        assert_eq!(german("jeden ersten montag"), "FREQ=MONTHLY;BYDAY=1MO");
        assert_eq!(german("wöchentlich 4 mal"), "FREQ=WEEKLY;COUNT=4");
    }

    #[test]
    fn what_is_not_a_rule_makes_none() {
        for input in [
            "",
            "banana",
            "every",
            "every 0 days",
            "on the 40th",
            "until June",
        ] {
            assert!(
                suggest(input, today(), DateLocale::English).is_empty(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn a_rule_reads_back_in_words() {
        let say = |text: &str| {
            describe(
                &text.parse::<RecurrenceRule>().unwrap(),
                DateLocale::English,
            )
        };
        assert_eq!(say("FREQ=DAILY"), "Every day");
        assert_eq!(
            say("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE"),
            "Every 2 weeks on Monday and Wednesday"
        );
        assert_eq!(
            say("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
            "Every week on weekdays"
        );
        assert_eq!(say("FREQ=MONTHLY;BYMONTHDAY=15"), "Every month on the 15th");
        assert_eq!(
            say("FREQ=MONTHLY;BYMONTHDAY=-1"),
            "Every month on the last day"
        );
        assert_eq!(
            say("FREQ=MONTHLY;BYDAY=1MO"),
            "Every month on the first Monday"
        );
        assert_eq!(
            say("FREQ=YEARLY;UNTIL=20260630"),
            "Every year, until 30 June 2026"
        );
        assert_eq!(say("FREQ=DAILY;COUNT=5"), "Every day, 5 times");
        assert_eq!(
            describe(
                &"FREQ=WEEKLY;BYDAY=FR".parse::<RecurrenceRule>().unwrap(),
                DateLocale::German
            ),
            "Wöchentlich am Freitag"
        );
    }
}
