//! Parsing the search query language.
//!
//! ```text
//! f:thomas  t:sam  cc:'some name'  s:invoice  b:refund
//! "exact phrase"        -f:noreply
//! is:unread|read|flagged|answered   has:attachment
//! in:Archive   acct:Work
//! after:2026-01-01   before:2026-06-30
//! header:X-Priority=1   header:List-Unsubscribe
//! ```
//!
//! Bare words search subject, sender and body together. Everything parses to an
//! AST first and is compiled to SQL separately, which keeps the two concerns
//! testable apart and makes a new operator an addition rather than a change.
//!
//! Text predicates and structured ones are deliberately different things: the
//! former become an FTS5 `MATCH`, the latter a `WHERE` over indexed columns.
//! Conflating them would mean either losing the ranking or losing the index.

use std::fmt;

/// A field a text term can be restricted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Subject,
    From,
    To,
    Cc,
    Body,
    /// No field given: subject, sender and body together.
    Any,
}

impl Field {
    /// The FTS column, or `None` for a search across several.
    pub fn column(self) -> Option<&'static str> {
        match self {
            Field::Subject => Some("subject"),
            Field::From => Some("from_addr"),
            Field::To => Some("to_addr"),
            Field::Cc => Some("cc_addr"),
            Field::Body => Some("body_text"),
            Field::Any => None,
        }
    }

    fn from_prefix(prefix: &str) -> Option<Field> {
        match prefix {
            "s" | "subject" => Some(Field::Subject),
            "f" | "from" => Some(Field::From),
            "t" | "to" => Some(Field::To),
            "cc" => Some(Field::Cc),
            "b" | "body" => Some(Field::Body),
            _ => None,
        }
    }
}

/// A message state that can be filtered on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Unread,
    Read,
    Flagged,
    Answered,
    HasAttachment,
}

/// One condition of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Words matched against an indexed field.
    Text {
        field: Field,
        value: String,
    },
    /// A quoted run matched as a phrase rather than as separate words.
    Phrase {
        field: Field,
        value: String,
    },
    Flag(Flag),
    Folder(String),
    Account(String),
    /// On or after this date, as `YYYY-MM-DD`.
    After(String),
    /// Strictly before this date.
    Before(String),
    /// A header the sync recorded, by its name in lower case: present, or
    /// with a value containing this text.
    Header {
        name: String,
        value: Option<String>,
    },
    /// Free text to rank by meaning rather than match by word.
    ///
    /// Introduced by `~`. Unlike every other term this one does not filter:
    /// it orders. A query carrying one is answered by the vector index and
    /// then narrowed by whatever else it carries, so `~refund f:thomas` reads
    /// as "mail from Thomas, most like 'refund' first".
    Semantic(String),
}

/// A term and whether it was negated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub negated: bool,
    pub term: Term,
}

/// A parsed query: every clause of a group must match, and any group may.
///
/// `clauses` is the first group, which is the whole query without `OR`;
/// `alternatives` are the groups an `OR` introduced, each a conjunction of
/// its own. `f:anna OR f:ben is:unread` reads as "from Anna, or unread and
/// from Ben": `OR` binds weaker than the space.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub clauses: Vec<Clause>,
    pub alternatives: Vec<Vec<Clause>>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty() && self.alternatives.iter().all(Vec::is_empty)
    }

    /// Every group, the first included, each a conjunction.
    pub fn groups(&self) -> impl Iterator<Item = &[Clause]> {
        std::iter::once(self.clauses.as_slice()).chain(self.alternatives.iter().map(Vec::as_slice))
    }

    /// Every clause of every group.
    pub fn all_clauses(&self) -> impl Iterator<Item = &Clause> {
        self.groups().flatten()
    }
}

/// Why a query could not be parsed, with where it went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Byte offset the problem starts at, for pointing at it.
    pub at: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at {})", self.message, self.at)
    }
}

/// One token: a bare word, a quoted run, or a `prefix:value` pair.
struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Lexer { input, pos: 0 }
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    /// Read a quoted run, honouring either quote style so a value containing
    /// one can be written with the other.
    fn read_quoted(&mut self, quote: char) -> Result<String, ParseError> {
        let start = self.pos;
        self.pos += quote.len_utf8();
        let mut value = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(ParseError {
                        message: format!("unclosed {quote} quote"),
                        at: start,
                    });
                }
                Some(c) if c == quote => {
                    self.pos += c.len_utf8();
                    return Ok(value);
                }
                Some(c) => {
                    value.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    /// Read until whitespace, stopping before a quote so `cc:'a b'` works.
    fn read_bare(&mut self) -> String {
        let mut value = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '"' || c == '\'' {
                break;
            }
            value.push(c);
            self.pos += c.len_utf8();
        }
        value
    }
}

