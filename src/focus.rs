//! Focus: a while in which Orb goes on syncing but keeps to itself what it
//! would announce.
//!
//! A focus is a *session* (a mode, and when it ends) kept in the database, so
//! that it outlives a restart. The mode, out of `focus.modes` in the config,
//! says what is held back: the notifications, the Dock badge, the chat. What
//! is held is announced when the mode's interval comes round, or when the
//! focus ends. Mail that matches a highlight rule with `breaks_focus` is
//! announced at once, whatever the mode.
//!
//! This module holds the modes, the session and the decisions; the database
//! and the notifications are [`crate::ffi::OrbHandle`]'s.

use serde::{Deserialize, Serialize};

use crate::rules::Scalar;

/// The shortest interval a mode may release what it holds at.
const SHORTEST_INTERVAL_SECONDS: i64 = 60;

/// What a mode does with the notifications for new mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusNotify {
    /// Announced as it arrives.
    On,
    /// Held until the focus ends.
    Off,
    /// Held, and announced together every so many seconds.
    Every(i64),
}

/// What a mode leaves on the Dock icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusBadge {
    On,
    /// Only what breaks the focus is counted.
    Exceptions,
    Off,
}

/// Which chat threads a mode announces as they start waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusChat {
    On,
    /// Only a thread that names the user.
    Mentions,
    Off,
}

/// One entry of `focus.modes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusMode {
    pub name: String,
    pub notify: FocusNotify,
    pub badge: FocusBadge,
    pub chat: FocusChat,
}

impl FocusMode {
    /// The mode in a line, for the picker.
    pub fn describe(&self) -> String {
        let notify = match self.notify {
            FocusNotify::On => "mail announced".to_string(),
            FocusNotify::Off => "mail held".to_string(),
            FocusNotify::Every(seconds) => format!("mail every {}", span(seconds)),
        };
        let badge = match self.badge {
            FocusBadge::On => "badge on",
            FocusBadge::Exceptions => "badge for exceptions",
            FocusBadge::Off => "badge off",
        };
        let chat = match self.chat {
            FocusChat::On => "chat announced",
            FocusChat::Mentions => "chat for mentions",
            FocusChat::Off => "chat held",
        };
        format!("{notify} · {badge} · {chat}")
    }

    /// Whether a chat thread is announced as it starts waiting.
    pub fn admits_chat(&self, mention: bool) -> bool {
        match self.chat {
            FocusChat::On => true,
            FocusChat::Mentions => mention,
            FocusChat::Off => false,
        }
    }
}

