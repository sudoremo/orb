//! The conditions a rule in `config.yml` puts to a message.
//!
//! Filter rules (`filters`) use them now, and highlight rules are to use the
//! same vocabulary, so the matching lives here rather than with either. A
//! condition is matched against [`MessageFacts`]: the decoded headers the
//! database keeps for every message, plus whichever further headers the rules
//! asked the server for.

use std::collections::{BTreeMap, HashMap};

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// A glob or, behind a `re:` prefix, a regular expression.
///
/// A glob matches the whole value, ignoring case: `*` stands for any run of
/// characters and `?` for one. A regular expression is found anywhere in the
/// value and is case-sensitive unless it says otherwise with `(?i)`.
#[derive(Debug, Clone)]
pub struct Pattern {
    source: String,
    regex: Regex,
}

impl Pattern {
    pub fn parse(text: &str) -> Result<Self, String> {
        let regex = match text.strip_prefix("re:") {
            Some(expression) => Regex::new(expression)
                .map_err(|e| format!("'{text}' is not a regular expression: {e}"))?,
            None => RegexBuilder::new(&glob_to_regex(text))
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("'{text}' is not a glob: {e}"))?,
        };
        Ok(Self {
            source: text.to_string(),
            regex,
        })
    }

    pub fn matches(&self, value: &str) -> bool {
        self.regex.is_match(value)
    }

    /// The pattern as the config wrote it.
    pub fn source(&self) -> &str {
        &self.source
    }
}

fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::with_capacity(glob.len() + 8);
    regex.push_str("^(?s:");
    for c in glob.chars() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push_str(")$");
    regex
}

/// What an address condition looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressPart {
    /// `alice@example.com`.
    Address,
    /// `Alice Example`, selected with a `name:` prefix.
    Name,
}

/// A pattern for one of a message's address fields.
#[derive(Debug, Clone)]
pub struct AddressPattern {
    part: AddressPart,
    pattern: Pattern,
}

impl AddressPattern {
    pub fn parse(text: &str) -> Result<Self, String> {
        let (part, pattern) = match text.strip_prefix("name:") {
            Some(rest) => (AddressPart::Name, rest),
            None => (AddressPart::Address, text),
        };
        Ok(Self {
            part,
            pattern: Pattern::parse(pattern)?,
        })
    }

    /// Whether any entry of a stored address list matches.
    pub fn matches(&self, list: Option<&str>) -> bool {
        address_entries(list.unwrap_or("")).iter().any(|entry| {
            let (name, address) = split_entry(entry);
            match self.part {
                AddressPart::Address => self.pattern.matches(&address),
                AddressPart::Name => self.pattern.matches(&name),
            }
        })
    }
}

