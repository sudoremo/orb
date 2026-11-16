//! Turning a parsed query into SQL.
//!
//! A query has two kinds of condition:
//!
//! - text, matched in two ways at once: as the start of a word through the
//!   full-text index, which reaches the body and ranks, and as a piece of the
//!   headers wherever it stands, which finds `thom` in "Thomas" and `mill`
//!   in "jmiller@…" — what someone means when they type part of a name;
//! - state, folder, account and dates, which are plain indexed columns;
//! - a header, looked for among the ones the sync recorded.
//!
//! Every text term becomes a condition of its own, so `-` and `OR` treat it
//! as they treat a folder or a date. The index answers each through a
//! subquery; the ranking comes from one more match over all of them, see
//! [`Compiled::rank_match`].

use super::fold;
use super::query::{Clause, Field, Flag, Query, Term};

/// A query compiled into the pieces a statement is assembled from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compiled {
    /// FTS5 `MATCH` expression to rank by: any of the query's words, so a
    /// message that has more of them, or has them in the subject, comes first.
    /// Never a condition — a header that only contains a word is still a hit,
    /// it just has no score. `None` when the query has no text.
    pub rank_match: Option<String>,
    /// Text to rank by meaning, from a `~` term.
    ///
    /// Separate from everything else because it is not a condition: it decides
    /// the order of the rows the conditions let through, and it is answered by
    /// a different index. A query carrying one is run against the vectors and
    /// filtered by `conditions`; one without is the exact search, unchanged.
    pub semantic: Option<String>,
    /// `WHERE` fragments, to be joined with AND. Parameters are `?`.
    pub conditions: Vec<String>,
    /// Values for the `?` placeholders, in order.
    pub params: Vec<String>,
}

/// Escape a value for use inside an FTS5 string literal.
///
/// FTS5 quotes with `"`, escaped by doubling. Anything else in the value is
/// then inert, which is what stops a query like `s:"a" OR b` from injecting
/// operators into the match expression.
fn fts_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// One text term as an FTS5 expression, its last word matched as the start
/// of a word: `invoic` finds "invoices". A single character stays whole, as
/// every word beginning with it would make the index read most of itself.
fn fts_term(column: Option<&str>, value: &str) -> String {
    let literal = fts_literal(value);
    let prefix = if value.chars().count() >= 2 { "*" } else { "" };
    match column {
        Some(column) => format!("{column} : {literal}{prefix}"),
        None => format!("{literal}{prefix}"),
    }
}

/// The header columns a term is looked for in as a piece of text.
fn header_columns(field: Field) -> &'static [&'static str] {
    match field {
        Field::From => &["m.from_addr"],
        Field::To => &["m.to_addr"],
        Field::Cc => &["m.cc_addr"],
        Field::Subject => &["m.subject"],
        Field::Body => &[],
        Field::Any => &["m.subject", "m.from_addr", "m.to_addr", "m.cc_addr"],
    }
}

/// The FTS column a term is matched in by word, where the index finds what
/// the text of the header does not: the body, which is not in the row, and
/// the subject and a bare word, where stemming finds "invoice" for
/// `invoices`. Addresses are names, and have nothing to stem.
fn indexed_column(field: Field) -> Option<Option<&'static str>> {
    match field {
        Field::Body => Some(Some("body_text")),
        Field::Subject => Some(Some("subject")),
        Field::Any => Some(None),
        Field::From | Field::To | Field::Cc => None,
    }
}

/// Compile a parsed query: its first group, which is the whole of a query
/// without `OR`. See [`compile_groups`] for the rest.
pub fn compile(query: &Query) -> Compiled {
    compile_clauses(&query.clauses)
}

/// Compile every group of a query, one [`Compiled`] per alternative. A query
/// is answered by running each and joining what they found.
pub fn compile_groups(query: &Query) -> Vec<Compiled> {
    query.groups().map(compile_clauses).collect()
}