/// Seconds as the config writes them: `30m`, `2h`, `90s`.
fn span(seconds: i64) -> String {
    if seconds % 3600 == 0 {
        format!("{}h", seconds / 3600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// The `focus` section of the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusConfig {
    /// When the workday ends, as `HH:MM`; what `workday` stands for as a span.
    pub workday_ends: Option<String>,
    /// The mode a focus starts in when none is named. Always one of `modes`.
    pub default: String,
    /// In the order the config lists them; never empty.
    pub modes: Vec<FocusMode>,
    /// A line for everything that was left out or adjusted.
    pub warnings: Vec<String>,
}

impl Default for FocusConfig {
    fn default() -> Self {
        FocusConfig {
            workday_ends: None,
            default: "deep".to_string(),
            modes: builtin_modes(),
            warnings: Vec::new(),
        }
    }
}

/// The modes of a config that names none.
fn builtin_modes() -> Vec<FocusMode> {
    vec![
        FocusMode {
            name: "deep".to_string(),
            notify: FocusNotify::Off,
            badge: FocusBadge::Exceptions,
            chat: FocusChat::Off,
        },
        FocusMode {
            name: "batched".to_string(),
            notify: FocusNotify::Every(30 * 60),
            badge: FocusBadge::Off,
            chat: FocusChat::Mentions,
        },
    ]
}

impl FocusConfig {
    pub fn mode(&self, name: &str) -> Option<&FocusMode> {
        self.modes.iter().find(|mode| mode.name == name)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFocus {
    workday_ends: Option<Scalar>,
    default: Option<String>,
    #[serde(default)]
    modes: OrderedModes,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMode {
    notify: Option<Scalar>,
    badge: Option<Scalar>,
    chat: Option<Scalar>,
}

/// `modes` as the file lists them: the first is the default, and the picker
/// offers them in this order.
#[derive(Default)]
struct OrderedModes(Vec<(String, RawMode)>);

impl<'de> Deserialize<'de> for OrderedModes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedModes;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the focus modes by name")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<OrderedModes, A::Error> {
                let mut modes = Vec::new();
                while let Some((name, mode)) = map.next_entry::<String, Option<RawMode>>()? {
                    modes.push((name, mode.unwrap_or_default()));
                }
                Ok(OrderedModes(modes))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl<'de> Deserialize<'de> for FocusConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(RawFocus::deserialize(deserializer)?.compile())
    }
}

impl RawFocus {
    fn compile(self) -> FocusConfig {
        let mut warnings = Vec::new();
        let workday_ends = self.workday_ends.and_then(|value| {
            let parsed = clock(&value.0);
            if parsed.is_none() {
                warnings.push(format!(
                    "focus.workday_ends: '{}' is not a time like 17:30; it is left out",
                    value.0
                ));
            }
            parsed
        });

        let mut modes: Vec<FocusMode> = Vec::new();
        for (name, raw) in self.modes.0 {
            let name = name.trim().to_string();
            // `off` ends a focus, and a mode named like a span could not be told from one.
            if name.is_empty() || name.contains(char::is_whitespace) || is_reserved(&name) {
                warnings.push(format!(
                    "focus.modes: '{name}' cannot name a mode; it is left out"
                ));
                continue;
            }
            modes.push(raw.compile(name, &mut warnings));
        }
        if modes.is_empty() {
            modes = builtin_modes();
        }

        let default = match self.default {
            Some(name) if modes.iter().any(|mode| mode.name == name) => name,
            Some(name) => {
                warnings.push(format!(
                    "focus.default: there is no mode '{name}'; '{}' is the default",
                    modes[0].name
                ));
                modes[0].name.clone()
            }
            None => modes[0].name.clone(),
        };
        FocusConfig {
            workday_ends,
            default,
            modes,
            warnings,
        }
    }
}

impl RawMode {
    fn compile(self, name: String, warnings: &mut Vec<String>) -> FocusMode {
        let mut warn = |key: &str, value: &str, instead: &str| {
            warnings.push(format!(
                "focus.modes.{name}.{key}: '{value}' is not understood; it is {instead}"
            ));
        };
        let notify = match self.notify.as_ref().map(|v| v.0.trim().to_lowercase()) {
            None => FocusNotify::Off,
            Some(text) => match text.as_str() {
                "off" | "false" | "no" | "none" => FocusNotify::Off,
                "on" | "true" | "yes" => FocusNotify::On,
                other => match crate::highlights::parse_duration(other) {
                    Ok(seconds) => FocusNotify::Every(seconds.max(SHORTEST_INTERVAL_SECONDS)),
                    Err(_) => {
                        warn("notify", other, "off");
                        FocusNotify::Off
                    }
                },
            },
        };
        let badge = match self.badge.as_ref().map(|v| v.0.trim().to_lowercase()) {
            None => FocusBadge::Exceptions,
            Some(text) => match text.as_str() {
                "off" | "false" | "no" | "none" => FocusBadge::Off,
                "on" | "true" | "yes" => FocusBadge::On,
                "exceptions" => FocusBadge::Exceptions,
                other => {
                    warn("badge", other, "exceptions");
                    FocusBadge::Exceptions
                }
            },
        };
        let chat = match self.chat.as_ref().map(|v| v.0.trim().to_lowercase()) {
            None => FocusChat::Off,
            Some(text) => match text.as_str() {
                "off" | "false" | "no" | "none" => FocusChat::Off,
                "on" | "true" | "yes" => FocusChat::On,
                "mentions" => FocusChat::Mentions,
                other => {
                    warn("chat", other, "off");
                    FocusChat::Off
                }
            },
        };
        FocusMode {
            name,
            notify,
            badge,
            chat,
        }
    }
}

/// The words the `focus-mode` command reads as something other than a mode.
fn is_reserved(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "off" | "forever" | "indefinitely" | "today" | "noon" | "workday"
    ) || name.starts_with(|c: char| c.is_ascii_digit())
}

/// `17:30` or `9:00` as `HH:MM`.
fn clock(text: &str) -> Option<String> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let (hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours < 24 && minutes < 60).then(|| format!("{hours:02}:{minutes:02}"))
}

/// A focus that was started: the mode by name, and until when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FocusSession {
    pub mode: String,
    /// Milliseconds since the epoch.
    pub started_at: i64,
    /// Milliseconds since the epoch; `None` until the user ends it.
    pub ends_at: Option<i64>,
}

impl FocusSession {
    pub fn is_over(&self, now_ms: i64) -> bool {
        self.ends_at.is_some_and(|ends| ends <= now_ms)
    }
}