/// The entries of an address list as the database stores it:
/// `Name <address>` or a bare address, joined with `, `.
///
/// A display name is stored decoded and unquoted, so it may hold a comma of
/// its own (`Doe, John <john@example.com>`); a piece with neither an `@` nor
/// a `<` is such a name and belongs to the piece after it.
fn address_entries(list: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut pending = String::new();
    let mut in_angle = false;
    let mut current = String::new();
    let mut pieces = Vec::new();
    for c in list.chars() {
        match c {
            '<' => in_angle = true,
            '>' => in_angle = false,
            ',' if !in_angle => {
                pieces.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    pieces.push(current);

    for piece in pieces {
        if !pending.is_empty() {
            pending.push(',');
        }
        pending.push_str(&piece);
        if piece.contains('@') || piece.contains('<') {
            entries.push(std::mem::take(&mut pending).trim().to_string());
        }
    }
    if !pending.trim().is_empty() {
        entries.push(pending.trim().to_string());
    }
    entries
}

/// The display name and the address of one entry.
fn split_entry(entry: &str) -> (String, String) {
    match (entry.rfind('<'), entry.rfind('>')) {
        (Some(open), Some(close)) if open < close => (
            entry[..open].trim().trim_matches('"').trim().to_string(),
            entry[open + 1..close].trim().to_string(),
        ),
        _ => (String::new(), entry.trim().to_string()),
    }
}

/// What a message offers to be matched against.
#[derive(Debug, Clone, Default)]
pub struct MessageFacts {
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub has_attachment: bool,
    /// Further headers, decoded, by lower-cased name. Holds only those some
    /// rule asked for; a header a message does not have is absent.
    pub headers: HashMap<String, Vec<String>>,
}

impl MessageFacts {
    /// Add the headers of a header block, as `HEADER.FIELDS` returns it,
    /// decoding RFC 2047 encoded-words.
    pub fn add_header_block(&mut self, block: &[u8]) {
        let Ok((headers, _)) = mailparse::parse_headers(block) else {
            return;
        };
        for header in headers {
            self.headers
                .entry(header.get_key().to_ascii_lowercase())
                .or_default()
                .push(header.get_value().trim().to_string());
        }
    }

    fn header(&self, name: &str) -> &[String] {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// A rule's `when`: every condition given must hold.
#[derive(Debug, Clone, Default)]
pub struct Conditions {
    pub from: Option<AddressPattern>,
    pub to: Option<AddressPattern>,
    pub cc: Option<AddressPattern>,
    pub subject: Option<Pattern>,
    pub list_id: Option<Pattern>,
    pub headers: Vec<(String, Pattern)>,
    pub has_attachment: Option<bool>,
}

impl Conditions {
    pub fn matches(&self, facts: &MessageFacts) -> bool {
        let address = |pattern: &Option<AddressPattern>, list: &Option<String>| {
            pattern
                .as_ref()
                .is_none_or(|pattern| pattern.matches(list.as_deref()))
        };
        address(&self.from, &facts.from)
            && address(&self.to, &facts.to)
            && address(&self.cc, &facts.cc)
            && self
                .subject
                .as_ref()
                .is_none_or(|pattern| pattern.matches(facts.subject.as_deref().unwrap_or("")))
            && self.list_id.as_ref().is_none_or(|pattern| {
                facts
                    .header("list-id")
                    .iter()
                    .any(|value| pattern.matches(value) || pattern.matches(list_identifier(value)))
            })
            && self.headers.iter().all(|(name, pattern)| {
                facts
                    .header(name)
                    .iter()
                    .any(|value| pattern.matches(value))
            })
            && self
                .has_attachment
                .is_none_or(|wanted| wanted == facts.has_attachment)
    }

    /// The headers these conditions need beyond what the database keeps,
    /// upper-cased as `HEADER.FIELDS` names them.
    pub fn header_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .headers
            .iter()
            .map(|(name, _)| name.to_ascii_uppercase())
            .collect();
        if self.list_id.is_some() {
            names.push("LIST-ID".to_string());
        }
        names
    }

    pub fn is_empty(&self) -> bool {
        self.from.is_none()
            && self.to.is_none()
            && self.cc.is_none()
            && self.subject.is_none()
            && self.list_id.is_none()
            && self.headers.is_empty()
            && self.has_attachment.is_none()
    }
}

/// The identifier inside a `List-Id`: `list.example.com` out of
/// `The List <list.example.com>`.
fn list_identifier(value: &str) -> &str {
    match (value.rfind('<'), value.rfind('>')) {
        (Some(open), Some(close)) if open < close => &value[open + 1..close],
        _ => value,
    }
}

/// A `when` as the config writes it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConditions {
    pub from: Option<Scalar>,
    pub to: Option<Scalar>,
    pub cc: Option<Scalar>,
    pub subject: Option<Scalar>,
    pub list_id: Option<Scalar>,
    #[serde(default)]
    pub header: BTreeMap<String, Scalar>,
    pub has_attachment: Option<bool>,
}

impl RawConditions {
    pub fn compile(&self) -> Result<Conditions, String> {
        let address = |key: &str, value: &Option<Scalar>| {
            value
                .as_ref()
                .map(|v| AddressPattern::parse(&v.0).map_err(|e| format!("`{key}`: {e}")))
                .transpose()
        };
        let pattern = |key: &str, value: &Option<Scalar>| {
            value
                .as_ref()
                .map(|v| Pattern::parse(&v.0).map_err(|e| format!("`{key}`: {e}")))
                .transpose()
        };
        let headers = self
            .header
            .iter()
            .map(|(name, value)| {
                if name.trim().is_empty() || name.contains(|c: char| c.is_whitespace() || c == ':')
                {
                    return Err(format!("`header`: '{name}' is not a header name"));
                }
                Pattern::parse(&value.0)
                    .map(|pattern| (name.clone(), pattern))
                    .map_err(|e| format!("`header.{name}`: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Conditions {
            from: address("from", &self.from)?,
            to: address("to", &self.to)?,
            cc: address("cc", &self.cc)?,
            subject: pattern("subject", &self.subject)?,
            list_id: pattern("list_id", &self.list_id)?,
            headers,
            has_attachment: self.has_attachment,
        })
    }
}

/// A pattern as YAML may hand it over: `X-Spam-Flag: YES` arrives as text, but
/// `X-Priority: 1` arrives as a number and is meant as the same thing.
#[derive(Debug, Clone)]
pub struct Scalar(pub String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Any {
            Text(String),
            Integer(i64),
            Float(f64),
            Bool(bool),
        }
        Ok(Scalar(match Any::deserialize(deserializer)? {
            Any::Text(text) => text,
            Any::Integer(n) => n.to_string(),
            Any::Float(n) => n.to_string(),
            Any::Bool(b) => b.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> MessageFacts {
        let mut facts = MessageFacts {
            from: Some("News Desk <desk@news.example.com>".into()),
            to: Some("Doe, John <john@example.com>, team@example.com".into()),
            cc: None,
            subject: Some("[News] Weekly digest".into()),
            has_attachment: false,
            headers: HashMap::new(),
        };
        facts.add_header_block(
            b"List-Id: =?utf-8?Q?Caf=C3=A9_news?= <cafe.news.example.com>\r\n\
              X-Spam-Flag: YES\r\n\r\n",
        );
        facts
    }

    fn when(yaml: &str) -> Conditions {
        let raw: RawConditions = serde_saphyr::from_str(yaml).expect("conditions parse");
        raw.compile().expect("conditions compile")
    }

    #[test]
    fn a_glob_matches_the_whole_address_ignoring_case() {
        assert!(when("from: '*@NEWS.example.com'").matches(&facts()));
        assert!(!when("from: '*@example.com'").matches(&facts()));
        assert!(when("from: 'desk@news.example.co?'").matches(&facts()));
    }

    #[test]
    fn a_name_prefix_matches_the_display_name() {
        assert!(when("from: 'name:news*'").matches(&facts()));
        assert!(!when("from: 'name:desk*'").matches(&facts()));
        // A name with a comma in it is still one entry.
        assert!(when("to: 'name:Doe, John'").matches(&facts()));
        assert!(when("to: 'team@example.com'").matches(&facts()));
    }

    #[test]
    fn a_regular_expression_is_found_anywhere_and_keeps_its_case() {
        assert!(when(r"subject: 're:^\[News\]'").matches(&facts()));
        assert!(!when(r"subject: 're:^\[news\]'").matches(&facts()));
        assert!(when(r"subject: 're:(?i)^\[news\]'").matches(&facts()));
        assert!(when("subject: 're:digest'").matches(&facts()));
    }

    #[test]
    fn headers_are_decoded_before_they_are_matched() {
        assert!(when("list_id: 'Café news*'").matches(&facts()));
        assert!(when("list_id: 'cafe.news.example.com'").matches(&facts()));
        assert!(when("list_id: '*'").matches(&facts()));
        assert!(when("header:\n  X-Spam-Flag: 'yes'").matches(&facts()));
        assert!(when("header:\n  x-spam-flag: 'YES'").matches(&facts()));
    }

    #[test]
    fn a_missing_header_matches_nothing_not_even_a_star() {
        let bare = MessageFacts::default();
        assert!(!when("list_id: '*'").matches(&bare));
        assert!(!when("header:\n  X-Spam-Flag: '*'").matches(&bare));
        assert!(when("has_attachment: false").matches(&bare));
        assert!(!when("has_attachment: true").matches(&bare));
    }

    #[test]
    fn every_condition_must_hold() {
        assert!(when("from: '*@news.example.com'\nsubject: '*digest'").matches(&facts()));
        assert!(!when("from: '*@news.example.com'\nsubject: '*invoice*'").matches(&facts()));
        assert!(when("").matches(&facts()));
    }

    #[test]
    fn a_number_is_taken_as_the_text_it_was_written_as() {
        let mut facts = MessageFacts::default();
        facts.add_header_block(b"X-Priority: 1\r\n\r\n");
        assert!(when("header:\n  X-Priority: 1").matches(&facts));
    }

    #[test]
    fn a_broken_pattern_says_which_key_it_was() {
        let raw: RawConditions = serde_saphyr::from_str("subject: 're:(unclosed'").unwrap();
        let error = raw.compile().unwrap_err();
        assert!(error.starts_with("`subject`:"), "{error}");
    }

    #[test]
    fn the_headers_to_ask_for_are_the_ones_the_rule_names() {
        let names = when("list_id: '*'\nheader:\n  X-Spam-Flag: 'YES'").header_names();
        assert_eq!(names, vec!["X-SPAM-FLAG", "LIST-ID"]);
    }
}
