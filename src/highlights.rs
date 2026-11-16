//! Highlight rules: how `highlights` in `config.yml` changes the look of a
//! message without touching it on the server.
//!
//! A highlight is evaluated when a row is rendered, so it applies at once to
//! every message, old and new, and a rule changed in the config shows after a
//! reload with nothing to re-run. The conditions are the filter rules'
//! vocabulary ([`crate::rules`]) plus `any`, the flags, the folder, the age and
//! the body; this module holds the rules, their styles and the decision of
//! which rules a message matches. Finding the facts is the store's
//! (`store::highlighting`).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::rules::{Conditions, MessageFacts, Pattern, RawConditions, Scalar};

/// The rules that parsed, and a line for each that did not or that was
/// adjusted.
#[derive(Debug, Clone, Default)]
pub struct HighlightSet {
    pub rules: Vec<HighlightRule>,
    pub warnings: Vec<String>,
}

impl<'de> Deserialize<'de> for HighlightSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
        Ok(HighlightSet::compile(raw))
    }
}

impl HighlightSet {
    fn compile(raw: Vec<serde_json::Value>) -> Self {
        let mut set = HighlightSet::default();
        for (index, value) in raw.into_iter().enumerate() {
            let label = value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| format!("highlight '{name}'"))
                .unwrap_or_else(|| format!("highlight #{}", index + 1));
            let rule = serde_json::from_value::<RawHighlight>(value)
                .map_err(|e| e.to_string())
                .and_then(|raw| raw.compile(&label, &mut set.warnings));
            match rule {
                Ok(rule) => set.rules.push(rule),
                Err(error) => set.warnings.push(format!("{label} is ignored: {error}")),
            }
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rules that apply to an account, in order.
    pub fn for_account(&self, account: &str) -> Vec<&HighlightRule> {
        self.rules
            .iter()
            .filter(|rule| rule.accounts.is_empty() || rule.accounts.iter().any(|a| a == account))
            .collect()
    }

    /// The names of the rules a message matches, in rule order.
    pub fn matching(&self, account: &str, facts: &HighlightFacts<'_>) -> Vec<String> {
        self.for_account(account)
            .into_iter()
            .filter(|rule| rule.when.matches(facts))
            .map(|rule| rule.name.clone())
            .collect()
    }

    /// Every `body` text some rule looks for, lower-cased: what the store has
    /// to ask the database about before the rules can be evaluated.
    pub fn body_needles(&self) -> Vec<String> {
        let mut needles = Vec::new();
        for rule in &self.rules {
            rule.when.collect_body_needles(&mut needles);
        }
        needles
    }

    /// Whether some rule looks at a header beyond the envelope.
    pub fn needs_headers(&self) -> bool {
        self.rules.iter().any(|rule| rule.when.needs_headers())
    }

    /// The headers the rules name with `header:`, upper-cased as the sync
    /// asks the server for them, without `List-Id`, which it asks for anyway.
    pub fn extra_header_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for rule in &self.rules {
            rule.when.collect_header_names(&mut names);
        }
        names.retain(|name| name != "LIST-ID");
        names.sort();
        names.dedup();
        names
    }

    /// The rules and their styles, as the interface draws them.
    pub fn styles(&self) -> Vec<HighlightStyle> {
        self.rules
            .iter()
            .map(|rule| HighlightStyle {
                name: rule.name.clone(),
                style: rule.style.clone(),
            })
            .collect()
    }
}

/// The `load_remote_images` rules: messages whose HTML may load its remote
/// images without being asked. A highlight's `name`, `accounts` and `when`,
/// with nothing to draw, so they share the highlights' evaluation.
#[derive(Debug, Clone, Default)]
pub struct RemoteImageRules(pub HighlightSet);

