//! Subject munging for display-time transformations
//!
//! This module provides regex-based subject line transformations for display-only purposes.
//! Rules are applied sequentially, with each rule seeing the result of the previous one.
//! Original subjects are never modified in the database.

use regex::Regex;
use std::borrow::Cow;

use crate::config::SubjectMungeRule;
use crate::logger;

/// Compiled subject munging rules for display-time transformation
#[derive(Debug, Clone, Default)]
pub struct SubjectMunger {
    rules: Vec<CompiledRule>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    regex: Regex,
    replacement: String,
    icon: Option<SubjectIcon>,
}

/// An icon a rule draws before the subject of the messages it matches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubjectIcon {
    /// The Font Awesome class, `fa-gears`.
    pub icon: String,
    /// A CSS colour, or `None` for the subject's own.
    pub color: Option<String>,
}

/// The Font Awesome class a configured icon name stands for: `fa-gears`,
/// `gears` and Nerd Fonts' `nf-fa-gears` all name the same icon. `None` for
/// what is not an icon name at all.
pub fn icon_class(name: &str) -> Option<String> {
    let name = name.trim();
    // Nerd Fonts spell Font Awesome's hyphens as underscores.
    let bare = match name.strip_prefix("nf-fa-") {
        Some(nerd) => nerd.replace('_', "-"),
        None => name.strip_prefix("fa-").unwrap_or(name).to_string(),
    };
    (!bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| format!("fa-{bare}"))
}

impl SubjectMunger {
    /// Create from config, compiling all regex patterns
    /// Invalid patterns are logged and skipped (non-fatal)
    pub fn from_config(config: &[SubjectMungeRule]) -> Self {
        let mut rules = Vec::new();

        for (idx, rule) in config.iter().enumerate() {
            match Regex::new(&rule.pattern) {
                Ok(regex) => {
                    let icon = rule
                        .icon
                        .as_deref()
                        .and_then(|name| match icon_class(name) {
                            Some(icon) => Some(icon),
                            None => {
                                logger::warn(&format!(
                                    "subject_munge[{idx}]: '{name}' is not an icon name; \
                                 the rule rewrites without one"
                                ));
                                None
                            }
                        });
                    let color = rule.icon_color.as_deref().and_then(|value| {
                        match crate::highlights::Color::parse(value) {
                            Ok(color) => Some(color.css()),
                            Err(e) => {
                                logger::warn(&format!("subject_munge[{idx}]: icon_color: {e}"));
                                None
                            }
                        }
                    });
                    rules.push(CompiledRule {
                        regex,
                        replacement: rule.replacement.clone(),
                        icon: icon.map(|icon| SubjectIcon { icon, color }),
                    });
                }
                Err(e) => {
                    logger::warn(&format!(
                        "Invalid regex in subject_munge[{}]: '{}' - {}",
                        idx, rule.pattern, e
                    ));
                }
            }
        }

        Self { rules }
    }

    /// The subject as the list shows it, with the icons of every rule that
    /// matched it, in rule order. A rule matches against the subject as the
    /// rules before it left it.
    pub fn display(&self, subject: Option<&str>) -> (String, Vec<SubjectIcon>) {
        let mut current = subject.unwrap_or("").to_string();
        let mut icons = Vec::new();
        for rule in &self.rules {
            if !rule.regex.is_match(&current) {
                continue;
            }
            if let Some(icon) = &rule.icon
                && !icons.contains(icon)
            {
                icons.push(icon.clone());
            }
            current = rule
                .regex
                .replace_all(&current, &rule.replacement)
                .into_owned();
        }
        (current, icons)
    }

    /// Give a list row its rewritten subject and the icons that go with it.
    pub fn apply(&self, header: &mut crate::ffi::MessageHeader) {
        if self.rules.is_empty() {
            return;
        }
        let (subject, icons) = self.display(header.subject.as_deref());
        if header.subject.is_some() {
            header.subject = Some(subject);
        }
        header.subject_icons = icons;
    }

