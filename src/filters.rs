//! Filter rules: what `filters` in `config.yml` asks to be done to mail as it
//! arrives.
//!
//! This module holds the rules and the decision of which rule acts on which
//! message; carrying the actions out is the store's (`store::filtering`).

use serde::Deserialize;

use crate::rules::{Conditions, MessageFacts, RawConditions};

/// The folder a rule watches when it names none.
pub const DEFAULT_FILTER_FOLDER: &str = "INBOX";

/// The rules that parsed, and a line for each that did not.
///
/// A rule that fails to parse is left out rather than failing the whole
/// config: one mistyped glob must not stop mail from being read.
#[derive(Debug, Clone, Default)]
pub struct FilterSet {
    pub rules: Vec<FilterRule>,
    pub warnings: Vec<String>,
}

impl<'de> Deserialize<'de> for FilterSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
        Ok(FilterSet::compile(raw))
    }
}

impl FilterSet {
    fn compile(raw: Vec<serde_json::Value>) -> Self {
        let mut set = FilterSet::default();
        for (index, value) in raw.into_iter().enumerate() {
            let label = value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| format!("filter '{name}'"))
                .unwrap_or_else(|| format!("filter #{}", index + 1));
            let rule = serde_json::from_value::<RawFilter>(value)
                .map_err(|e| e.to_string())
                .and_then(RawFilter::compile);
            match rule {
                Ok(rule) => {
                    if set.rules.iter().any(|r| r.name == rule.name) {
                        set.warnings.push(format!(
                            "{label}: another filter has the same name, so the log cannot tell \
                             them apart"
                        ));
                    }
                    set.rules.push(rule);
                }
                Err(error) => set.warnings.push(format!("{label} is ignored: {error}")),
            }
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rules that watch `folder` of `account`, in order.
    pub fn for_folder(&self, account: &str, folder: &str) -> Vec<&FilterRule> {
        self.rules
            .iter()
            .filter(|rule| rule.applies_to(account, folder))
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFilter {
    name: String,
    #[serde(default)]
    accounts: Vec<String>,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    when: RawConditions,
    #[serde(default)]
    then: RawActions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActions {
    #[serde(rename = "move")]
    move_to: Option<String>,
    #[serde(default)]
    reading: bool,
    #[serde(default)]
    read: bool,
    #[serde(default)]
    flag: bool,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    task: bool,
    #[serde(default)]
    stop: bool,
}

impl RawFilter {
    fn compile(self) -> Result<FilterRule, String> {
        if self.name.trim().is_empty() {
            return Err("`name` is empty".to_string());
        }
        let move_to = match self.then.move_to {
            Some(folder) if folder.trim().is_empty() => {
                return Err("`then.move` names no folder".to_string());
            }
            other => other.map(|folder| folder.trim().to_string()),
        };
        let destinations = [move_to.is_some(), self.then.reading, self.then.delete];
        if destinations.iter().filter(|&&set| set).count() > 1 {
            return Err(
                "`then` has more than one of `move`, `reading` and `delete`; keep one".to_string(),
            );
        }
        let then = Actions {
            move_to,
            reading: self.then.reading,
            read: self.then.read,
            flag: self.then.flag,
            delete: self.then.delete,
            task: self.then.task,
            stop: self.then.stop,
        };
        if then.is_empty() {
            return Err("`then` does nothing".to_string());
        }
        let folders = if self.folders.is_empty() {
            vec![DEFAULT_FILTER_FOLDER.to_string()]
        } else {
            self.folders
        };
        Ok(FilterRule {
            name: self.name,
            accounts: self.accounts,
            folders,
            when: self.when.compile()?,
            then,
        })
    }
}

/// One rule, compiled.
#[derive(Debug, Clone)]
pub struct FilterRule {
    pub name: String,
    /// Empty for every account.
    pub accounts: Vec<String>,
    pub folders: Vec<String>,
    pub when: Conditions,
    pub then: Actions,
}

impl FilterRule {
    pub fn applies_to(&self, account: &str, folder: &str) -> bool {
        (self.accounts.is_empty() || self.accounts.iter().any(|a| a == account))
            && self.folders.iter().any(|f| same_folder(f, folder))
    }
}

/// Whether a folder named in the config is the folder the server calls
/// `actual`: the same name, INBOX in any case, or the name under the `INBOX.`
/// or `INBOX/` prefix of a server that keeps every folder inside the inbox.
pub fn same_folder(configured: &str, actual: &str) -> bool {
    configured == actual
        || (configured.eq_ignore_ascii_case("INBOX") && actual.eq_ignore_ascii_case("INBOX"))
        || ["INBOX.", "INBOX/"].iter().any(|prefix| {
            actual
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                && actual.get(prefix.len()..) == Some(configured)
        })
}

/// Resolve a folder named in the config against the folders an account has:
/// the exact name, the name in another case, or the name inside the prefix
/// the server keeps its folders under — the rule the tasks folder follows.
/// `None` when the account has no such folder.
pub fn resolve_folder(requested: &str, existing: &[String]) -> Option<String> {
    [".", "/"].iter().find_map(|delimiter| {
        let plan = crate::tasks::plan_tasks_folder(requested, Some(delimiter), existing);
        (!plan.create).then_some(plan.name)
    })
}

/// A rule's `then`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actions {
    pub move_to: Option<String>,
    /// To the reading folder, from any account.
    pub reading: bool,
    pub read: bool,
    pub flag: bool,
    /// To Trash; never for good.
    pub delete: bool,
    pub task: bool,
    pub stop: bool,
}

impl Actions {
    fn is_empty(&self) -> bool {
        self == &Actions::default()
    }

    /// Whether the message leaves the folder, after which no later rule can
    /// see it.
    pub fn removes(&self) -> bool {
        self.move_to.is_some() || self.reading || self.delete
    }

    /// The actions as the log and the review panel name them.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.read {
            parts.push("read".to_string());
        }
        if self.flag {
            parts.push("flag".to_string());
        }
        if self.task {
            parts.push("task".to_string());
        }
        if let Some(folder) = &self.move_to {
            parts.push(format!("move to {folder}"));
        }
        if self.reading {
            parts.push("move to reading".to_string());
        }
        if self.delete {
            parts.push("delete".to_string());
        }
        if self.stop {
            parts.push("stop".to_string());
        }
        parts.join(", ")
    }
}

/// The messages one rule acts on, by UID.
#[derive(Debug, Clone)]
pub struct Batch<'r> {
    pub rule: &'r FilterRule,
    pub uids: Vec<u32>,
}