impl<'de> Deserialize<'de> for RemoteImageRules {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
        let mut set = HighlightSet::default();
        for (index, value) in raw.into_iter().enumerate() {
            let label = value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| format!("load_remote_images '{name}'"))
                .unwrap_or_else(|| format!("load_remote_images #{}", index + 1));
            let rule = serde_json::from_value::<RawRemoteImageRule>(value)
                .map_err(|e| e.to_string())
                .and_then(|raw| raw.compile(index));
            match rule {
                Ok(rule) => set.rules.push(rule),
                Err(error) => set.warnings.push(format!("{label} is ignored: {error}")),
            }
        }
        Ok(RemoteImageRules(set))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemoteImageRule {
    name: Option<String>,
    #[serde(default)]
    accounts: Vec<String>,
    when: RawHighlightConditions,
}

impl RawRemoteImageRule {
    fn compile(self, index: usize) -> Result<HighlightRule, String> {
        let when = self.when.compile()?;
        if when.is_empty() {
            return Err("`when` is empty, which would load every message's images".to_string());
        }
        Ok(HighlightRule {
            name: self
                .name
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("#{}", index + 1)),
            accounts: self.accounts,
            when,
            style: Style::default(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHighlight {
    name: String,
    #[serde(default)]
    accounts: Vec<String>,
    #[serde(default)]
    when: RawHighlightConditions,
    #[serde(default)]
    style: RawStyle,
}

impl RawHighlight {
    fn compile(self, label: &str, warnings: &mut Vec<String>) -> Result<HighlightRule, String> {
        if self.name.trim().is_empty() {
            return Err("`name` is empty".to_string());
        }
        let when = self.when.compile()?;
        let style = self.style.compile(label, warnings)?;
        Ok(HighlightRule {
            name: self.name,
            accounts: self.accounts,
            when,
            style,
        })
    }
}

/// One rule, compiled.
#[derive(Debug, Clone)]
pub struct HighlightRule {
    pub name: String,
    /// Empty for every account.
    pub accounts: Vec<String>,
    pub when: HighlightConditions,
    pub style: Style,
}

/// A rule's `when`: the filter conditions, and the ones only a rendering can
/// answer.
#[derive(Debug, Clone, Default)]
pub struct HighlightConditions {
    pub base: Conditions,
    /// At least one of these must hold, besides everything else.
    pub any: Vec<HighlightConditions>,
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
    pub folder: Option<Pattern>,
    /// Seconds. `older_than: 7d` holds for a message dated more than a week
    /// ago; `newer_than`, for one dated less than that ago.
    pub older_than: Option<i64>,
    pub newer_than: Option<i64>,
    /// Text the body has to contain, lower-cased. False until the body has
    /// been fetched.
    pub body: Option<String>,
}

impl HighlightConditions {
    pub fn matches(&self, facts: &HighlightFacts<'_>) -> bool {
        self.base.matches(facts.base)
            && (self.any.is_empty() || self.any.iter().any(|one| one.matches(facts)))
            && self.unread.is_none_or(|wanted| wanted == facts.unread)
            && self.flagged.is_none_or(|wanted| wanted == facts.flagged)
            && self
                .folder
                .as_ref()
                .is_none_or(|pattern| pattern.matches(facts.folder))
            && self
                .older_than
                .is_none_or(|seconds| facts.age_seconds.is_some_and(|age| age > seconds))
            && self
                .newer_than
                .is_none_or(|seconds| facts.age_seconds.is_some_and(|age| age < seconds))
            && self
                .body
                .as_ref()
                .is_none_or(|needle| facts.body_hits.contains(needle))
    }

    fn collect_body_needles(&self, needles: &mut Vec<String>) {
        if let Some(needle) = &self.body
            && !needles.contains(needle)
        {
            needles.push(needle.clone());
        }
        for one in &self.any {
            one.collect_body_needles(needles);
        }
    }

    /// Whether no condition is given, so that every message matches.
    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
            && self.any.is_empty()
            && self.unread.is_none()
            && self.flagged.is_none()
            && self.folder.is_none()
            && self.older_than.is_none()
            && self.newer_than.is_none()
            && self.body.is_none()
    }

    fn needs_headers(&self) -> bool {
        !self.base.header_names().is_empty() || self.any.iter().any(|one| one.needs_headers())
    }

    fn collect_header_names(&self, names: &mut Vec<String>) {
        names.extend(self.base.header_names());
        for one in &self.any {
            one.collect_header_names(names);
        }
    }
}

/// What a message offers a highlight rule beyond [`MessageFacts`].
#[derive(Debug, Clone)]
pub struct HighlightFacts<'a> {
    pub base: &'a MessageFacts,
    pub unread: bool,
    pub flagged: bool,
    pub folder: &'a str,
    /// How long ago the message is dated, when it has a readable date.
    pub age_seconds: Option<i64>,
    /// The `body` texts, lower-cased, that the stored body contains. Empty for
    /// a body not fetched yet.
    pub body_hits: &'a HashSet<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHighlightConditions {
    from: Option<Scalar>,
    to: Option<Scalar>,
    cc: Option<Scalar>,
    subject: Option<Scalar>,
    list_id: Option<Scalar>,
    #[serde(default)]
    header: std::collections::BTreeMap<String, Scalar>,
    has_attachment: Option<bool>,
    #[serde(default)]
    any: Vec<RawHighlightConditions>,
    unread: Option<bool>,
    flagged: Option<bool>,
    folder: Option<Scalar>,
    older_than: Option<Scalar>,
    newer_than: Option<Scalar>,
    body: Option<Scalar>,
}

impl RawHighlightConditions {
    fn compile(self) -> Result<HighlightConditions, String> {
        let base = RawConditions {
            from: self.from,
            to: self.to,
            cc: self.cc,
            subject: self.subject,
            list_id: self.list_id,
            header: self.header,
            has_attachment: self.has_attachment,
        }
        .compile()?;
        let duration = |key: &str, value: Option<Scalar>| {
            value
                .map(|v| parse_duration(&v.0).map_err(|e| format!("`{key}`: {e}")))
                .transpose()
        };
        Ok(HighlightConditions {
            base,
            any: self
                .any
                .into_iter()
                .map(|one| one.compile().map_err(|e| format!("`any`: {e}")))
                .collect::<Result<_, _>>()?,
            unread: self.unread,
            flagged: self.flagged,
            folder: self
                .folder
                .map(|v| Pattern::parse(&v.0).map_err(|e| format!("`folder`: {e}")))
                .transpose()?,
            older_than: duration("older_than", self.older_than)?,
            newer_than: duration("newer_than", self.newer_than)?,
            body: self
                .body
                .map(|v| v.0.trim().to_lowercase())
                .filter(|needle| !needle.is_empty()),
        })
    }
}

/// `30m`, `12h`, `7d`, `2w`: a number and a unit, in seconds.
pub fn parse_duration(text: &str) -> Result<i64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("'{text}' has no unit; write 30m, 12h, 7d or 2w"))?;
    let (number, unit) = text.split_at(split);
    let number: i64 = number
        .parse()
        .map_err(|_| format!("'{text}' does not begin with a number"))?;
    let seconds = match unit.trim() {
        "m" | "min" | "minute" | "minutes" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 7 * 86_400,
        other => return Err(format!("'{other}' is not a unit; write m, h, d or w")),
    };
    Ok(number * seconds)
}

