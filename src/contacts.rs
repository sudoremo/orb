//! The address book behind recipient completion.
//!
//! Three sources answer the same question — "who might this be?" — and are
//! ranked against each other by one scorer so the order means something:
//!
//! - shortcuts written by hand in the config, which are meant to win;
//! - everyone the mail already knows about, drawn from the messages themselves;
//! - the system address book, which the platform layer supplies (see
//!   `merge`), because reaching macOS Contacts is not this crate's business.
//!
//! What the mail knows is worth more than it sounds: an address you have
//! written to is a stronger suggestion than one that merely appeared in a
//! header, and one you wrote to last week is stronger than one from 2019. That
//! ordering is the whole difference between a completion list that saves typing
//! and one that has to be read.

use crate::config::{ContactAlias, ContactGroup, ContactsConfig};
use crate::constants::MAX_RETRY_ATTEMPTS;
use crate::db::Database;
use crate::{Error, Result};

/// One address, split into the part a person reads and the part mail needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Recipient {
    /// The display name, if the header carried one.
    pub name: Option<String>,
    /// The bare address, lowercased — addresses are compared case-blind.
    pub address: String,
}

impl Recipient {
    /// Whether this is an address at all, rather than a recipient still being
    /// typed.
    ///
    /// A draft is allowed to hold `foo` — that is the point of a draft — but
    /// the address book must not learn from it. Offering `foo` as a completion
    /// would put something unsendable at the top of the list, and the same
    /// half-typed fragment would come back for years.
    pub fn is_address(&self) -> bool {
        match self.address.split_once('@') {
            Some((local, domain)) => !local.is_empty() && domain.contains('.'),
            None => false,
        }
    }

    /// Render as a header would carry it: `Name <address>`, or bare.
    pub fn formatted(&self) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => format!("{} <{}>", name.trim(), self.address),
            _ => self.address.clone(),
        }
    }
}

/// Where a suggestion came from, which is worth showing beside it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// A shortcut from the config standing for one address.
    Alias { shortcut: String },
    /// A shortcut from the config standing for several.
    Group { shortcut: String },
    /// Learned from the mail.
    History,
    /// The system address book.
    System,
}

/// A completion offered while a recipient is being typed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Suggestion {
    /// What to call it in the list — a person's name, or a group's description.
    pub name: Option<String>,
    /// What choosing it inserts. More than one only for a group.
    pub recipients: Vec<Recipient>,
    pub source: Source,
    /// Higher is a better match. Only meaningful within one query.
    pub score: i32,
}

// ============================================================================
// Parsing address lists
// ============================================================================