/// One text term as a condition: found by word in the index, or as text in
/// a header, case and accents aside.
fn text_condition(field: Field, value: &str, params: &mut Vec<String>) -> Option<String> {
    let mut either = Vec::new();
    if let Some(column) = indexed_column(field) {
        either.push(
            "m.id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)".to_string(),
        );
        params.push(fts_term(column, value));
    }
    let needle = fold(value.trim());
    if !needle.is_empty() {
        for column in header_columns(field) {
            either.push(format!("orb_contains({column}, ?)"));
            params.push(needle.clone());
        }
    }
    match either.len() {
        0 => None,
        1 => either.pop(),
        _ => Some(format!("({})", either.join(" OR "))),
    }
}

/// A header as a condition: `List-Id` from its column, any other from the
/// pairs the sync recorded, which it records only for the names it was asked
/// to fetch (see [`crate::Config::extra_header_names`]).
fn header_condition(name: &str, value: Option<&str>, params: &mut Vec<String>) -> String {
    let needle = value.map(fold).filter(|needle| !needle.is_empty());
    if name == "list-id" {
        return match needle {
            Some(needle) => {
                params.push(needle);
                "orb_contains(m.list_id, ?)".to_string()
            }
            None => "m.list_id IS NOT NULL".to_string(),
        };
    }
    params.push(name.to_string());
    let matched = match needle {
        Some(needle) => {
            params.push(needle);
            " AND orb_contains(json_extract(h.value, '$[1]'), ?)"
        }
        None => "",
    };
    format!(
        "(m.extra_headers IS NOT NULL AND EXISTS (SELECT 1 FROM json_each(m.extra_headers) h \
         WHERE json_extract(h.value, '$[0]') = ?{matched}))"
    )
}