/// How a highlighted message is drawn. Colours are CSS, ready to use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Style {
    /// Text colour of the sender and the subject.
    pub color: Option<String>,
    /// Row background, at low alpha.
    pub background: Option<String>,
    /// A Font Awesome icon name, `fa-crown`.
    pub icon: Option<String>,
    pub icon_color: Option<String>,
    /// A small chip at the end of the subject.
    pub label: Option<String>,
    pub label_color: Option<String>,
    pub bold: bool,
    pub italic: bool,
    /// The row recedes instead of standing out.
    pub dim: bool,
    /// Count the rule's unread messages next to the folder.
    pub sidebar_badge: bool,
}

/// A rule's name with its style, for the interface.
#[derive(Debug, Clone, Serialize)]
pub struct HighlightStyle {
    pub name: String,
    pub style: Style,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStyle {
    color: Option<String>,
    background: Option<String>,
    icon: Option<String>,
    icon_color: Option<String>,
    label: Option<String>,
    label_color: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default)]
    dim: bool,
    #[serde(default)]
    sidebar_badge: bool,
}

impl RawStyle {
    fn compile(self, label: &str, warnings: &mut Vec<String>) -> Result<Style, String> {
        let color = |key: &str, value: Option<String>| {
            value
                .map(|v| Color::parse(&v).map_err(|e| format!("`style.{key}`: {e}")))
                .transpose()
        };
        let text = color("color", self.color)?;
        let mut background = color("background", self.background)?;
        if let Some(bg) = background.as_mut() {
            let against = text.as_ref().map(Color::rgba).unwrap_or(theme::FG);
            if let Some(adjusted) = bg.readable_against(against) {
                warnings.push(format!(
                    "{label}: `style.background` {} does not contrast enough with the text; \
                     drawn as {} instead",
                    bg.css(),
                    adjusted.css()
                ));
                *bg = adjusted;
            }
        }
        let icon = match self.icon {
            Some(icon) => {
                let icon = icon.trim().to_string();
                let name = icon.strip_prefix("fa-").unwrap_or(&icon);
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    return Err(format!("`style.icon`: '{icon}' is not an icon name"));
                }
                Some(format!("fa-{name}"))
            }
            None => None,
        };
        let style = Style {
            color: text.as_ref().map(Color::css),
            background: background.as_ref().map(Color::css),
            icon,
            icon_color: color("icon_color", self.icon_color)?.map(|c| c.css()),
            label: self
                .label
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
            label_color: color("label_color", self.label_color)?.map(|c| c.css()),
            bold: self.bold,
            italic: self.italic,
            dim: self.dim,
            sidebar_badge: self.sidebar_badge,
        };
        if style == Style::default() {
            return Err("`style` changes nothing".to_string());
        }
        Ok(style)
    }
}