/// Split a header's address list into its addresses.
///
/// Commas inside a quoted display name (`"Doe, Jane" <t@x>`) or inside the
/// angle brackets do not separate addresses, so those runs are stepped over
/// rather than split — otherwise one badly-punctuated name turns into two
/// recipients, one of which is not an address at all.
pub fn parse_list(value: &str) -> Vec<Recipient> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut angled = false;

    for ch in value.chars() {
        match ch {
            '"' => quoted = !quoted,
            '<' if !quoted => angled = true,
            '>' if !quoted => angled = false,
            ',' if !quoted && !angled => {
                out.extend(parse_one(&current));
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    out.extend(parse_one(&current));
    out
}

/// Parse one address, with or without a name in front of it.
pub fn parse_one(value: &str) -> Option<Recipient> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let (name, address) = match (value.rfind('<'), value.rfind('>')) {
        (Some(open), Some(close)) if close > open => {
            (value[..open].trim(), value[open + 1..close].trim())
        }
        // No angle brackets: the whole thing is the address.
        _ => ("", value),
    };

    let address = address.trim();
    if address.is_empty() {
        return None;
    }

    // A name may be quoted, and the quotes are punctuation rather than part of
    // it. Unescaping is deliberate: `"O\"Brien"` should read as typed.
    let name = name.trim().trim_matches(['"', '\'']).replace("\\\"", "\"");
    let name = name.trim();

    Some(Recipient {
        // A name that only repeats the address adds nothing but width.
        name: (!name.is_empty() && !name.eq_ignore_ascii_case(address)).then(|| name.to_string()),
        address: address.to_lowercase(),
    })
}

// ============================================================================
// Scoring
// ============================================================================

/// No match at all. Anything scoring this is not offered.
const NO_MATCH: i32 = 0;

/// How well `query` describes the person at `address`.
///
/// One scorer for every source, so a name from the system address book and a
/// name learned from the mail compete on the same terms rather than by which
/// list happened to be consulted first.
///
/// The cases are ordered by how much they tell us. Someone typing `sing`
/// almost certainly means Nina Berger, so a match at the start of any word
/// beats one buried in the middle of an address — without that, a search for a
/// surname is drowned by every `no-reply@…sing…` the mail has ever carried.
pub fn score(query: &str, name: Option<&str>, address: &str) -> i32 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return NO_MATCH;
    }
    let address = address.to_lowercase();
    let name = name.unwrap_or("").to_lowercase();

    if address == query {
        return 1000;
    }
    if name == query {
        return 900;
    }
    if starts_a_word(&name, &query) || address.starts_with(&query) {
        return 700;
    }
    // The local part is what people think of as someone's name at work, so
    // `nina.berger@…` should answer to `berger` as readily as to `nina`.
    if let Some((local, domain)) = address.split_once('@') {
        if starts_a_word(local, &query) {
            return 650;
        }
        if domain.starts_with(&query) {
            return 500;
        }
    }
    if initials(&name) == query {
        return 600;
    }
    if name.contains(&query) || address.contains(&query) {
        return 300;
    }
    NO_MATCH
}

/// Does `query` begin `text`, or begin any word within it?
///
/// Words break on whitespace and on the punctuation that separates the parts of
/// an address — a dot, a dash, an underscore — so `berger` finds both
/// "Nina Berger" and `nina.berger`.
fn starts_a_word(text: &str, query: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    text.split(|c: char| c.is_whitespace() || matches!(c, '.' | '-' | '_' | '+' | '\''))
        .any(|word| word.starts_with(query))
}

/// The first letter of each word, for people who type `nb` and mean Nina
/// Berger.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .collect()
}

/// What being written to, and being written to recently, is worth.
///
/// Added on top of the match score, so it separates equally good matches
/// without ever promoting a worse one above a better one: the whole range here
/// is smaller than the gap between two adjacent match tiers.
fn familiarity(sent: i64, received: i64, last_sent: Option<&str>) -> i32 {
    // Writing to someone says far more than receiving from them — anyone can
    // send you mail, and most of what arrives is from nobody you would write
    // back to.
    let mut bonus = (sent.min(20) * 4) as i32 + (received.min(20)) as i32;
    if let Some(last) = last_sent {
        bonus += match days_since(last) {
            Some(days) if days <= 7 => 40,
            Some(days) if days <= 30 => 25,
            Some(days) if days <= 365 => 10,
            _ => 0,
        };
    }
    bonus
}

/// Whole days between `date` and now, if it can be read at all.
fn days_since(date: &str) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(date).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(parsed.with_timezone(&chrono::Utc))
            .num_days(),
    )
}

// ============================================================================
// The service
// ============================================================================

/// Answers "who might this be?" from the config and the mail.
pub struct ContactService {
    db_path: String,
    aliases: Vec<ContactAlias>,
    groups: Vec<ContactGroup>,
    auto_extract: bool,
}

impl ContactService {
    pub fn new(db_path: &str, config: &ContactsConfig) -> Self {
        Self {
            db_path: db_path.to_string(),
            aliases: config.aliases.clone(),
            groups: config.groups.clone(),
            auto_extract: config.auto_extract,
        }
    }

    /// Whether learning from the mail is switched on.
    pub fn auto_extract(&self) -> bool {
        self.auto_extract
    }