/// The value of `header:Name="two words"`, whose quoted part the bare word
/// stopped at.
fn lexer_quoted_value(lexer: &mut Lexer, value: String) -> Result<String, ParseError> {
    match lexer.peek() {
        Some(q @ ('"' | '\'')) if value.ends_with('=') => Ok(value + &lexer.read_quoted(q)?),
        _ => Ok(value),
    }
}

/// `Name` or `Name=text` of a `header:` term.
fn parse_header(value: String, at: usize) -> Result<Term, ParseError> {
    let (name, text) = match value.split_once('=') {
        Some((name, text)) => (name, Some(text.trim())),
        None => (value.as_str(), None),
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ParseError {
            message: format!("'{name}' is not a header name; try header:X-Priority=1"),
            at,
        });
    }
    Ok(Term::Header {
        name: name.to_ascii_lowercase(),
        value: text.filter(|text| !text.is_empty()).map(str::to_string),
    })
}

/// Read the text of a `~` query.
///
/// Consumes words until the input ends or a field-qualified term begins, so a
/// semantic query can be written as an ordinary sentence and still be combined
/// with filters. A quoted run is taken whole, which is how to search for text
/// that itself contains a colon.
fn read_semantic_text(lexer: &mut Lexer) -> Result<String, ParseError> {
    let mut words: Vec<String> = Vec::new();

    loop {
        lexer.skip_whitespace();
        let Some(next) = lexer.peek() else { break };

        if let q @ ('"' | '\'') = next {
            words.push(lexer.read_quoted(q)?);
            continue;
        }

        // Look ahead without consuming: a field term ends the semantic text
        // and has to be parsed as a filter by the main loop.
        let mark = lexer.pos;
        let word = lexer.read_bare();
        if word.is_empty() {
            break;
        }
        if starts_a_filter(&word) {
            lexer.pos = mark;
            break;
        }
        words.push(word);
    }

    Ok(words.join(" "))
}

/// Whether a bare word would parse as a filter rather than as prose.
///
/// Only a recognised prefix counts. An ordinary word containing a colon —
/// "note:" mid-sentence, or a bare URL — stays part of the text, which is what
/// someone typing a sentence means by it.
fn starts_a_filter(word: &str) -> bool {
    word.split_once(':').is_some_and(|(prefix, _)| {
        Field::from_prefix(prefix).is_some() || is_structured_prefix(prefix)
    })
}

/// Prefixes that introduce a structured term rather than a field of text.
///
/// Kept beside the match that consumes them; a prefix added there and not here
/// would be swallowed by a preceding `~` instead of filtering.
fn is_structured_prefix(prefix: &str) -> bool {
    matches!(
        prefix.to_ascii_lowercase().as_str(),
        "is" | "has"
            | "in"
            | "folder"
            | "acct"
            | "account"
            | "after"
            | "since"
            | "before"
            | "until"
            | "header"
    )
}

fn parse_flag(value: &str) -> Option<Flag> {
    match value.to_ascii_lowercase().as_str() {
        "unread" | "new" => Some(Flag::Unread),
        "read" | "seen" => Some(Flag::Read),
        "flagged" | "starred" => Some(Flag::Flagged),
        "answered" | "replied" => Some(Flag::Answered),
        _ => None,
    }
}

/// A date, accepted only as `YYYY-MM-DD` so ambiguous orderings cannot silently
/// mean the wrong thing.
fn validate_date(value: &str, at: usize) -> Result<String, ParseError> {
    let ok = value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if ok {
        Ok(value.to_string())
    } else {
        Err(ParseError {
            message: format!("'{value}' is not a date; use YYYY-MM-DD"),
            at,
        })
    }
}