/// The interface's dark palette, as `styles.css` has it, for the contrast
/// check: the pane, its text, and the colours a rule may name, which the
/// light appearance gives values of its own.
pub mod theme {
    use super::Rgba;

    pub const BG: Rgba = Rgba::opaque(0x1c, 0x1d, 0x21);
    pub const FG: Rgba = Rgba::opaque(0xe7, 0xe8, 0xeb);

    /// The colours a rule may name instead of writing.
    pub const NAMED: &[(&str, Rgba)] = &[
        ("accent", Rgba::opaque(0x7d, 0x97, 0xff)),
        ("warning", Rgba::opaque(0xe7, 0xb0, 0x4c)),
        ("danger", Rgba::opaque(0xf1, 0x79, 0x72)),
        ("success", Rgba::opaque(0x5e, 0xc4, 0x8c)),
        ("muted", Rgba::opaque(0x99, 0x9c, 0xa3)),
    ];
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Rgba {
    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// WCAG relative luminance.
    fn luminance(self) -> f64 {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// This colour drawn over `under`.
    fn over(self, under: Rgba) -> Rgba {
        let a = f64::from(self.a).clamp(0.0, 1.0);
        let mix = |top: u8, bottom: u8| {
            (f64::from(top) * a + f64::from(bottom) * (1.0 - a)).round() as u8
        };
        Rgba::opaque(
            mix(self.r, under.r),
            mix(self.g, under.g),
            mix(self.b, under.b),
        )
    }
}

/// WCAG contrast ratio of two opaque colours.
pub fn contrast(a: Rgba, b: Rgba) -> f64 {
    let (la, lb) = (a.luminance(), b.luminance());
    let (light, dark) = if la > lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

/// The least contrast WCAG AA asks of ordinary text.
const MIN_CONTRAST: f64 = 4.5;

/// A colour a rule names: one of the theme's, or one written out.
#[derive(Debug, Clone, PartialEq)]
pub enum Color {
    Named(&'static str, Rgba),
    Value(Rgba),
}

impl Color {
    /// `accent`, `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(r, g, b)` or
    /// `rgba(r, g, b, a)`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if let Some((name, rgba)) = theme::NAMED
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(text))
        {
            return Ok(Color::Named(name, *rgba));
        }
        if let Some(hex) = text.strip_prefix('#') {
            let digits: Vec<u8> = hex
                .chars()
                .map(|c| c.to_digit(16).map(|d| d as u8))
                .collect::<Option<_>>()
                .ok_or_else(|| format!("'{text}' is not a hex colour"))?;
            let (r, g, b, a) = match digits.as_slice() {
                [r, g, b] => (r * 17, g * 17, b * 17, 255),
                [r, g, b, a] => (r * 17, g * 17, b * 17, a * 17),
                [r1, r2, g1, g2, b1, b2] => (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, 255),
                [r1, r2, g1, g2, b1, b2, a1, a2] => {
                    (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, a1 * 16 + a2)
                }
                _ => return Err(format!("'{text}' is not a hex colour")),
            };
            return Ok(Color::Value(Rgba {
                r,
                g,
                b,
                a: f32::from(a) / 255.0,
            }));
        }
        let lower = text.to_ascii_lowercase();
        if let Some(inner) = lower
            .strip_prefix("rgba(")
            .or_else(|| lower.strip_prefix("rgb("))
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let parts: Vec<&str> = inner
                .split([',', '/', ' '])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect();
            if parts.len() != 3 && parts.len() != 4 {
                return Err(format!("'{text}' is not an rgb() colour"));
            }
            let channel = |part: &str| -> Result<u8, String> {
                let value: f64 = part
                    .strip_suffix('%')
                    .map(|p| p.parse::<f64>().map(|v| v * 2.55))
                    .unwrap_or_else(|| part.parse())
                    .map_err(|_| format!("'{text}' is not an rgb() colour"))?;
                Ok(value.round().clamp(0.0, 255.0) as u8)
            };
            let a = match parts.get(3) {
                Some(part) => part
                    .strip_suffix('%')
                    .map(|p| p.parse::<f32>().map(|v| v / 100.0))
                    .unwrap_or_else(|| part.parse())
                    .map_err(|_| format!("'{text}' is not an rgb() colour"))?,
                None => 1.0,
            };
            return Ok(Color::Value(Rgba {
                r: channel(parts[0])?,
                g: channel(parts[1])?,
                b: channel(parts[2])?,
                a: a.clamp(0.0, 1.0),
            }));
        }
        Err(format!(
            "'{text}' is not a colour Orb knows; write #rrggbb, rgba(), or one of accent, \
             warning, danger, success, muted"
        ))
    }

    pub fn rgba(&self) -> Rgba {
        match self {
            Color::Named(_, rgba) | Color::Value(rgba) => *rgba,
        }
    }

    pub fn css(&self) -> String {
        match self {
            Color::Named(name, _) => format!("var(--{name})"),
            Color::Value(c) => {
                if c.a >= 1.0 {
                    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
                } else {
                    format!("rgba({}, {}, {}, {:.2})", c.r, c.g, c.b, c.a)
                }
            }
        }
    }

    /// This colour as a row background, darkened or lightened until `text`
    /// reads on it; `None` when it reads as it is. The alpha is kept: it is
    /// the background as drawn over the pane that has to contrast.
    pub fn readable_against(&self, text: Rgba) -> Option<Color> {
        let mut candidate = self.rgba();
        if contrast(candidate.over(theme::BG), text) >= MIN_CONTRAST {
            return None;
        }
        // Move away from the text: a light text wants a darker background.
        let towards: u8 = if text.luminance() > 0.5 { 0 } else { 255 };
        for _ in 0..40 {
            let step = |c: u8| -> u8 {
                let c = f64::from(c);
                (c + (f64::from(towards) - c) * 0.1).round() as u8
            };
            candidate = Rgba {
                r: step(candidate.r),
                g: step(candidate.g),
                b: step(candidate.b),
                a: candidate.a,
            };
            if contrast(candidate.over(theme::BG), text) >= MIN_CONTRAST {
                return Some(Color::Value(candidate));
            }
        }
        Some(Color::Value(candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(yaml: &str) -> HighlightSet {
        serde_saphyr::from_str(yaml).expect("the highlights parse")
    }

    fn facts<'a>(
        base: &'a MessageFacts,
        folder: &'a str,
        hits: &'a HashSet<String>,
    ) -> HighlightFacts<'a> {
        HighlightFacts {
            base,
            unread: true,
            flagged: false,
            folder,
            age_seconds: Some(3 * 86_400),
            body_hits: hits,
        }
    }

    #[test]
    fn any_is_or_within_a_rule_and_the_rest_still_must_hold() {
        let set = set("
- name: Invoices
  when:
    any:
      - subject: 're:(?i)invoice|rechnung'
      - has_attachment: true
        from: '*@billing.example.com'
    unread: true
  style: { bold: true }
");
        let hits = HashSet::new();
        let by_subject = MessageFacts {
            subject: Some("Your invoice".into()),
            ..MessageFacts::default()
        };
        let by_sender = MessageFacts {
            from: Some("bills@billing.example.com".into()),
            has_attachment: true,
            ..MessageFacts::default()
        };
        let neither = MessageFacts {
            from: Some("bills@billing.example.com".into()),
            ..MessageFacts::default()
        };
        assert_eq!(
            set.matching("A", &facts(&by_subject, "INBOX", &hits)),
            vec!["Invoices"]
        );
        assert_eq!(
            set.matching("A", &facts(&by_sender, "INBOX", &hits)),
            vec!["Invoices"]
        );
        assert!(
            set.matching("A", &facts(&neither, "INBOX", &hits))
                .is_empty()
        );
        let mut read = facts(&by_subject, "INBOX", &hits);
        read.unread = false;
        assert!(
            set.matching("A", &read).is_empty(),
            "`unread` still has to hold"
        );
    }

    #[test]
    fn a_body_condition_is_false_until_the_body_is_there() {
        let set = set("
- name: Due
  when: { body: 'Payment Due' }
  style: { label: due }
");
        assert_eq!(set.body_needles(), vec!["payment due"]);
        let base = MessageFacts::default();
        let none = HashSet::new();
        assert!(set.matching("A", &facts(&base, "INBOX", &none)).is_empty());
        let hits: HashSet<String> = ["payment due".to_string()].into();
        assert_eq!(
            set.matching("A", &facts(&base, "INBOX", &hits)),
            vec!["Due"]
        );
    }

    #[test]
    fn folder_age_and_flags_are_conditions_too() {
        let set = set("
- name: Stale
  when: { folder: 'INBOX', older_than: 2d, flagged: false }
  style: { dim: true }
- name: Fresh
  when: { newer_than: 1w }
  style: { italic: true }
");
        let base = MessageFacts::default();
        let hits = HashSet::new();
        assert_eq!(
            set.matching("A", &facts(&base, "INBOX", &hits)),
            vec!["Stale", "Fresh"]
        );
        assert_eq!(
            set.matching("A", &facts(&base, "Archive", &hits)),
            vec!["Fresh"]
        );
        let mut undated = facts(&base, "INBOX", &hits);
        undated.age_seconds = None;
        assert!(set.matching("A", &undated).is_empty(), "no date, no age");
    }

    #[test]
    fn rules_match_in_order_and_only_for_their_accounts() {
        let set = set("
- name: Boss
  accounts: [Work]
  when: { from: anna@example.com }
  style: { color: '#e6b422', icon: crown }
- name: Everyone
  style: { dim: true }
");
        let base = MessageFacts {
            from: Some("Anna <anna@example.com>".into()),
            ..MessageFacts::default()
        };
        let hits = HashSet::new();
        assert_eq!(
            set.matching("Work", &facts(&base, "INBOX", &hits)),
            vec!["Boss", "Everyone"]
        );
        assert_eq!(
            set.matching("Home", &facts(&base, "INBOX", &hits)),
            vec!["Everyone"]
        );
        assert_eq!(set.styles()[0].style.icon.as_deref(), Some("fa-crown"));
    }

    #[test]
    fn colours_are_the_theme_s_or_written_out() {
        assert_eq!(Color::parse("accent").unwrap().css(), "var(--accent)");
        assert_eq!(Color::parse("#E6B422").unwrap().css(), "#e6b422");
        assert_eq!(Color::parse("#fff").unwrap().css(), "#ffffff");
        assert_eq!(
            Color::parse("rgba(76, 141, 255, 0.10)").unwrap().css(),
            "rgba(76, 141, 255, 0.10)"
        );
        assert_eq!(
            Color::parse("rgb(0 0 0 / 50%)").unwrap().css(),
            "rgba(0, 0, 0, 0.50)"
        );
        assert!(Color::parse("red").is_err());
        assert!(Color::parse("#12345").is_err());
    }

    #[test]
    fn a_light_background_is_darkened_until_the_text_reads_and_says_so() {
        let set = set("
- name: Loud
  style: { background: '#ffffff' }
- name: Quiet
  style: { background: 'rgba(76, 141, 255, 0.10)' }
- name: Dark text
  style: { color: '#111111', background: '#ffffff' }
");
        assert_eq!(set.rules.len(), 3);
        assert_eq!(set.warnings.len(), 1, "{:?}", set.warnings);
        assert!(set.warnings[0].starts_with("highlight 'Loud': `style.background` #ffffff"));
        let adjusted = Color::parse(set.rules[0].style.background.as_deref().unwrap()).unwrap();
        assert!(contrast(adjusted.rgba(), theme::FG) >= MIN_CONTRAST);
        assert_eq!(
            set.rules[1].style.background.as_deref(),
            Some("rgba(76, 141, 255, 0.10)"),
            "a low-alpha tint over the dark pane reads fine"
        );
        assert_eq!(set.rules[2].style.background.as_deref(), Some("#ffffff"));
    }

    #[test]
    fn a_rule_that_does_not_parse_is_a_warning_and_the_rest_still_work() {
        let set = set("
- name: Good
  style: { bold: true }
- name: No style
  when: { from: x }
- name: Bad colour
  style: { color: purple }
- name: Bad age
  when: { older_than: soon }
  style: { dim: true }
- name: Bad icon
  style: { icon: 'crown jewels' }
- name: Typo
  styles: { dim: true }
");
        assert_eq!(set.rules.len(), 1);
        assert_eq!(set.warnings.len(), 5, "{:#?}", set.warnings);
        assert!(set.warnings[0].contains("changes nothing"));
        assert!(set.warnings[1].contains("`style.color`"));
        assert!(set.warnings[2].contains("`older_than`"));
        assert!(set.warnings[3].contains("`style.icon`"));
        assert!(set.warnings[4].contains("styles"));
    }

    #[test]
    fn remote_image_rules_need_a_condition_but_no_name_or_style() {
        let rules: RemoteImageRules = serde_saphyr::from_str(
            "
- when: { subject: '*digest*' }
- name: Shop
  accounts: [Work]
  when: { list_id: '*.shop.example' }
- name: Everything
  when: {}
- name: Styled
  when: { from: x }
  style: { bold: true }
",
        )
        .expect("the rules parse");
        let set = &rules.0;
        assert_eq!(
            set.rules
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["#1", "Shop"]
        );
        assert_eq!(set.warnings.len(), 2, "{:#?}", set.warnings);
        assert!(set.warnings[0].contains("'Everything'") && set.warnings[0].contains("empty"));
        assert!(set.warnings[1].contains("style"));
        assert_eq!(set.extra_header_names(), Vec::<String>::new());
        assert!(set.needs_headers());

        let hits = HashSet::new();
        let digest = MessageFacts {
            subject: Some("Weekly digest".into()),
            ..MessageFacts::default()
        };
        assert_eq!(
            set.matching("Home", &facts(&digest, "INBOX", &hits)),
            vec!["#1"]
        );
    }

    #[test]
    fn durations_are_a_number_and_a_unit() {
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("12h").unwrap(), 43_200);
        assert_eq!(parse_duration("7 days").unwrap(), 7 * 86_400);
        assert_eq!(parse_duration("2w").unwrap(), 14 * 86_400);
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("7y").is_err());
    }
}