fn compile_clauses(clauses: &[Clause]) -> Compiled {
    let mut compiled = Compiled::default();
    let mut ranked: Vec<String> = Vec::new();

    for Clause { negated, term } in clauses {
        match term {
            Term::Text { field, value } | Term::Phrase { field, value } => {
                let mut params = Vec::new();
                let Some(condition) = text_condition(*field, value, &mut params) else {
                    continue;
                };
                compiled.conditions.push(if *negated {
                    format!("NOT {condition}")
                } else {
                    condition
                });
                compiled.params.extend(params);
                if !*negated {
                    ranked.push(fts_term(field.column(), value));
                }
            }
            // Ranking, not filtering, so it contributes no condition. A
            // negated `~` would mean "unlike this", which has no useful
            // meaning over a ranking and is rejected by the parser.
            Term::Semantic(text) => {
                compiled.semantic = Some(match compiled.semantic.take() {
                    // Two `~` terms are one query in two pieces, which is what
                    // `~invoice s:march ~overdue` reads as.
                    Some(existing) => format!("{existing} {text}"),
                    None => text.clone(),
                });
            }

            Term::Flag(flag) => {
                // Read and flagged live in a JSON `flags` blob, so they are
                // matched textually. The wire form is stored with a leading
                // backslash; older rows kept a bare name, so both are accepted.
                let (condition, value) = match flag {
                    Flag::Unread => ("m.flags NOT LIKE ?", "%Seen%"),
                    Flag::Read => ("m.flags LIKE ?", "%Seen%"),
                    Flag::Flagged => ("m.flags LIKE ?", "%Flagged%"),
                    Flag::Answered => ("m.flags LIKE ?", "%Answered%"),
                    Flag::HasAttachment => ("m.has_attachments = 1", ""),
                };
                let condition = if *negated {
                    match flag {
                        Flag::Unread => "m.flags LIKE ?".to_string(),
                        Flag::Read => "m.flags NOT LIKE ?".to_string(),
                        Flag::HasAttachment => {
                            "(m.has_attachments IS NULL OR m.has_attachments = 0)".to_string()
                        }
                        _ => format!("NOT ({condition})"),
                    }
                } else {
                    condition.to_string()
                };
                compiled.conditions.push(condition.clone());
                if condition.contains('?') {
                    compiled.params.push(value.to_string());
                }
            }

            Term::Folder(name) => {
                compiled.conditions.push(if *negated {
                    "f.name <> ?".to_string()
                } else {
                    "f.name = ?".to_string()
                });
                compiled.params.push(name.clone());
            }

            Term::Account(name) => {
                compiled.conditions.push(if *negated {
                    "f.account_name <> ?".to_string()
                } else {
                    "f.account_name = ?".to_string()
                });
                compiled.params.push(name.clone());
            }

            // Dates compare against the sortable integer column, so the
            // comparison uses the same index the list is ordered by. The bound
            // value is the day's midnight UTC.
            Term::After(date) => {
                compiled
                    .conditions
                    .push("m.date_unix >= CAST(strftime('%s', ?) AS INTEGER)".to_string());
                compiled.params.push(date.clone());
            }
            Term::Before(date) => {
                compiled
                    .conditions
                    .push("m.date_unix < CAST(strftime('%s', ?) AS INTEGER)".to_string());
                compiled.params.push(date.clone());
            }

            Term::Header { name, value } => {
                let mut params = Vec::new();
                let condition = header_condition(name, value.as_deref(), &mut params);
                compiled.conditions.push(if *negated {
                    format!("NOT {condition}")
                } else {
                    condition
                });
                compiled.params.extend(params);
            }
        }
    }

    if !ranked.is_empty() {
        compiled.rank_match = Some(ranked.join(" OR "));
    }

    compiled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::parse;

    fn compiled(input: &str) -> Compiled {
        compile(&parse(input).unwrap())
    }

    #[test]
    fn a_sender_term_is_looked_for_inside_the_sender() {
        // `f:thom` is part of a name, not a word of its own.
        let c = compiled("f:Thom");
        assert_eq!(c.conditions, ["orb_contains(m.from_addr, ?)"]);
        assert_eq!(c.params, ["thom"], "folded, so the case does not matter");
        assert_eq!(c.rank_match.as_deref(), Some(r#"from_addr : "Thom"*"#));
    }

    #[test]
    fn a_bare_term_searches_every_header_and_the_body() {
        let c = compiled("invoice");
        assert_eq!(c.conditions.len(), 1);
        let condition = &c.conditions[0];
        assert!(condition.contains("messages_fts MATCH ?"));
        for column in ["m.subject", "m.from_addr", "m.to_addr", "m.cc_addr"] {
            assert!(
                condition.contains(column),
                "{column} missing from {condition}"
            );
        }
        assert_eq!(
            c.params[0], r#""invoice"*"#,
            "the index matches it as a prefix"
        );
        assert_eq!(c.params.len(), 5);
    }

    #[test]
    fn a_body_term_is_answered_by_the_index_alone() {
        let c = compiled("b:refund");
        assert_eq!(
            c.conditions,
            ["m.id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)"]
        );
        assert_eq!(c.params, [r#"body_text : "refund"*"#]);
    }

    #[test]
    fn several_text_terms_must_all_match() {
        let c = compiled("f:thomas s:invoice");
        assert_eq!(c.conditions.len(), 2);
        assert_eq!(c.conditions[0], "orb_contains(m.from_addr, ?)");
        assert!(c.conditions[1].contains("orb_contains(m.subject, ?)"));
        assert!(c.conditions[1].contains("messages_fts MATCH ?"));
        assert_eq!(c.params[1], r#"subject : "invoice"*"#);
        assert_eq!(
            c.rank_match.as_deref(),
            Some(r#"from_addr : "thomas"* OR subject : "invoice"*"#)
        );
    }

    #[test]
    fn a_negated_term_is_a_negated_condition_and_does_not_rank() {
        let c = compiled("invoice -f:noreply");
        assert_eq!(c.conditions[1], "NOT orb_contains(m.from_addr, ?)");
        assert_eq!(c.rank_match.as_deref(), Some(r#""invoice"*"#));
        // An exclusion alone needs nothing to exclude from.
        let c = compiled("-f:noreply");
        assert_eq!(c.conditions, ["NOT orb_contains(m.from_addr, ?)"]);
        assert!(c.rank_match.is_none());
    }

    #[test]
    fn a_single_character_is_not_a_prefix() {
        assert_eq!(compiled("b:x").params, [r#"body_text : "x""#]);
    }

    #[test]
    fn structured_terms_do_not_touch_the_index() {
        let c = compiled("is:unread in:Archive acct:Work");
        assert!(
            c.rank_match.is_none(),
            "a query with no text needs no MATCH at all"
        );
        assert_eq!(c.conditions.len(), 3);
        assert!(c.params.contains(&"Archive".to_string()));
        assert!(c.params.contains(&"Work".to_string()));
    }

    #[test]
    fn read_and_unread_are_opposites() {
        assert_eq!(compiled("is:unread").conditions, ["m.flags NOT LIKE ?"]);
        assert_eq!(compiled("is:read").conditions, ["m.flags LIKE ?"]);
        // Negating one gives the other, rather than a doubly-negated LIKE.
        assert_eq!(compiled("-is:unread").conditions, ["m.flags LIKE ?"]);
        assert_eq!(compiled("-is:read").conditions, ["m.flags NOT LIKE ?"]);
    }

    #[test]
    fn a_missing_attachment_flag_counts_as_no_attachment() {
        // NULL means "not known yet", and `NOT (x = 1)` is NULL for a NULL x,
        // which would silently drop those rows from the result.
        let c = compiled("-has:attachment");
        assert_eq!(
            c.conditions,
            ["(m.has_attachments IS NULL OR m.has_attachments = 0)"]
        );
        assert!(c.params.is_empty());
    }

    #[test]
    fn dates_compare_against_the_sortable_column() {
        let c = compiled("after:2026-01-01 before:2026-06-30");
        assert!(c.conditions[0].contains("m.date_unix >="));
        assert!(c.conditions[1].contains("m.date_unix <"));
        assert_eq!(c.params, ["2026-01-01", "2026-06-30"]);
    }

    #[test]
    fn a_phrase_keeps_its_words_together() {
        let c = compiled("cc:'Some Name'");
        assert_eq!(c.conditions, ["orb_contains(m.cc_addr, ?)"]);
        assert_eq!(c.params, ["some name"]);
        let c = compiled("b:'second quarter'");
        assert_eq!(c.params, [r#"body_text : "second quarter"*"#]);
    }

    #[test]
    fn quotes_in_a_value_cannot_break_out_of_the_match_expression() {
        // Doubling is FTS5's escape; anything else in the value is then inert.
        let c = compile(&parse(r#"b:'say "hi"'"#).unwrap());
        assert_eq!(c.params, [r#"body_text : "say ""hi"""*"#]);
    }

    #[test]
    fn fts_operators_in_user_input_stay_literal() {
        // Otherwise a search for the word OR would change the query's meaning.
        let c = compiled("b:OR");
        assert_eq!(c.params, [r#"body_text : "OR"*"#]);
    }

    #[test]
    fn a_mixed_query_splits_between_the_index_and_the_columns() {
        let c = compiled("f:thomas is:unread in:Archive after:2026-01-01");
        assert_eq!(c.conditions.len(), 4);
        assert_eq!(c.params.len(), 4);
        assert!(c.rank_match.is_some());
    }

    #[test]
    fn a_header_is_looked_for_among_the_recorded_ones() {
        let c = compiled("header:X-Assigned-To-Me=true");
        assert_eq!(c.conditions.len(), 1);
        assert!(c.conditions[0].contains("json_each(m.extra_headers)"));
        assert_eq!(c.params, vec!["x-assigned-to-me", "true"]);
        assert_eq!(c.rank_match, None);

        let c = compiled("-header:X-Spam");
        assert!(c.conditions[0].starts_with("NOT ("));
        assert_eq!(c.params, vec!["x-spam"]);

        let c = compiled("header:List-Id=Rust");
        assert_eq!(c.conditions, vec!["orb_contains(m.list_id, ?)"]);
        assert_eq!(c.params, vec!["rust"]);
    }

    #[test]
    fn an_empty_query_compiles_to_nothing() {
        let c = compiled("");
        assert!(c.rank_match.is_none());
        assert!(c.conditions.is_empty());
        assert!(c.params.is_empty());
    }

    #[test]
    fn every_placeholder_has_exactly_one_value() {
        let c = compiled(
            "invoice -thomas b:refund f:anna is:unread is:flagged in:Archive acct:Work \
             after:2026-01-01 has:attachment",
        );
        let placeholders: usize = c.conditions.iter().map(|c| c.matches('?').count()).sum();
        assert_eq!(
            placeholders,
            c.params.len(),
            "a mismatch here binds the wrong value to the wrong condition"
        );
    }
}