/// What the clock asks of a running focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDue {
    Nothing,
    /// The mode's interval came round: announce what is held, and go on.
    Release,
    /// The focus is over: announce what is held, and forget the session.
    End,
}

/// What is due at `now_ms` for `session`, whose mode is `mode` if the config
/// still has it, when what was held was last announced at `last_release_ms`.
pub fn due(
    session: &FocusSession,
    mode: Option<&FocusMode>,
    last_release_ms: i64,
    now_ms: i64,
) -> FocusDue {
    let Some(mode) = mode else {
        return FocusDue::End;
    };
    if session.is_over(now_ms) {
        return FocusDue::End;
    }
    match mode.notify {
        FocusNotify::Every(seconds)
            if now_ms.saturating_sub(last_release_ms) >= seconds.saturating_mul(1000) =>
        {
            FocusDue::Release
        }
        _ => FocusDue::Nothing,
    }
}

/// One mode, as the interface lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FocusModeInfo {
    pub name: String,
    pub detail: String,
}

/// Where focus stands, for the status line and the picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FocusStatus {
    /// The running focus, if one is.
    pub session: Option<FocusSession>,
    pub modes: Vec<FocusModeInfo>,
    pub default_mode: String,
    pub workday_ends: Option<String>,
}

impl FocusStatus {
    pub fn new(config: &FocusConfig, session: Option<FocusSession>) -> Self {
        FocusStatus {
            session,
            modes: config
                .modes
                .iter()
                .map(|mode| FocusModeInfo {
                    name: mode.name.clone(),
                    detail: mode.describe(),
                })
                .collect(),
            default_mode: config.default.clone(),
            workday_ends: config.workday_ends.clone(),
        }
    }
}

/// What this process holds back while a focus runs. Kept in memory: a sync
/// announces only the mail it brought (see `docs/CONCURRENCY_DESIGN.md`), so
/// what a process held is its own to announce, and what was held when Orb
/// quit is, like everything that waits at launch, not announced.
#[derive(Debug, Default)]
pub struct FocusHeld {
    /// Row ids of the mail not announced.
    pub mail: Vec<i64>,
    /// Room and thread of the chat threads not announced.
    pub chat: Vec<(String, String)>,
    /// When what was held was last announced, in ms; 0 before the first tick
    /// of a focus this process has not seen yet.
    pub last_release_ms: i64,
    /// The start of the session the above belongs to.
    pub session_started_at: i64,
}

impl FocusHeld {
    /// Count from `session`'s start when it is not the one already followed.
    pub fn follow(&mut self, session: &FocusSession, now_ms: i64) {
        if self.session_started_at != session.started_at {
            self.session_started_at = session.started_at;
            self.last_release_ms = now_ms;
        }
    }