    /// Whether any rule is configured, so a caller can skip the work.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Apply all munging rules sequentially to a subject
    /// Returns Cow::Borrowed if no transformation, Cow::Owned if transformed
    pub fn munge<'a>(&self, subject: Option<&'a str>) -> Cow<'a, str> {
        let Some(subject) = subject else {
            return Cow::Borrowed("");
        };

        // Fast path: no rules configured
        if self.rules.is_empty() {
            return Cow::Borrowed(subject);
        }

        let mut current = Cow::Borrowed(subject);

        // Apply each rule sequentially (each sees result of previous)
        for rule in &self.rules {
            current = match current {
                Cow::Borrowed(s) => rule.regex.replace_all(s, &rule.replacement),
                Cow::Owned(s) => {
                    let result = rule.regex.replace_all(&s, &rule.replacement);
                    match result {
                        Cow::Borrowed(_) => Cow::Owned(s),
                        Cow::Owned(new_s) => Cow::Owned(new_s),
                    }
                }
            };
        }

        current
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_rule_can_shorten_and_add_an_icon() {
        let munger = SubjectMunger::from_config(&[
            SubjectMungeRule {
                pattern: r"^\(Acme \| Tracker \| Engineering\) ".into(),
                replacement: String::new(),
                icon: Some("nf-fa-gears".into()),
                icon_color: Some("#4c8dff".into()),
            },
            SubjectMungeRule {
                pattern: r"Issue #\d+".into(),
                icon: Some("bug".into()),
                ..Default::default()
            },
        ]);
        let (subject, icons) =
            munger.display(Some("(Acme | Tracker | Engineering) [U] Issue #1234 Dates"));
        assert_eq!(subject, "[U] Issue #1234 Dates");
        assert_eq!(
            icons,
            vec![
                SubjectIcon {
                    icon: "fa-gears".into(),
                    color: Some("#4c8dff".into())
                },
                SubjectIcon {
                    icon: "fa-bug".into(),
                    color: None
                },
            ]
        );
        assert_eq!(
            munger.display(Some("Lunch?")),
            ("Lunch?".to_string(), vec![])
        );
    }

    #[test]
    fn icon_names_are_read_as_font_awesome() {
        assert_eq!(icon_class("fa-gears").as_deref(), Some("fa-gears"));
        assert_eq!(icon_class("gears").as_deref(), Some("fa-gears"));
        assert_eq!(icon_class("nf-fa-gears").as_deref(), Some("fa-gears"));
        assert_eq!(
            icon_class("nf-fa-circle_xmark").as_deref(),
            Some("fa-circle-xmark")
        );
        assert_eq!(icon_class("💡"), None);
        assert_eq!(icon_class(""), None);
    }
    use super::*;

    #[test]
    fn test_empty_rules() {
        let munger = SubjectMunger::from_config(&[]);
        assert_eq!(munger.munge(Some("Test")), "Test");
        assert_eq!(munger.munge(None), "");
    }

    #[test]
    fn test_single_rule() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[JIRA\] \(([A-Z]+-[0-9]+)\) ".to_string(),
            replacement: "[$1] ".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("[JIRA] (PROJ-123) Fix bug")),
            "[PROJ-123] Fix bug"
        );
    }

    #[test]
    fn test_sequential_rules() {
        // Rules are applied in order: first remove [SPAM], then collapse Re: Re:
        let rules = vec![
            SubjectMungeRule {
                pattern: r"^\[SPAM\] ".to_string(),
                replacement: "".to_string(),
                ..Default::default()
            },
            SubjectMungeRule {
                pattern: r"^(Re: )+".to_string(),
                replacement: "Re: ".to_string(),
                ..Default::default()
            },
        ];

        let munger = SubjectMunger::from_config(&rules);
        // [SPAM] Re: Re: Test -> Re: Re: Test -> Re: Test
        assert_eq!(munger.munge(Some("[SPAM] Re: Re: Test")), "Re: Test");
    }

    #[test]
    fn test_invalid_regex_skipped() {
        let rules = vec![
            SubjectMungeRule {
                pattern: r"[invalid".to_string(), // Invalid regex
                replacement: "replaced".to_string(),
                ..Default::default()
            },
            SubjectMungeRule {
                pattern: r"test".to_string(), // Valid rule
                replacement: "TEST".to_string(),
                ..Default::default()
            },
        ];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(munger.munge(Some("test")), "TEST");
    }

    #[test]
    fn test_capture_groups() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^(\w+): (\w+): (.*)".to_string(),
            replacement: "[$1/$2] $3".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("project: component: Fix issue")),
            "[project/component] Fix issue"
        );
    }

    #[test]
    fn test_cow_optimization_no_match() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[TAG\]".to_string(),
            replacement: "".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        let subject = "No tag here";
        let result = munger.munge(Some(subject));

        // Verify it's borrowed, not owned (zero allocation)
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn test_cow_owned_on_match() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[TAG\] ".to_string(),
            replacement: "".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        let result = munger.munge(Some("[TAG] Subject"));

        // Verify it's owned when transformation occurs
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(result, "Subject");
    }

    #[test]
    fn test_unicode_subjects() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[标签\] ".to_string(),
            replacement: "".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(munger.munge(Some("[标签] 测试主题")), "测试主题");
    }

    #[test]
    fn test_emoji_subjects() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^🔥 ".to_string(),
            replacement: "[HOT] ".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("🔥 Important message")),
            "[HOT] Important message"
        );
    }

    #[test]
    fn test_complex_nested_tags() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[([^\]]+)\]\s*\[([^\]]+)\]\s*".to_string(),
            replacement: "[$1/$2] ".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("[Team] [Project] Status update")),
            "[Team/Project] Status update"
        );
    }

    #[test]
    fn test_multiple_capture_groups() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^Bug #(\d+): (.+) \(Priority: (\d+)\)$".to_string(),
            replacement: "[P$3] Bug $1: $2".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("Bug #12345: Memory leak (Priority: 1)")),
            "[P1] Bug 12345: Memory leak"
        );
    }

    #[test]
    fn test_whitespace_preservation() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[TAG\]\s+".to_string(),
            replacement: "".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        // Multiple spaces should be consumed by \s+
        assert_eq!(
            munger.munge(Some("[TAG]  Subject with spaces")),
            "Subject with spaces"
        );
    }

    #[test]
    fn test_case_sensitive_matching() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^Re: ".to_string(),
            replacement: "RE: ".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(munger.munge(Some("Re: Test")), "RE: Test");
        // Should NOT match "re:" (lowercase)
        assert_eq!(munger.munge(Some("re: Test")), "re: Test");
    }

    #[test]
    fn test_empty_replacement() {
        let rules = vec![SubjectMungeRule {
            pattern: r"CONFIDENTIAL: ".to_string(),
            replacement: "".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("CONFIDENTIAL: Internal memo")),
            "Internal memo"
        );
    }

    #[test]
    fn test_no_match_returns_original() {
        let rules = vec![SubjectMungeRule {
            pattern: r"^\[NOMATCH\]".to_string(),
            replacement: "".to_string(),
            ..Default::default()
        }];

        let munger = SubjectMunger::from_config(&rules);
        let original = "This doesn't match";
        assert_eq!(munger.munge(Some(original)), original);
    }

    #[test]
    fn test_multiple_rules_different_patterns() {
        let rules = vec![
            SubjectMungeRule {
                pattern: r"^\[EXTERNAL\]\s*".to_string(),
                replacement: "EXT ".to_string(),
                ..Default::default()
            },
            SubjectMungeRule {
                pattern: r"^\[URGENT\]\s*".to_string(),
                replacement: "URG ".to_string(),
                ..Default::default()
            },
            SubjectMungeRule {
                pattern: r"^Fwd:\s*".to_string(),
                replacement: "FWD ".to_string(),
                ..Default::default()
            },
        ];

        let munger = SubjectMunger::from_config(&rules);
        assert_eq!(
            munger.munge(Some("[EXTERNAL] Important notice")),
            "EXT Important notice"
        );
        assert_eq!(
            munger.munge(Some("[URGENT] Action required")),
            "URG Action required"
        );
        assert_eq!(
            munger.munge(Some("Fwd: Meeting notes")),
            "FWD Meeting notes"
        );
    }
}