    /// Suggestions for a partly-typed recipient, best first.
    ///
    /// An empty query is not nothing: it is someone asking to be shown who they
    /// write to, before they have typed a letter. That answer is the people
    /// written to most and most recently, which is the useful one often enough
    /// to be worth offering.
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<Suggestion> {
        let query = query.trim();
        if query.is_empty() {
            return self.most_written_to(limit);
        }

        let mut out = Vec::new();

        for alias in &self.aliases {
            // A shortcut is typed to be matched, so it counts as a name here in
            // addition to whatever name the alias carries.
            let by_shortcut = score(query, Some(&alias.shortcut), &alias.email);
            let by_name = score(query, alias.name.as_deref(), &alias.email);
            let best = by_shortcut.max(by_name);
            if best > NO_MATCH {
                out.push(Suggestion {
                    name: alias.name.clone(),
                    recipients: parse_list(&alias.email),
                    source: Source::Alias {
                        shortcut: alias.shortcut.clone(),
                    },
                    // Config beats everything learned: it was written down on
                    // purpose, and nothing found by guessing should outrank it.
                    score: best + 500,
                });
            }
        }

        for group in &self.groups {
            let matched = score(query, Some(&group.shortcut), "").max(score(
                query,
                group.description.as_deref(),
                "",
            ));
            if matched > NO_MATCH {
                out.push(Suggestion {
                    name: group.description.clone(),
                    recipients: group.emails.iter().filter_map(|e| parse_one(e)).collect(),
                    source: Source::Group {
                        shortcut: group.shortcut.clone(),
                    },
                    score: matched + 500,
                });
            }
        }

        match self.learned(query, limit) {
            Ok(learned) => out.extend(learned),
            Err(e) => crate::logger::warn(&format!(
                "recipient completion could not read the address book: {e}"
            )),
        }

        merge(out, limit)
    }

    /// Everyone the mail knows about who matches.
    ///
    /// The database narrows by substring — the widest of the matches `score`
    /// recognises — and the scorer then does the ranking. Doing it this way
    /// keeps one definition of what counts as a match instead of splitting it
    /// between SQL and Rust, where the two would drift.
    fn learned(&self, query: &str, limit: usize) -> Result<Vec<Suggestion>> {
        let db = Database::new(&self.db_path)?;
        let candidates = db.match_contacts(query, limit * 20)?;
        Ok(candidates
            .into_iter()
            .filter_map(|c| {
                let matched = score(query, c.display_name.as_deref(), &c.email);
                (matched > NO_MATCH).then(|| Suggestion {
                    name: c.display_name.clone(),
                    recipients: vec![Recipient {
                        name: c.display_name,
                        address: c.email,
                    }],
                    source: Source::History,
                    score: matched
                        + familiarity(c.sent_count, c.received_count, c.last_sent.as_deref()),
                })
            })
            .collect())
    }

    /// The people written to most and most recently.
    ///
    /// What the list shows before anything is typed. Only correspondents the
    /// user has actually written to: everyone who has ever sent them mail is a
    /// list of strangers, and offering it would bury the handful of people this
    /// is meant to save typing.
    fn most_written_to(&self, limit: usize) -> Vec<Suggestion> {
        let Ok(db) = Database::new(&self.db_path) else {
            return Vec::new();
        };
        match db.most_written_to(limit) {
            Ok(found) => found
                .into_iter()
                .map(|c| Suggestion {
                    name: c.display_name.clone(),
                    recipients: vec![Recipient {
                        name: c.display_name,
                        address: c.email,
                    }],
                    source: Source::History,
                    score: familiarity(c.sent_count, c.received_count, c.last_sent.as_deref()),
                })
                .collect(),
            Err(e) => {
                crate::logger::warn(&format!("could not read the address book: {e}"));
                Vec::new()
            }
        }
    }