/// Parse a query string.
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let mut lexer = Lexer::new(input);
    let mut clauses = Vec::new();
    let mut alternatives: Vec<Vec<Clause>> = Vec::new();
    // `NOT` negates the term after it, like `-`.
    let mut not_pending = false;

    loop {
        lexer.skip_whitespace();
        let Some(first) = lexer.peek() else { break };
        let token_start = lexer.pos;

        // `OR` and `NOT`, as words on their own and in capitals, so that a
        // search for the word "or" stays possible.
        if first == 'O' || first == 'N' {
            let mark = lexer.pos;
            let word = lexer.read_bare();
            match word.as_str() {
                "OR" => {
                    if clauses.is_empty() && alternatives.is_empty() {
                        return Err(ParseError {
                            message: "'OR' needs something before it".to_string(),
                            at: token_start,
                        });
                    }
                    alternatives.push(std::mem::take(&mut clauses));
                    continue;
                }
                "NOT" => {
                    not_pending = true;
                    continue;
                }
                _ => lexer.pos = mark,
            }
        }

        let negated = first == '-' || not_pending;
        not_pending = false;
        if first == '-' {
            lexer.pos += 1;
            if lexer.peek().is_none_or(char::is_whitespace) {
                return Err(ParseError {
                    message: "'-' must be followed by something to exclude".to_string(),
                    at: token_start,
                });
            }
        }

        // `~` opens a semantic query, which runs to the end of the input or to
        // the next field-qualified term. Everything between is one piece of
        // text: `~invoice was never paid` is a sentence, not four words to
        // match, while `~refund f:thomas` still filters by sender.
        if first == '~' && !negated {
            lexer.pos += 1;
            let text = read_semantic_text(&mut lexer)?;
            if text.is_empty() {
                return Err(ParseError {
                    message: "'~' must be followed by what to search for".to_string(),
                    at: token_start,
                });
            }
            clauses.push(Clause {
                negated: false,
                term: Term::Semantic(text),
            });
            continue;
        }

        // A quoted run with no prefix is a phrase over any field.
        if let Some(q @ ('"' | '\'')) = lexer.peek() {
            let value = lexer.read_quoted(q)?;
            clauses.push(Clause {
                negated,
                term: Term::Phrase {
                    field: Field::Any,
                    value,
                },
            });
            continue;
        }

        let word = lexer.read_bare();
        if word.is_empty() {
            continue;
        }

        // `prefix:value`, where the value may itself be quoted.
        let Some((prefix, rest)) = word.split_once(':') else {
            clauses.push(Clause {
                negated,
                term: Term::Text {
                    field: Field::Any,
                    value: word,
                },
            });
            continue;
        };

        let value_start = lexer.pos;
        let (value, quoted) = if rest.is_empty() {
            // `f: thomas` as well as `f:thomas`. The space is a natural way to
            // type it and every other mail search accepts it; refusing meant a
            // query that looked right returned nothing.
            //
            // The word is only taken from across a space when it is not itself
            // a filter, so `f: is:unread` does not read a state as a sender —
            // it stays an error, which is what it is.
            let mark = lexer.pos;
            lexer.skip_whitespace();
            match lexer.peek() {
                Some(q @ ('"' | '\'')) => (lexer.read_quoted(q)?, true),
                _ => {
                    let word = lexer.read_bare();
                    if word.is_empty() || starts_a_filter(&word) {
                        lexer.pos = mark;
                        (String::new(), false)
                    } else {
                        (word, false)
                    }
                }
            }
        } else {
            (rest.to_string(), false)
        };

        if value.is_empty() {
            return Err(ParseError {
                message: format!("'{prefix}:' needs a value"),
                at: token_start,
            });
        }

        let lower = prefix.to_ascii_lowercase();
        let term = if let Some(field) = Field::from_prefix(&lower) {
            if quoted {
                Term::Phrase { field, value }
            } else {
                Term::Text { field, value }
            }
        } else {
            match lower.as_str() {
                "is" => Term::Flag(parse_flag(&value).ok_or_else(|| ParseError {
                    message: format!(
                        "unknown state '{value}'; try unread, read, flagged or answered"
                    ),
                    at: value_start,
                })?),
                "has" => {
                    if value.eq_ignore_ascii_case("attachment")
                        || value.eq_ignore_ascii_case("attachments")
                    {
                        Term::Flag(Flag::HasAttachment)
                    } else {
                        return Err(ParseError {
                            message: format!("unknown 'has:{value}'; only attachment is supported"),
                            at: value_start,
                        });
                    }
                }
                "in" | "folder" => Term::Folder(value),
                "acct" | "account" => Term::Account(value),
                "after" | "since" => Term::After(validate_date(&value, value_start)?),
                "before" | "until" => Term::Before(validate_date(&value, value_start)?),
                "header" => parse_header(lexer_quoted_value(&mut lexer, value)?, value_start)?,
                _ => {
                    // An unknown prefix is far more likely to be a colon inside
                    // ordinary text than a typo'd operator, so treat it as text
                    // rather than refusing the query.
                    Term::Text {
                        field: Field::Any,
                        value: word,
                    }
                }
            }
        };

        clauses.push(Clause { negated, term });
    }

    if not_pending {
        return Err(ParseError {
            message: "'NOT' must be followed by something to exclude".to_string(),
            at: lexer.pos,
        });
    }
    // `OR` moved the groups before it aside; the last one is what is left.
    if !alternatives.is_empty() {
        if clauses.is_empty() {
            return Err(ParseError {
                message: "'OR' needs something after it".to_string(),
                at: lexer.pos,
            });
        }
        let mut groups = std::mem::take(&mut alternatives);
        groups.push(clauses);
        let first = groups.remove(0);
        return Ok(Query {
            clauses: first,
            alternatives: groups,
        });
    }

    Ok(Query {
        clauses,
        alternatives,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OR` splits a query into alternatives, each a conjunction of what is
    /// between; `NOT` negates like `-`; both only as capitals on their own.
    #[test]
    fn or_and_not_are_operators_only_in_capitals() {
        let q = parse("f:anna OR f:ben is:unread").unwrap();
        assert_eq!(q.clauses.len(), 1);
        assert_eq!(q.alternatives.len(), 1);
        assert_eq!(q.alternatives[0].len(), 2);
        assert_eq!(q.groups().count(), 2);

        let q = parse("NOT f:noreply invoice").unwrap();
        assert!(q.clauses[0].negated);
        assert!(!q.clauses[1].negated);
        assert!(q.alternatives.is_empty());

        let q = parse("this or that").unwrap();
        assert_eq!(q.clauses.len(), 3, "a lower-case 'or' is a word");
        let q = parse("Oregon Not").unwrap();
        assert_eq!(q.clauses.len(), 2, "a word that merely begins so is a word");

        assert!(parse("OR f:anna").is_err());
        assert!(parse("f:anna OR").is_err());
        assert!(parse("f:anna NOT").is_err());
    }

    #[test]
    fn a_header_term_names_a_header_and_perhaps_its_value() {
        let header = |input: &str| parse(input).map(|q| q.clauses[0].term.clone());
        assert_eq!(
            header("header:X-Assigned-To-Me=true").unwrap(),
            Term::Header {
                name: "x-assigned-to-me".to_string(),
                value: Some("true".to_string())
            }
        );
        assert_eq!(
            header("header:List-Unsubscribe").unwrap(),
            Term::Header {
                name: "list-unsubscribe".to_string(),
                value: None
            }
        );
        assert_eq!(
            header("header:X-Mailer=\"Shop Mailer\" invoice").unwrap(),
            Term::Header {
                name: "x-mailer".to_string(),
                value: Some("Shop Mailer".to_string())
            }
        );
        assert_eq!(
            parse("header:X-Mailer=\"Shop Mailer\" invoice")
                .unwrap()
                .clauses
                .len(),
            2
        );
        assert!(parse("header:=true").is_err());
        assert!(parse("header:X_Bad").is_err());
    }

    /// A space after the colon is how people type it, and it used to return
    /// nothing at all.
    #[test]
    fn a_space_after_the_prefix_is_allowed() {
        for query in ["f:thomas", "f: thomas", "f:  thomas"] {
            let q = parse(query).unwrap_or_else(|e| panic!("{query}: {}", e.message));
            assert_eq!(
                q.clauses[0].term,
                Term::Text {
                    field: Field::From,
                    value: "thomas".to_string()
                },
                "{query}"
            );
        }

        // Quoted values too.
        let q = parse("cc: 'some name'").unwrap();
        assert_eq!(
            q.clauses[0].term,
            Term::Phrase {
                field: Field::Cc,
                value: "some name".to_string()
            }
        );
    }

    #[test]
    fn a_space_does_not_let_a_prefix_swallow_the_next_filter() {
        // `f: is:unread` has no sender in it. Reading "is:unread" as the
        // sender would silently search for a person named "is:unread".
        assert!(parse("f: is:unread").is_err());
        assert!(parse("s: in:Archive").is_err());
        // And a prefix with nothing after it at all is still an error.
        assert!(parse("f:").is_err());
        assert!(parse("f:   ").is_err());
    }

    #[test]
    fn a_space_after_one_prefix_leaves_the_rest_of_the_query_alone() {
        let q = parse("f: thomas is:unread").unwrap();
        assert_eq!(q.clauses.len(), 2);
        assert_eq!(
            q.clauses[0].term,
            Term::Text {
                field: Field::From,
                value: "thomas".to_string()
            }
        );
        assert_eq!(q.clauses[1].term, Term::Flag(Flag::Unread));
    }

    /// `~` is prose, not a list of words to match.
    #[test]
    fn a_semantic_term_takes_the_words_that_follow_it() {
        let q = parse("~invoice was never paid").unwrap();
        assert_eq!(q.clauses.len(), 1);
        assert_eq!(
            q.clauses[0].term,
            Term::Semantic("invoice was never paid".to_string())
        );
    }

    #[test]
    fn a_filter_after_a_semantic_term_still_filters() {
        // The whole point of the split: rank by meaning, narrow by sender.
        let q = parse("~refund f:thomas").unwrap();
        assert_eq!(q.clauses.len(), 2);
        assert_eq!(q.clauses[0].term, Term::Semantic("refund".to_string()));
        assert_eq!(
            q.clauses[1].term,
            Term::Text {
                field: Field::From,
                value: "thomas".to_string()
            }
        );
    }

    #[test]
    fn every_structured_prefix_ends_the_semantic_text() {
        // A prefix that failed to stop it would be swallowed into the prose
        // and silently stop filtering.
        for filter in [
            "f:x",
            "t:x",
            "s:x",
            "b:x",
            "cc:x",
            "is:unread",
            "has:attachment",
            "in:Archive",
            "folder:Archive",
            "acct:Work",
            "account:Work",
            "after:2026-01-01",
            "since:2026-01-01",
            "before:2026-06-30",
            "until:2026-06-30",
        ] {
            let q = parse(&format!("~overdue payment {filter}")).unwrap();
            assert_eq!(
                q.clauses[0].term,
                Term::Semantic("overdue payment".to_string()),
                "{filter} should have ended the semantic text"
            );
            assert_eq!(
                q.clauses.len(),
                2,
                "{filter} should have become its own term"
            );
        }
    }

    #[test]
    fn a_colon_in_ordinary_prose_stays_in_the_text() {
        // "note:" is not an operator, and someone typing a sentence means it
        // as words.
        let q = parse("~note: the meeting moved").unwrap();
        assert_eq!(
            q.clauses[0].term,
            Term::Semantic("note: the meeting moved".to_string())
        );
        assert_eq!(q.clauses.len(), 1);
    }

    #[test]
    fn a_quoted_run_inside_a_semantic_query_is_kept_whole() {
        let q = parse(r#"~"re: your order" shipped"#).unwrap();
        assert_eq!(
            q.clauses[0].term,
            Term::Semantic("re: your order shipped".to_string())
        );
    }

    #[test]
    fn a_semantic_term_needs_something_to_search_for() {
        assert!(parse("~").is_err());
        assert!(parse("~   ").is_err());
        // But a filter alone after it is not text, so it is still an error.
        assert!(parse("~ f:thomas").is_err());
    }

    #[test]
    fn a_bare_tilde_mid_word_is_not_an_operator() {
        // Only a leading `~` opens a semantic query; otherwise an address or
        // a path containing one would change the query's meaning.
        let q = parse("s:a~b").unwrap();
        assert_eq!(
            q.clauses[0].term,
            Term::Text {
                field: Field::Subject,
                value: "a~b".to_string()
            }
        );
    }

    fn text(field: Field, value: &str) -> Clause {
        Clause {
            negated: false,
            term: Term::Text {
                field,
                value: value.to_string(),
            },
        }
    }

    #[test]
    fn bare_words_search_any_field() {
        assert_eq!(
            parse("invoice refund").unwrap().clauses,
            vec![text(Field::Any, "invoice"), text(Field::Any, "refund")]
        );
    }

    #[test]
    fn field_prefixes_are_recognised() {
        let q = parse("f:thomas t:sam s:invoice b:refund cc:jane").unwrap();
        assert_eq!(
            q.clauses,
            vec![
                text(Field::From, "thomas"),
                text(Field::To, "sam"),
                text(Field::Subject, "invoice"),
                text(Field::Body, "refund"),
                text(Field::Cc, "jane"),
            ]
        );
    }

    #[test]
    fn long_prefixes_mean_the_same_as_short_ones() {
        assert_eq!(
            parse("from:a subject:b").unwrap().clauses,
            vec![text(Field::From, "a"), text(Field::Subject, "b")]
        );
    }

    #[test]
    fn a_quoted_value_is_a_phrase() {
        let q = parse("cc:'some name'").unwrap();
        assert_eq!(
            q.clauses,
            vec![Clause {
                negated: false,
                term: Term::Phrase {
                    field: Field::Cc,
                    value: "some name".to_string()
                }
            }]
        );
    }

    #[test]
    fn either_quote_style_works() {
        // So a value containing one quote can be written with the other.
        assert_eq!(
            parse(r#"s:"it's here""#).unwrap().clauses,
            vec![Clause {
                negated: false,
                term: Term::Phrase {
                    field: Field::Subject,
                    value: "it's here".to_string()
                }
            }]
        );
    }

    #[test]
    fn a_bare_quoted_run_is_a_phrase_over_any_field() {
        assert_eq!(
            parse("\"quarterly report\"").unwrap().clauses,
            vec![Clause {
                negated: false,
                term: Term::Phrase {
                    field: Field::Any,
                    value: "quarterly report".to_string()
                }
            }]
        );
    }

    #[test]
    fn terms_can_be_negated() {
        let q = parse("-f:noreply invoice").unwrap();
        assert!(q.clauses[0].negated);
        assert!(!q.clauses[1].negated);
    }

    #[test]
    fn states_and_attachments_parse() {
        let q = parse("is:unread is:flagged has:attachment").unwrap();
        assert_eq!(
            q.clauses.iter().map(|c| c.term.clone()).collect::<Vec<_>>(),
            vec![
                Term::Flag(Flag::Unread),
                Term::Flag(Flag::Flagged),
                Term::Flag(Flag::HasAttachment),
            ]
        );
    }

    #[test]
    fn state_synonyms_are_accepted() {
        assert_eq!(
            parse("is:seen").unwrap().clauses[0].term,
            Term::Flag(Flag::Read)
        );
        assert_eq!(
            parse("is:starred").unwrap().clauses[0].term,
            Term::Flag(Flag::Flagged)
        );
    }

    #[test]
    fn scoping_and_dates_parse() {
        let q = parse("in:Archive acct:Work after:2026-01-01 before:2026-06-30").unwrap();
        assert_eq!(
            q.clauses.iter().map(|c| c.term.clone()).collect::<Vec<_>>(),
            vec![
                Term::Folder("Archive".to_string()),
                Term::Account("Work".to_string()),
                Term::After("2026-01-01".to_string()),
                Term::Before("2026-06-30".to_string()),
            ]
        );
    }

    #[test]
    fn a_folder_name_with_spaces_can_be_quoted() {
        assert_eq!(
            parse("in:'Sent Messages'").unwrap().clauses[0].term,
            Term::Folder("Sent Messages".to_string())
        );
    }

    #[test]
    fn an_unknown_prefix_is_treated_as_text() {
        // A colon in ordinary text is far more common than a typo'd operator,
        // and refusing the query would be the wrong trade.
        assert_eq!(
            parse("re:something").unwrap().clauses[0].term,
            Term::Text {
                field: Field::Any,
                value: "re:something".to_string()
            }
        );
    }

    #[test]
    fn an_empty_query_parses_to_nothing() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("   ").unwrap().is_empty());
    }

    #[test]
    fn errors_point_at_the_problem() {
        let err = parse("s:'unclosed").unwrap_err();
        assert!(err.message.contains("unclosed"));
        assert_eq!(err.at, 2, "should point at the opening quote");

        let err = parse("is:sideways").unwrap_err();
        assert!(err.message.contains("unknown state"));

        let err = parse("after:01-02-2026").unwrap_err();
        assert!(err.message.contains("YYYY-MM-DD"));

        let err = parse("f:").unwrap_err();
        assert!(err.message.contains("needs a value"));

        let err = parse("- invoice").unwrap_err();
        assert!(err.message.contains("followed by"));
    }

    #[test]
    fn a_realistic_query_parses_whole() {
        let q = parse("f:thomas t:sam s:some-subject b:some-body cc:'some name'").unwrap();
        assert_eq!(q.clauses.len(), 5);
        assert_eq!(
            q.clauses[4].term,
            Term::Phrase {
                field: Field::Cc,
                value: "some name".to_string()
            }
        );
    }
}
