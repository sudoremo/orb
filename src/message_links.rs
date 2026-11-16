//! The web addresses a message points to, such as the issue a tracker's
//! notification is about, found by the `message_links` rules in its subject
//! or its body.

use regex::Regex;

use crate::config::{MessageLinkRule, MessageLinkTarget};
use crate::logger;
use crate::rules::AddressPattern;

/// A link a rule found in a message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageLink {
    pub name: Option<String>,
    pub url: String,
}

/// What the rules look at: the subject as it arrived, the `From` as the
/// database stores it, and the body, when there is one to look at.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkSource<'a> {
    pub subject: &'a str,
    pub from: Option<&'a str>,
    pub body: Option<&'a str>,
}

/// The links of every rule that matches `source`, in rule order. A rule
/// whose pattern or `from` does not compile is skipped with a warning.
pub fn links_for(rules: &[MessageLinkRule], source: LinkSource<'_>) -> Vec<MessageLink> {
    rules
        .iter()
        .enumerate()
        .filter_map(|(idx, rule)| {
            if let Some(from) = &rule.from {
                match AddressPattern::parse(from) {
                    Ok(pattern) if pattern.matches(source.from) => {}
                    Ok(_) => return None,
                    Err(e) => {
                        logger::warn(&format!("message_links[{idx}]: from {e}"));
                        return None;
                    }
                }
            }
            let regex = match Regex::new(&rule.pattern) {
                Ok(regex) => regex,
                Err(e) => {
                    logger::warn(&format!("message_links[{idx}]: '{}' - {e}", rule.pattern));
                    return None;
                }
            };
            let text = match rule.target {
                MessageLinkTarget::Subject => source.subject,
                MessageLinkTarget::Body => source.body?,
            };
            let captures = regex.captures(text)?;
            let mut url = String::new();
            captures.expand(&rule.url, &mut url);
            Some(MessageLink {
                name: rule.name.clone(),
                url,
            })
        })
        .collect()
}

/// The web addresses written out in `text`, in order: `http://` and
/// `https://` up to the next space, without the punctuation that ends a
/// sentence or a bracket nothing opened.
pub fn urls_in(text: &str) -> Vec<String> {
    let pattern = Regex::new(r#"(?i)\bhttps?://[^\s<>"']+"#).expect("a valid pattern");
    pattern
        .find_iter(text)
        .map(|found| {
            let mut url = found.as_str();
            loop {
                let opens = url.matches('(').count();
                let closes = url.matches(')').count();
                match url.chars().last() {
                    Some('.' | ',' | ';' | ':' | '!' | '?') => url = &url[..url.len() - 1],
                    Some(')') if closes > opens => url = &url[..url.len() - 1],
                    _ => break,
                }
            }
            url.to_string()
        })
        .filter(|url| url.len() > "https://".len())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, pattern: &str, url: &str) -> MessageLinkRule {
        MessageLinkRule {
            name: Some(name.into()),
            pattern: pattern.into(),
            url: url.into(),
            ..Default::default()
        }
    }

    fn subject(subject: &str) -> LinkSource<'_> {
        LinkSource {
            subject,
            ..Default::default()
        }
    }

    #[test]
    fn urls_are_found_without_the_punctuation_around_them() {
        assert_eq!(
            urls_in("See https://example.com/a?b=1. And (http://example.org/x)!"),
            ["https://example.com/a?b=1", "http://example.org/x"]
        );
        assert_eq!(
            urls_in("https://en.wikipedia.org/wiki/Rust_(language)"),
            ["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert!(urls_in("no address here, nor https:// alone").is_empty());
    }

    #[test]
    fn a_rule_builds_the_address_from_the_subject() {
        let rules = [
            rule(
                "issue",
                r"Issue #(\d+)",
                "https://tracker.example.com/go/$1",
            ),
            rule(
                "build",
                r"^Build (\w+) failed",
                "https://ci.example.com/${1}/log",
            ),
        ];
        assert_eq!(
            links_for(&rules, subject("[Tracker] Issue #4711 Dates")),
            vec![MessageLink {
                name: Some("issue".into()),
                url: "https://tracker.example.com/go/4711".into(),
            }]
        );
        assert_eq!(
            links_for(&rules, subject("Build main failed"))
                .first()
                .map(|l| l.url.as_str()),
            Some("https://ci.example.com/main/log")
        );
        assert!(links_for(&rules, subject("Lunch?")).is_empty());
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_skipped() {
        let rules = [
            rule("broken", "(", "x"),
            rule("ok", "x", "https://example.com"),
        ];
        assert_eq!(links_for(&rules, subject("x")).len(), 1);
    }

    #[test]
    fn a_body_rule_reads_the_body_and_only_the_body() {
        let runs = MessageLinkRule {
            target: MessageLinkTarget::Body,
            ..rule(
                "run",
                r"https://git\.example\.com/\S+/actions/runs/\d+",
                "$0",
            )
        };
        let body = "Run failed.\n\nhttps://git.example.com/tools/ci/actions/runs/38064/jobs/7";
        let found = links_for(
            std::slice::from_ref(&runs),
            LinkSource {
                subject: "Run failed: ci.yml (428d428)",
                body: Some(body),
                ..Default::default()
            },
        );
        assert_eq!(
            found.first().map(|l| l.url.as_str()),
            Some("https://git.example.com/tools/ci/actions/runs/38064")
        );
        assert!(links_for(std::slice::from_ref(&runs), subject(body)).is_empty());
    }

    #[test]
    fn from_restricts_a_rule_to_its_senders() {
        let issue = MessageLinkRule {
            from: Some("*@tracker.example.com".into()),
            ..rule("issue", r"#(\d+)", "https://tracker.example.com/go/$1")
        };
        let rules = [issue];
        let from = |from| LinkSource {
            subject: "Issue #12",
            from: Some(from),
            ..Default::default()
        };
        assert_eq!(
            links_for(&rules, from("Tracker <bot@tracker.example.com>")).len(),
            1
        );
        assert!(links_for(&rules, from("Alice <alice@example.com>")).is_empty());
        assert!(links_for(&rules, subject("Issue #12")).is_empty());
    }
}