    /// Note that these addresses were written to, so they rank higher next time.
    ///
    /// Called when a message is actually sent rather than when a draft is saved:
    /// a draft may never go anywhere, and a half-typed address is not somebody
    /// you correspond with.
    pub fn record_sent(&self, recipients: &[String]) -> Result<()> {
        let db = Database::new(&self.db_path).inspect_err(|e| {
            crate::logger::warn(&format!(
                "Failed to open contacts database '{}' for writing: {}",
                self.db_path, e
            ));
        })?;

        let mut first_err: Option<Error> = None;
        for parsed in recipients
            .iter()
            .flat_map(|r| parse_list(r))
            .filter(Recipient::is_address)
        {
            if let Err(e) =
                Self::upsert_with_retry(&db, &parsed.address, parsed.name.as_deref(), true)
            {
                crate::logger::warn(&format!(
                    "Failed to record sent contact '{}': {}",
                    parsed.address, e
                ));
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Upsert a contact, retrying on transient busy/locked errors.
    ///
    /// Each connection already sets `PRAGMA busy_timeout`, so SQLite blocks
    /// internally on contention; this adds a small bounded retry on top so a
    /// writer that holds the lock longer than the timeout under heavy sync does
    /// not cause the contact write to be lost.
    fn upsert_with_retry(
        db: &Database,
        email: &str,
        display_name: Option<&str>,
        is_sent: bool,
    ) -> Result<()> {
        let mut attempt = 1;
        loop {
            match Database::upsert_contact(db, email, display_name, is_sent) {
                Ok(()) => return Ok(()),
                Err(e) if e.is_retryable() && attempt < MAX_RETRY_ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(50 * attempt as u64));
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Put suggestions from every source in one order, keeping each address once.
///
/// The same person is usually in the mail *and* in the system address book, and
/// showing them twice is worse than showing them once: the second entry costs a
/// row of a short list and tells the reader nothing. The higher-scoring entry
/// wins, which is also the one more likely to carry a real name.
pub fn merge(mut suggestions: Vec<Suggestion>, limit: usize) -> Vec<Suggestion> {
    suggestions.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            // A stable tie-break, so the list does not reshuffle between
            // keystrokes that score identically.
            .then_with(|| key(a).cmp(&key(b)))
    });

    let mut seen = std::collections::HashSet::new();
    suggestions.retain(|s| seen.insert(key(s)));
    suggestions.truncate(limit);
    suggestions
}

/// What makes two suggestions the same suggestion.
fn key(s: &Suggestion) -> String {
    s.recipients
        .iter()
        .map(|r| r.address.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipient(name: Option<&str>, address: &str) -> Recipient {
        Recipient {
            name: name.map(String::from),
            address: address.to_string(),
        }
    }

    #[test]
    fn an_address_list_is_split_into_addresses() {
        assert_eq!(
            parse_list("Nina Berger <nina.berger@example.com>, felix@example.com"),
            vec![
                recipient(Some("Nina Berger"), "nina.berger@example.com"),
                recipient(None, "felix@example.com"),
            ]
        );
    }

    /// The comma in a quoted name is punctuation, not a separator — splitting
    /// on it invents a recipient who is not an address.
    #[test]
    fn a_comma_inside_a_name_does_not_separate() {
        assert_eq!(
            parse_list("\"Doe, Jane\" <t@x.com>, b@y.com"),
            vec![
                recipient(Some("Doe, Jane"), "t@x.com"),
                recipient(None, "b@y.com"),
            ]
        );
    }

    #[test]
    fn addresses_are_compared_case_blind() {
        assert_eq!(
            parse_one("Foo@Example.COM").unwrap().address,
            "foo@example.com"
        );
    }

    #[test]
    fn a_name_that_only_repeats_the_address_is_dropped() {
        assert_eq!(
            parse_one("a@b.com <a@b.com>").unwrap(),
            recipient(None, "a@b.com")
        );
    }

    #[test]
    fn nothing_is_parsed_out_of_nothing() {
        assert!(parse_list("").is_empty());
        assert!(parse_list("  ,  , ").is_empty());
        assert!(parse_one("Name <>").is_none());
    }

    /// A draft holds recipients that are not addresses yet; the address book
    /// must not choke on them.
    #[test]
    fn a_half_typed_recipient_parses_as_itself() {
        assert_eq!(parse_one("foo").unwrap(), recipient(None, "foo"));
    }

    /// …but must not learn from them either, or the fragment is offered as a
    /// completion for years afterwards.
    #[test]
    fn only_real_addresses_are_worth_remembering() {
        assert!(recipient(None, "ada@example.com").is_address());
        assert!(!recipient(None, "foo").is_address());
        assert!(!recipient(None, "@example.com").is_address());
        // No dot in the domain: a local alias, not something to suggest later.
        assert!(!recipient(None, "root@localhost").is_address());
    }

    #[test]
    fn a_recipient_renders_back_the_way_a_header_carries_it() {
        assert_eq!(
            recipient(Some("Ada"), "ada@x.com").formatted(),
            "Ada <ada@x.com>"
        );
        assert_eq!(recipient(None, "ada@x.com").formatted(), "ada@x.com");
    }

    // ---- scoring ----

    #[test]
    fn an_exact_address_outranks_everything() {
        assert!(
            score("ada@x.com", None, "ada@x.com") > score("ada", Some("Ada Lovelace"), "ada@x.com")
        );
    }

    /// The case that makes surname completion work at all.
    #[test]
    fn a_surname_finds_the_person() {
        let matched = score("berg", Some("Nina Berger"), "nina.berger@example.com");
        assert!(matched > NO_MATCH);
        // And beats an address that merely contains the letters somewhere.
        assert!(matched > score("berg", None, "no-reply@iceberg.example.com"));
    }

    /// Without this, `berger` finds nothing for an address whose name was never
    /// recorded — which is most of what an inbox contains.
    #[test]
    fn a_local_part_is_searchable_by_its_parts() {
        assert!(score("berger", None, "nina.berger@example.com") > NO_MATCH);
        assert!(score("weber", None, "paul-weber@x.com") > NO_MATCH);
    }

    #[test]
    fn initials_find_a_person() {
        assert!(score("nb", Some("Nina Berger"), "x@y.com") > NO_MATCH);
    }

    #[test]
    fn a_query_matching_nothing_scores_nothing() {
        assert_eq!(score("zzz", Some("Nina Berger"), "t@x.com"), NO_MATCH);
        assert_eq!(score("", Some("Nina Berger"), "t@x.com"), NO_MATCH);
        assert_eq!(score("   ", None, "t@x.com"), NO_MATCH);
    }

    /// Familiarity separates equally good matches without ever reordering
    /// unequal ones — otherwise a stranger you mailed once would outrank the
    /// person whose name you actually typed.
    #[test]
    fn familiarity_never_outweighs_a_better_match() {
        let best_bonus = familiarity(10_000, 10_000, Some(&chrono::Utc::now().to_rfc3339()));
        let prefix = score("berg", Some("Berger"), "a@b.com");
        let substring = score("berg", None, "iceberg@b.com");
        assert!(
            substring + best_bonus < prefix,
            "{substring} + {best_bonus} < {prefix}"
        );
    }

    #[test]
    fn writing_to_someone_counts_for_more_than_hearing_from_them() {
        assert!(familiarity(5, 0, None) > familiarity(0, 5, None));
    }

    // ---- merging ----

    fn suggestion(address: &str, score: i32, source: Source) -> Suggestion {
        Suggestion {
            name: None,
            recipients: vec![recipient(None, address)],
            source,
            score,
        }
    }

    #[test]
    fn the_same_address_is_offered_once() {
        let merged = merge(
            vec![
                suggestion("a@x.com", 10, Source::History),
                suggestion("a@x.com", 900, Source::System),
                suggestion("b@x.com", 500, Source::History),
            ],
            10,
        );
        assert_eq!(merged.len(), 2);
        // The better-scoring one survives, and leads.
        assert_eq!(merged[0].score, 900);
        assert_eq!(merged[0].source, Source::System);
    }

    #[test]
    fn a_tie_does_not_reshuffle_between_keystrokes() {
        let one = merge(
            vec![
                suggestion("b@x.com", 500, Source::History),
                suggestion("a@x.com", 500, Source::System),
            ],
            10,
        );
        let other = merge(
            vec![
                suggestion("a@x.com", 500, Source::System),
                suggestion("b@x.com", 500, Source::History),
            ],
            10,
        );
        assert_eq!(key(&one[0]), key(&other[0]));
    }

    #[test]
    fn merging_respects_the_limit() {
        let many: Vec<_> = (0..50)
            .map(|i| suggestion(&format!("a{i}@x.com"), i, Source::History))
            .collect();
        assert_eq!(merge(many, 8).len(), 8);
    }
}