/// Decide which rule acts on which message.
///
/// Each message goes through the rules in order. Every rule that matches acts
/// on it, until one says `stop` or takes the message out of the folder — after
/// a move nothing is left here for a later rule to act on. The result has one
/// batch per rule that matched anything, in rule order, so that a folder of
/// new mail costs one command per rule rather than one per message.
pub fn plan<'r>(rules: &[&'r FilterRule], messages: &[(u32, MessageFacts)]) -> Vec<Batch<'r>> {
    let mut batches: Vec<Batch<'r>> = rules
        .iter()
        .map(|rule| Batch {
            rule,
            uids: Vec::new(),
        })
        .collect();
    for (uid, facts) in messages {
        for batch in batches.iter_mut() {
            if !batch.rule.when.matches(facts) {
                continue;
            }
            batch.uids.push(*uid);
            if batch.rule.then.stop || batch.rule.then.removes() {
                break;
            }
        }
    }
    batches.retain(|batch| !batch.uids.is_empty());
    batches
}

/// The header fields the rules need from the server, beyond what the
/// database keeps. Always names at least one field, so that the fetch also
/// says which of the messages are still there.
pub fn header_fields(rules: &[&FilterRule]) -> Vec<String> {
    let mut fields = vec!["MESSAGE-ID".to_string()];
    for rule in rules {
        for name in rule.when.header_names() {
            if !fields.contains(&name) {
                fields.push(name);
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(yaml: &str) -> FilterSet {
        serde_saphyr::from_str(yaml).expect("the filters parse")
    }

    fn message(from: &str, subject: &str) -> MessageFacts {
        MessageFacts {
            from: Some(from.to_string()),
            subject: Some(subject.to_string()),
            ..MessageFacts::default()
        }
    }

    #[test]
    fn a_rule_that_does_not_parse_is_a_warning_naming_it_and_the_rest_still_work() {
        let set = set("
- name: Good
  when: { from: '*@example.com' }
  then: { read: true }
- name: Typo
  when: { form: '*@example.com' }
  then: { read: true }
- name: Broken
  when: { subject: 're:(' }
  then: { read: true }
- name: Both
  then: { move: Archive, delete: true }
- name: Idle
  when: { from: x }
- when: { from: x }
  then: { read: true }
");
        assert_eq!(
            set.rules
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Good"]
        );
        assert_eq!(set.warnings.len(), 5, "{:#?}", set.warnings);
        assert!(set.warnings[0].starts_with("filter 'Typo' is ignored:"));
        assert!(set.warnings[0].contains("form"), "{}", set.warnings[0]);
        assert!(set.warnings[1].contains("`subject`"), "{}", set.warnings[1]);
        assert!(set.warnings[2].contains("more than one of `move`, `reading` and `delete`"));
        assert!(set.warnings[3].contains("does nothing"));
        assert!(set.warnings[4].starts_with("filter #6 is ignored:"));
    }

    #[test]
    fn a_rule_watches_inbox_of_every_account_unless_told_otherwise() {
        let set = set("
- name: Anywhere
  then: { read: true }
- name: Work lists
  accounts: [Work]
  folders: [Lists]
  then: { read: true }
");
        let names = |account, folder| {
            set.for_folder(account, folder)
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("Home", "INBOX"), vec!["Anywhere"]);
        assert_eq!(names("Home", "Inbox"), vec!["Anywhere"]);
        assert_eq!(names("Work", "Lists"), vec!["Work lists"]);
        assert_eq!(names("Work", "INBOX.Lists"), vec!["Work lists"]);
        assert!(
            names("Work", "Entwürfe").is_empty(),
            "a non-ASCII name is not a panic"
        );
        assert!(names("Work", "INBOX.Entwürfe").is_empty());
        assert!(names("Home", "Lists").is_empty());
        assert!(names("Work", "Archive").is_empty());
    }

    #[test]
    fn stop_and_a_move_end_a_message_s_way_through_the_rules() {
        let set = set("
- name: Read news
  when: { from: '*@news.example.com' }
  then: { read: true }
- name: File news
  when: { from: '*@news.example.com' }
  then: { move: News }
- name: Shield boss
  when: { from: boss@example.com }
  then: { flag: true, stop: true }
- name: Everything else
  then: { task: true }
");
        let rules = set.for_folder("Any", "INBOX");
        let messages = vec![
            (1, message("news@news.example.com", "Digest")),
            (2, message("boss@example.com", "Now")),
            (3, message("friend@example.com", "Hi")),
            (4, message("other@news.example.com", "Digest")),
        ];
        let batches: Vec<(&str, Vec<u32>)> = plan(&rules, &messages)
            .into_iter()
            .map(|b| (b.rule.name.as_str(), b.uids))
            .collect();
        assert_eq!(
            batches,
            vec![
                ("Read news", vec![1, 4]),
                ("File news", vec![1, 4]),
                ("Shield boss", vec![2]),
                ("Everything else", vec![3]),
            ]
        );
    }

    #[test]
    fn a_target_folder_is_found_inside_the_server_s_prefix() {
        let existing = vec![
            "INBOX".to_string(),
            "INBOX.Archive".to_string(),
            "INBOX.News".to_string(),
        ];
        assert_eq!(
            resolve_folder("News", &existing).as_deref(),
            Some("INBOX.News")
        );
        assert_eq!(
            resolve_folder("news", &existing).as_deref(),
            Some("INBOX.News")
        );
        assert_eq!(resolve_folder("Missing", &existing), None);
        let flat = vec![
            "INBOX".to_string(),
            "News".to_string(),
            "Lists/Rust".to_string(),
        ];
        assert_eq!(resolve_folder("News", &flat).as_deref(), Some("News"));
        assert_eq!(
            resolve_folder("Lists/Rust", &flat).as_deref(),
            Some("Lists/Rust")
        );
    }

    #[test]
    fn the_fields_asked_for_are_the_ones_the_rules_need() {
        let set = set("
- name: Lists
  when: { list_id: '*' }
  then: { read: true }
- name: Spam
  when: { header: { X-Spam-Flag: 'YES' } }
  then: { delete: true }
");
        assert_eq!(
            header_fields(&set.for_folder("Any", "INBOX")),
            vec!["MESSAGE-ID", "LIST-ID", "X-SPAM-FLAG"]
        );
    }

    #[test]
    fn a_rule_sends_mail_to_one_place_at_most() {
        let rule = |then: &str| {
            set(&format!(
                "- name: News\n  when: {{ list_id: news }}\n  then: {then}\n"
            ))
        };
        let set = rule("{ reading: true }");
        assert!(set.warnings.is_empty(), "{:?}", set.warnings);
        assert!(set.rules[0].then.reading);
        assert!(set.rules[0].then.removes());
        for both in [
            "{ reading: true, move: News }",
            "{ reading: true, delete: true }",
        ] {
            let set = rule(both);
            assert!(set.rules.is_empty(), "{both}");
            assert!(
                set.warnings[0].contains("more than one"),
                "{:?}",
                set.warnings
            );
        }
    }

    #[test]
    fn actions_are_described_in_the_order_they_happen() {
        let actions = Actions {
            move_to: Some("News".into()),
            read: true,
            task: true,
            ..Actions::default()
        };
        assert_eq!(actions.describe(), "read, task, move to News");
    }
}