    /// Everything held, which is held no longer.
    pub fn take(&mut self, now_ms: i64) -> (Vec<i64>, Vec<(String, String)>) {
        self.last_release_ms = now_ms;
        (
            std::mem::take(&mut self.mail),
            std::mem::take(&mut self.chat),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> FocusConfig {
        serde_saphyr::from_str(yaml).expect("focus config")
    }

    fn session(ends_at: Option<i64>) -> FocusSession {
        FocusSession {
            mode: "deep".to_string(),
            started_at: 1_000,
            ends_at,
        }
    }

    #[test]
    fn a_config_without_modes_has_the_two_built_in() {
        let config = parse("workday_ends: '17:30'\n");
        assert_eq!(config.workday_ends.as_deref(), Some("17:30"));
        assert_eq!(config.default, "deep");
        let names: Vec<&str> = config.modes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["deep", "batched"]);
        assert!(config.warnings.is_empty());
        assert_eq!(
            config,
            FocusConfig {
                workday_ends: Some("17:30".to_string()),
                ..FocusConfig::default()
            }
        );
    }

    #[test]
    fn modes_keep_the_order_of_the_file_and_the_first_is_the_default() {
        let config = parse(
            "modes:\n  quiet: { notify: off, badge: off, chat: mentions }\n  \
             hourly: { notify: 1h, badge: on, chat: on }\n  bare:\n",
        );
        assert_eq!(config.default, "quiet");
        assert_eq!(
            config.modes,
            [
                FocusMode {
                    name: "quiet".into(),
                    notify: FocusNotify::Off,
                    badge: FocusBadge::Off,
                    chat: FocusChat::Mentions,
                },
                FocusMode {
                    name: "hourly".into(),
                    notify: FocusNotify::Every(3600),
                    badge: FocusBadge::On,
                    chat: FocusChat::On,
                },
                // A mode that says nothing holds everything but the exceptions.
                FocusMode {
                    name: "bare".into(),
                    notify: FocusNotify::Off,
                    badge: FocusBadge::Exceptions,
                    chat: FocusChat::Off,
                },
            ]
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn yaml_booleans_are_read_as_on_and_off() {
        let config = parse("modes:\n  m: { notify: true, badge: false, chat: true }\n");
        assert_eq!(config.modes[0].notify, FocusNotify::On);
        assert_eq!(config.modes[0].badge, FocusBadge::Off);
        assert_eq!(config.modes[0].chat, FocusChat::On);
    }

    #[test]
    fn what_cannot_be_given_is_named_and_falls_back_to_holding() {
        let config = parse(
            "workday_ends: late\ndefault: nowhere\nmodes:\n  off: {}\n  2h: {}\n  \
             m: { notify: sometimes, badge: red, chat: loud }\n",
        );
        assert_eq!(config.workday_ends, None);
        assert_eq!(config.default, "m");
        assert_eq!(
            config.modes,
            [FocusMode {
                name: "m".into(),
                notify: FocusNotify::Off,
                badge: FocusBadge::Exceptions,
                chat: FocusChat::Off,
            }]
        );
        for needle in [
            "workday_ends",
            "'off' cannot name a mode",
            "'2h' cannot name a mode",
            "focus.modes.m.notify",
            "focus.modes.m.badge",
            "focus.modes.m.chat",
            "no mode 'nowhere'",
        ] {
            assert!(
                config.warnings.iter().any(|w| w.contains(needle)),
                "no warning about {needle}: {:?}",
                config.warnings
            );
        }
    }

    #[test]
    fn an_interval_is_never_shorter_than_a_minute() {
        let config = parse("modes:\n  m: { notify: 0m }\n");
        assert_eq!(config.modes[0].notify, FocusNotify::Every(60));
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        assert!(serde_saphyr::from_str::<FocusConfig>("mode: deep\n").is_err());
    }

    #[test]
    fn a_focus_ends_at_its_time_and_not_before() {
        let config = FocusConfig::default();
        let deep = config.mode("deep");
        assert_eq!(
            due(&session(Some(5_000)), deep, 1_000, 4_999),
            FocusDue::Nothing
        );
        assert_eq!(
            due(&session(Some(5_000)), deep, 1_000, 5_000),
            FocusDue::End
        );
        // One the user ends never ends by the clock.
        assert_eq!(
            due(&session(None), deep, 1_000, i64::MAX),
            FocusDue::Nothing
        );
    }

    #[test]
    fn a_mode_with_an_interval_releases_when_it_comes_round() {
        let config = FocusConfig::default();
        let batched = config.mode("batched");
        let half_hour = 30 * 60 * 1000;
        assert_eq!(
            due(&session(None), batched, 1_000, 1_000 + half_hour - 1),
            FocusDue::Nothing
        );
        assert_eq!(
            due(&session(None), batched, 1_000, 1_000 + half_hour),
            FocusDue::Release
        );
        // The end comes before the interval.
        assert_eq!(
            due(&session(Some(2_000)), batched, 1_000, 1_000 + half_hour),
            FocusDue::End
        );
    }

    #[test]
    fn a_focus_whose_mode_left_the_config_ends() {
        assert_eq!(due(&session(None), None, 1_000, 1_001), FocusDue::End);
    }

    #[test]
    fn chat_is_admitted_as_the_mode_says() {
        let mode = |chat| FocusMode {
            chat,
            ..builtin_modes().remove(0)
        };
        assert!(mode(FocusChat::On).admits_chat(false));
        assert!(mode(FocusChat::Mentions).admits_chat(true));
        assert!(!mode(FocusChat::Mentions).admits_chat(false));
        assert!(!mode(FocusChat::Off).admits_chat(true));
    }

    #[test]
    fn what_is_held_counts_from_the_start_of_its_own_focus() {
        let mut held = FocusHeld::default();
        held.follow(&session(None), 7_000);
        assert_eq!(held.last_release_ms, 7_000);
        held.mail.push(1);
        // The same focus, looked at again: nothing starts over.
        held.follow(&session(None), 9_000);
        assert_eq!(held.last_release_ms, 7_000);
        assert_eq!(held.take(9_500), (vec![1], Vec::new()));
        assert_eq!(held.last_release_ms, 9_500);
        assert!(held.mail.is_empty());
    }

    #[test]
    fn a_mode_reads_as_a_line() {
        let modes = builtin_modes();
        assert_eq!(
            modes[0].describe(),
            "mail held · badge for exceptions · chat held"
        );
        assert_eq!(
            modes[1].describe(),
            "mail every 30m · badge off · chat for mentions"
        );
    }
}
