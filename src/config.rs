use secrecy::SecretString;
use serde::Deserialize;
use shell_words;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CONFIG_PATH: &str = "~/.config/orb/config.yml";

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub db_path: String,
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub bindings: BindingsConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
    /// Optional log file path for warnings and debug info
    pub log_file: Option<String>,
    /// Sidebar display mode (single_account or multi_account)
    #[serde(default)]
    pub sidebar_mode: SidebarMode,
    /// Virtual folder definitions for merged views
    #[serde(default)]
    pub virtual_folders: Vec<VirtualFolderConfig>,
    /// What the top of the sidebar holds, in this order: task views, folders,
    /// views, the reading list and chat's Waiting. Left out, Today alone.
    /// `pinned_folders` is its older name, from when only folders could be.
    #[serde(default, alias = "pinned_folders")]
    pub pinned: Option<Vec<PinnedEntry>>,
    /// `pinned` as checked against the rest of the config, with what it
    /// cannot give dropped and Today standing in for a list never written.
    #[serde(skip)]
    pub pins: Vec<Pin>,
    /// Subject munging rules (regex-based display transformations)
    #[serde(default)]
    pub subject_munges: Vec<SubjectMungeRule>,
    /// Addresses a message's subject points to, opened by `open-link`
    #[serde(default)]
    pub message_links: Vec<MessageLinkRule>,
    /// Contact and alias configuration for address autocompletion
    #[serde(default)]
    pub contacts: ContactsConfig,
    /// How a reply quotes what it is replying to.
    #[serde(default)]
    pub reply: ReplyConfig,
    /// What the compose window checks before a message is sent.
    #[serde(default)]
    pub compose: ComposeConfig,
    /// Senders whose HTML mail may load its remote images without asking.
    ///
    /// Each entry is a full address (`alice@example.com`) or a domain
    /// (`example.com`, also written `@example.com`), which stands for every
    /// address at it and at its subdomains. Everyone else's remote images,
    /// stylesheets and fonts stay blocked until asked for per message.
    #[serde(default)]
    pub load_remote_images_from: Vec<String>,
    /// Messages whose HTML may load its remote images without asking, by the
    /// highlight rules' conditions: for what a sender alone cannot say.
    #[serde(default)]
    pub load_remote_images: crate::highlights::RemoteImageRules,
    /// Which rendering a message opens in, when it has both.
    ///
    /// The toggle in the reader still switches between them; this is where it
    /// starts. A message with only one rendering shows that one regardless.
    #[serde(default)]
    pub default_message_view: MessageView,
    /// Directory holding the embedding model, for search by meaning.
    ///
    /// Normally unset: the packaged app points at the copy inside its own
    /// bundle. Set it to use a model from elsewhere, which is also how the
    /// tests and any non-bundled build find one. Unset and unbundled means
    /// semantic search is off; exact search does not depend on it.
    pub embedding_model: Option<String>,
    /// How long a message must stay on screen before it counts as read.
    ///
    /// The reader follows the cursor, so marking on display alone would clear
    /// unread mail merely passed over on the way somewhere else. Waiting for
    /// the message to be dwelt on distinguishes reading it from walking past.
    ///
    /// Zero never marks automatically; `mark-read` still does.
    #[serde(default = "default_mark_read_after_seconds")]
    pub mark_read_after_seconds: f64,
    /// Where tasks are stored. Absent means task management is off.
    pub tasks: Option<TasksConfig>,
    /// The Rocket.Chat server chat is read from and written to. Absent means
    /// chat is off.
    pub chat: Option<ChatConfig>,
    /// Where the reading list is kept. Absent means there is none.
    pub reading: Option<ReadingConfig>,
    /// Rules run on mail as it arrives in a synced folder.
    #[serde(default)]
    pub filters: crate::filters::FilterSet,
    /// Rules that change how a message looks, evaluated as it is drawn.
    #[serde(default)]
    pub highlights: crate::highlights::HighlightSet,
    /// How the message list is laid out.
    #[serde(default)]
    pub list: ListConfig,
    /// How search results are shown.
    #[serde(default)]
    pub search: SearchConfig,
    /// How the windows look.
    #[serde(default)]
    pub appearance: AppearanceConfig,
    /// What the Dock icon counts, one badge each, at most four. Empty: one
    /// badge for the unread mail in every inbox.
    #[serde(default)]
    pub dock_badges: Vec<DockBadge>,
    /// Where Ctrl+H/J/K/L go once Orb has nothing left to move to that way.
    #[serde(default)]
    pub navigation: NavigationConfig,
    /// What the config asked for that cannot be given, found while reading
    /// it; shown at launch beside the other warnings.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

/// One count on the Dock icon: the unread mail in a folder, of one account or
/// of every account.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DockBadge {
    /// The account counted; every account when absent.
    pub account: Option<String>,
    /// The folder counted (default INBOX).
    #[serde(default = "default_dock_folder")]
    pub folder: String,
    /// The badge's colour as `#rrggbb`; the system's red when absent.
    pub color: Option<String>,
}

fn default_dock_folder() -> String {
    "INBOX".to_string()
}

/// The most badges the Dock icon has corners for.
pub const MAX_DOCK_BADGES: usize = 4;

/// How the windows look.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppearanceConfig {
    /// `system` (default): follow the system appearance; `dark` or `light`
    /// to fix it.
    #[serde(default)]
    pub theme: Theme,
    /// The text size in pixels (default 13); ⌘+ and ⌘- change it for the
    /// machine, and ⌘0 comes back to this.
    #[serde(default = "default_font_size")]
    pub font_size: f64,
    /// The text size of Neovim in the compose window, in pixels; unset
    /// follows `font_size`. ⌘+ and ⌘- scale it along with the rest.
    #[serde(default)]
    pub editor_font_size: Option<f64>,
    /// Draw HTML mail dark when the window is dark (default true); the
    /// reader has a toggle for the message on screen.
    #[serde(default = "default_true")]
    pub dark_html_mail: bool,
    /// `hybrid` (default): SF Pro for text and SF Mono for columns;
    /// `terminal`: monospace throughout, denser rows, a modeline.
    #[serde(default)]
    pub look: Look,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            font_size: default_font_size(),
            editor_font_size: None,
            dark_html_mail: true,
            look: Look::Hybrid,
        }
    }
}

fn default_font_size() -> f64 {
    13.0
}

/// Which appearance the windows take.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

/// Which of the two designs the windows are drawn in.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    #[default]
    Hybrid,
    Terminal,
}

impl Look {
    pub fn as_str(self) -> &'static str {
        match self {
            Look::Hybrid => "hybrid",
            Look::Terminal => "terminal",
        }
    }
}

/// How the message list is laid out.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListConfig {
    /// The columns, in the order the list has them: `meta`, `tasks`, `from`,
    /// `to`, `subject`, `date`. Default: all but `to`. A folder's own
    /// `columns` overrides it — `to` for Sent.
    pub columns: Option<Vec<String>>,
    /// The date column's format: `auto`, which shows each date as fully as
    /// the column has room for and the year whenever it is not this one, or
    /// strftime-style: `%Y-%m-%d %H:%M`, `%b %e`, `%-d.%-m. %H:%M`. Default:
    /// `theme.date_format`, which is `auto`.
    pub date_format: Option<String>,
    /// How tall a row is: `compact`, `comfortable` (default) or `spacious`.
    #[serde(default)]
    pub density: Density,
}

/// How search results are shown.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    /// The order the mail of a search comes in until another is picked:
    /// `relevance` (default), or one of a folder's `sort` orders. A search
    /// by meaning is always closest first.
    #[serde(default)]
    pub sort: SearchSort,
}

/// The order of a search's mail: by relevance, or as a folder sorts.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    #[default]
    Relevance,
    DateNewest,
    DateOldest,
    SubjectAsc,
    SubjectDesc,
    SenderAsc,
    SenderDesc,
}

impl SearchSort {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchSort::Relevance => "relevance",
            SearchSort::DateNewest => "date_newest",
            SearchSort::DateOldest => "date_oldest",
            SearchSort::SubjectAsc => "subject_asc",
            SearchSort::SubjectDesc => "subject_desc",
            SearchSort::SenderAsc => "sender_asc",
            SearchSort::SenderDesc => "sender_desc",
        }
    }
}

/// How tall a row of the list is.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
    Spacious,
}

impl Density {
    pub fn as_str(self) -> &'static str {
        match self {
            Density::Compact => "compact",
            Density::Comfortable => "comfortable",
            Density::Spacious => "spacious",
        }
    }
}

/// The columns the list can have, in their order.
pub const LIST_COLUMNS: &[&str] = &["meta", "tasks", "from", "to", "subject", "date"];

/// The columns a list shows when the config says nothing.
pub const DEFAULT_LIST_COLUMNS: &[&str] = &["meta", "tasks", "from", "subject", "date"];

impl Config {
    /// The date format of the list: `list.date_format`, else the theme's.
    pub fn list_date_format(&self) -> &str {
        self.list
            .date_format
            .as_deref()
            .unwrap_or(&self.theme.date_format)
    }

    /// The columns of the list, checked and in their order.
    pub fn list_columns(&self) -> Vec<String> {
        ordered_columns(self.list.columns.as_deref())
    }
}

/// `requested` in the list's own order, with unknown names dropped; the
/// default set when nothing was requested.
pub fn ordered_columns(requested: Option<&[String]>) -> Vec<String> {
    match requested {
        Some(names) => LIST_COLUMNS
            .iter()
            .filter(|column| names.iter().any(|n| n.eq_ignore_ascii_case(column)))
            .map(|column| column.to_string())
            .collect(),
        None => DEFAULT_LIST_COLUMNS
            .iter()
            .map(|column| column.to_string())
            .collect(),
    }
}

/// Long enough not to trigger while moving through a list, short enough that
/// anything actually being read is caught.
/// The bare, lower-cased address in a `From` header value: `alice@example.com`
/// out of `Alice <alice@example.com>`, or the value itself when it has no
/// angle brackets.
pub fn sender_address(from: &str) -> Option<String> {
    let bare = match from.rsplit_once('<') {
        Some((_, rest)) => rest.split('>').next().unwrap_or(""),
        None => from,
    };
    let bare = bare.trim().to_ascii_lowercase();
    if bare.contains('@') { Some(bare) } else { None }
}

fn default_mark_read_after_seconds() -> f64 {
    2.0
}

/// The designated tasks account and folder.
///
/// ```yaml
/// tasks:
///   account: Work
///   folder: Tasks
/// ```
///
/// There is exactly one tasks account, and therefore exactly one task list and
/// one global ordering. The account stays an ordinary mail account: only the
/// one folder is taken out of the mail world, by [`Config::hide_tasks_folder`].
///
/// `folder` is the server-side name and is used **verbatim**. No prefix is
/// assumed and in particular no leading `.` — that is a Courier/Maildir
/// convention, not a universal one, and prepending it on a Dovecot server
/// creates a folder literally called `.Tasks` next to the real one. Where the
/// server namespaces its mailboxes under `INBOX`, the name is resolved against
/// the namespace at startup (see `tasks::sync::plan_tasks_folder`) rather than
/// guessed here.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TasksConfig {
    /// The entry in `accounts` whose IMAP folder holds the tasks.
    pub account: String,
    /// The folder's server-side name, taken verbatim.
    #[serde(default = "default_tasks_folder")]
    pub folder: String,
    /// The language the date picker reads (§12.1): `"en"` or `"de"`.
    ///
    /// A setting rather than a guess from `LANG`. The parser's whole contract is
    /// that a given string always means the same day, and an environment
    /// variable that differs between a terminal launch and a Finder launch would
    /// make `12/9` mean September on one and December on the other — which is
    /// the "silently wrong guess" §12.1 rules out. Unrecognised values fall back
    /// to English with a warning rather than failing the whole config.
    #[serde(default = "default_tasks_locale")]
    pub locale: String,
    /// Whether Today keeps the tasks completed today, struck through, until
    /// midnight (default true; §10.2).
    #[serde(default = "default_true")]
    pub keep_completed_in_today: bool,
    /// Whether Today lists its completed tasks above the open ones rather
    /// than in their place in the order (default false; §24).
    #[serde(default)]
    pub completed_first_in_today: bool,
    /// Days after which a completed task leaves Completed for the Archive
    /// (default 30; 0 never archives; §18).
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u32,
    /// Snapshots of every task, written on a schedule (todo-spec §20).
    /// Absent: no export.
    #[serde(default)]
    pub export: Option<TaskExportConfig>,
}

/// The `chat:` section: one Rocket.Chat server.
///
/// ```yaml
/// chat:
///   server: https://chat.example.com
///   rooms:
///     general: { waiting: false }
/// ```
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChatConfig {
    /// The server's address, `https://` and the host, without a path.
    pub server: String,
    /// How Orb gets a session on the server.
    #[serde(default)]
    pub sign_in: ChatSignIn,
    /// With `sign_in: token_command`: a command that prints the user id and
    /// a personal access token, separated by whitespace.
    pub token_command: Option<String>,
    /// How far back a thread can start waiting the first time Orb sees the
    /// server, in days (default 14), so a busy server does not start with
    /// every thread it ever had.
    #[serde(default = "default_chat_waiting_days")]
    pub waiting_days: u32,
    /// Settings per room, by the room's name as Rocket.Chat shows it in its
    /// address (`general`, not `#general`).
    #[serde(default)]
    pub rooms: HashMap<String, ChatRoomConfig>,
    /// The reactions offered on a message, as Rocket.Chat's shortcodes, in
    /// the order offered.
    #[serde(default = "default_chat_reactions")]
    pub reactions: Vec<String>,
    /// Text in a message that becomes a link, such as an issue number.
    #[serde(default)]
    pub links: Vec<ChatLinkConfig>,
    /// Whether the Dock badge counts the threads in Waiting besides the mail
    /// (default false).
    #[serde(default)]
    pub badge: bool,
}

fn default_chat_reactions() -> Vec<String> {
    [":+1:", ":heart:", ":joy:", ":tada:", ":eyes:"]
        .map(String::from)
        .to_vec()
}

/// One kind of text that becomes a link in a chat message.
///
/// ```yaml
/// links:
///   - { pattern: '#(\d+)', url: 'https://tracker.example.com/issues/$1' }
/// ```
#[derive(Deserialize, serde::Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChatLinkConfig {
    /// A JavaScript regular expression, matched against the text as shown.
    pub pattern: String,
    /// The address a match opens: `$0` is the whole match, `$1` to `$9` its
    /// groups.
    pub url: String,
}

/// How Orb signs in to the chat server.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatSignIn {
    /// The server's own sign-in page, in a window of Orb's; the session it
    /// hands the web client is kept in the Keychain.
    #[default]
    Sso,
    /// The user id and token printed by `token_command`.
    TokenCommand,
}

/// What one room does in Orb.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChatRoomConfig {
    /// Whether the room's threads can wait (default true).
    #[serde(default = "default_true")]
    pub waiting: bool,
    /// Whether a thread starting to wait in the room notifies (default true).
    #[serde(default = "default_true")]
    pub notify: bool,
}

impl Default for ChatRoomConfig {
    fn default() -> Self {
        Self {
            waiting: true,
            notify: true,
        }
    }
}

fn default_chat_waiting_days() -> u32 {
    14
}

impl ChatConfig {
    /// The settings of the room called `name`; defaults for a room the config
    /// does not mention.
    pub fn room(&self, name: &str) -> ChatRoomConfig {
        self.rooms.get(name).copied().unwrap_or_default()
    }

    /// The user id and token `token_command` prints.
    pub fn run_token_command(&self) -> std::result::Result<(String, SecretString), String> {
        let command = self
            .token_command
            .as_deref()
            .ok_or("chat.token_command is not set")?;
        let words = shell_words::split(command).map_err(|e| e.to_string())?;
        let (program, args) = words.split_first().ok_or("chat.token_command is empty")?;
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| format!("chat.token_command could not run: {e}"))?;
        if !output.status.success() {
            return Err(format!("chat.token_command failed: {}", output.status));
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| "chat.token_command printed something other than text")?;
        let mut words = text.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(user), Some(token), None) => {
                Ok((user.to_string(), SecretString::from(token.to_string())))
            }
            _ => Err("chat.token_command must print a user id and a token".to_string()),
        }
    }
}

/// Where and how often the tasks are exported (todo-spec §20.3).
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskExportConfig {
    /// The directory the snapshots go to; `~` is expanded.
    pub directory: String,
    /// How old the newest complete snapshot may get (default 24).
    #[serde(default = "default_export_interval")]
    pub interval_hours: f64,
    /// Complete snapshots of this machine kept (default 30, at least 1).
    #[serde(default = "default_export_keep")]
    pub keep: usize,
}

fn default_export_interval() -> f64 {
    24.0
}

fn default_export_keep() -> usize {
    30
}

fn default_archive_after_days() -> u32 {
    30
}

fn default_tasks_folder() -> String {
    "Tasks".to_string()
}

/// The reading list: newsletters and feed items set aside to read later.
///
/// ```yaml
/// reading:
///   account: Private
///   folder: Reading
///   archive_folder: Reading Archive
///   feeds:
///     - url: https://example.com/feed.xml
/// ```
///
/// Two ordinary folders of one account, synced and searched like any other
/// but shown under Reading rather than among the account's folders. An item's
/// state is plain IMAP, so every client sees it: new is unread, must read is
/// `\Flagged`, archived is in the archive folder. The archive is a sibling
/// rather than a subfolder so that no hierarchy delimiter is assumed; both
/// names are resolved against the server's namespace as the tasks folder is.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReadingConfig {
    /// The entry in `accounts` that holds the reading folders.
    pub account: String,
    /// The folder of items to read, by its server-side name.
    #[serde(default = "default_reading_folder")]
    pub folder: String,
    /// The folder of items read or put aside.
    #[serde(default = "default_reading_archive_folder")]
    pub archive_folder: String,
    /// RSS, Atom and JSON feeds whose items are brought into the reading folder.
    #[serde(default)]
    pub feeds: Vec<FeedConfig>,
    /// How often each feed is asked for new items (default 60).
    #[serde(default = "default_poll_minutes")]
    pub poll_minutes: u32,
}

/// One subscription of the reading list.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// The feed's address, `http` or `https`.
    pub url: String,
    /// What its items are sent by; the feed's own title when absent.
    pub name: Option<String>,
}

fn default_poll_minutes() -> u32 {
    60
}

fn default_reading_folder() -> String {
    "Reading".to_string()
}

fn default_reading_archive_folder() -> String {
    "Reading Archive".to_string()
}

/// What a reading folder is for, as the sidebar's role spells it.
pub const READING_ROLE: &str = "reading";
pub const READING_ARCHIVE_ROLE: &str = "reading-archive";

/// Whether `folder`, as the server names it, is the folder configured as
/// `configured`: the same name in any case, or that name as the leaf of a
/// namespace-qualified one (`INBOX.Tasks`, `INBOX/Tasks`).
fn names_folder(configured: &str, folder: &str) -> bool {
    if folder.eq_ignore_ascii_case(configured) {
        return true;
    }
    // Only `.` and `/` are in use as hierarchy delimiters by the servers
    // this runs against; a leaf match on anything else would be guessing.
    folder
        .rsplit_once(['.', '/'])
        .is_some_and(|(_, leaf)| leaf.eq_ignore_ascii_case(configured))
}

fn default_tasks_locale() -> String {
    "en".to_string()
}

impl TasksConfig {
    /// Whether `folder` names this account's tasks folder.
    ///
    /// The exact name matches, and so does a namespace-qualified form of it
    /// (`INBOX.Tasks`, `INBOX/Tasks`) — the configured name is a leaf and the
    /// server may only ever mention the qualified one. The asymmetry is
    /// deliberate: a false positive hides one mail folder that the user has
    /// already declared to be the tasks folder, while a false negative lets
    /// task messages into the sidebar, the unread badges, the search index and
    /// the new-mail notifications, where they are indistinguishable from mail
    /// nobody sent.
    pub fn matches_folder(&self, folder: &str) -> bool {
        names_folder(&self.folder, folder)
    }

    /// The date picker's language.
    ///
    /// Accepts a bare tag (`de`) and a full one (`de-CH`, `de_DE.UTF-8`), so a
    /// value copied out of a locale list works. Anything else is English: a
    /// picker that refuses to open because the locale is misspelled is worse
    /// than one that offers English month names.
    pub fn date_locale(&self) -> crate::tasks::DateLocale {
        let tag = self
            .locale
            .split(['-', '_', '.'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match tag.as_str() {
            "de" => crate::tasks::DateLocale::German,
            "en" | "" => crate::tasks::DateLocale::English,
            other => {
                crate::logger::warn(&format!(
                    "[tasks] locale '{other}' is not supported (expected 'en' or 'de'); \
                     the date picker will use English"
                ));
                crate::tasks::DateLocale::English
            }
        }
    }
}

impl Config {
    /// Whether a message from `from` (a header value such as
    /// `Alice <alice@example.com>`) may load its remote images unasked, per
    /// `load_remote_images_from`.
    pub fn allows_remote_images_from(&self, from: &str) -> bool {
        let Some(address) = sender_address(from) else {
            return false;
        };
        let Some((_, domain)) = address.rsplit_once('@') else {
            return false;
        };
        self.load_remote_images_from.iter().any(|entry| {
            let entry = entry.trim().trim_start_matches('@').to_ascii_lowercase();
            if entry.is_empty() {
                false
            } else if entry.contains('@') {
                address == entry
            } else {
                domain == entry || domain.ends_with(&format!(".{entry}"))
            }
        })
    }

    /// The headers the highlight and remote-image rules name with `header:`,
    /// which the sync fetches beside the envelope.
    pub fn extra_header_names(&self) -> Vec<String> {
        let mut names = self.highlights.extra_header_names();
        names.extend(self.load_remote_images.0.extra_header_names());
        names.extend(
            self.virtual_folders
                .iter()
                .flat_map(VirtualFolderConfig::header_names),
        );
        // A feed item's article, which `open-link` opens.
        if self.reading.is_some() {
            names.push(crate::feeds::LINK_HEADER.to_string());
        }
        names.sort();
        names.dedup();
        names
    }

    /// The tasks folder for `account`, if that account is the designated one.
    pub fn tasks_folder_for(&self, account: &str) -> Option<&str> {
        self.tasks
            .as_ref()
            .filter(|tasks| tasks.account == account)
            .map(|tasks| tasks.folder.as_str())
    }

    /// Whether this account/folder pair is the tasks folder.
    ///
    /// The predicate every place that enumerates mail folders has to consult:
    /// the tasks folder is not synced as mail, not indexed, not shown in the
    /// sidebar, not a move target, not counted in a badge and never a source of
    /// a new-mail notification.
    pub fn is_tasks_folder(&self, account: &str, folder: &str) -> bool {
        self.tasks
            .as_ref()
            .is_some_and(|tasks| tasks.account == account && tasks.matches_folder(folder))
    }

    /// What this account/folder pair is to the reading list, if anything:
    /// [`READING_ROLE`] or [`READING_ARCHIVE_ROLE`].
    pub fn reading_role(&self, account: &str, folder: &str) -> Option<&'static str> {
        let reading = self.reading.as_ref().filter(|r| r.account == account)?;
        if names_folder(&reading.folder, folder) {
            Some(READING_ROLE)
        } else if names_folder(&reading.archive_folder, folder) {
            Some(READING_ARCHIVE_ROLE)
        } else {
            None
        }
    }

    /// Take a folder out of the mail world for good.
    ///
    /// Implemented as a per-folder config entry rather than as a new field
    /// because `visible: false, sync: false` is exactly what that map already
    /// means, and every enumeration in the app already asks it: the sidebar via
    /// [`AccountConfig::is_folder_visible`] and the sync via
    /// [`AccountConfig::should_sync_folder`]. One entry therefore keeps task
    /// messages out of the folder list, out of mail sync — and so out of the
    /// message FTS/embedding index, the unread badges, the new-mail
    /// notifications and All Inboxes, none of which can see a folder that was
    /// never synced.
    ///
    /// It overrides an explicit entry for the folder under the account's
    /// `folders` on purpose: a folder cannot be both the task store and
    /// ordinary mail.
    ///
    /// Called once for the configured name when the config is read, and again
    /// by the sync layer for the *resolved* name once the server has been asked
    /// what the folder is actually called.
    pub fn hide_tasks_folder(&mut self, account: &str, folder: &str) {
        for config in self.accounts.iter_mut().filter(|a| a.name == account) {
            config.folders.insert(
                folder.to_string(),
                FolderConfig {
                    visible: false,
                    sync: false,
                    show_total: false,
                    show_unread: false,
                    display_name: None,
                    sort: None,
                    watch: false,
                    threads: None,
                    columns: None,
                },
            );
        }
    }
}

/// Where Ctrl+H/J/K/L go once Orb has nothing left to move to that way.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct NavigationConfig {
    /// Run with `left`, `right`, `up` or `down` as its one argument, typically
    /// to focus the window of another application that lies that way. A path,
    /// not a shell command line; `~` is expanded.
    pub escalate_command: Option<String>,
}

/// What the compose window checks before a message is sent.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComposeConfig {
    /// Words that mention an attachment. A message whose own text, without
    /// the quote and the signature, has one but no attachment is warned about
    /// before sending. Matched case-insensitively at the start of a word, so
    /// `attach` also finds `attached`; empty turns the check off.
    #[serde(default = "default_attachment_words")]
    pub attachment_words: Vec<String>,
    /// The folder mail templates are read from; `templates` beside the
    /// config file when unset. `~` is expanded.
    #[serde(default)]
    pub templates_dir: Option<String>,
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            attachment_words: default_attachment_words(),
            templates_dir: None,
        }
    }
}

fn default_attachment_words() -> Vec<String> {
    [
        "attach",
        "enclos",
        "anbei",
        "anhang",
        "anhäng",
        "angehängt",
        "beigefügt",
        "beiliegend",
        "ci-joint",
        "pièce jointe",
        "pièces jointes",
        "allegat",
    ]
    .map(String::from)
    .to_vec()
}

/// How a reply quotes the message it answers.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReplyConfig {
    /// Whether to include the original at all.
    #[serde(default = "default_true")]
    pub quote: bool,
    /// What each quoted line begins with.
    #[serde(default = "default_quote_prefix")]
    pub prefix: String,
    /// The line introducing the quote.
    ///
    /// `{sender}` and `{date}` are replaced. Empty means no attribution line,
    /// for anyone who prefers the quote to stand on its own.
    #[serde(default = "default_attribution")]
    pub attribution: String,
    /// Whether the signature follows the quote or precedes it.
    #[serde(default)]
    pub signature: SignaturePlacement,
}

impl Default for ReplyConfig {
    fn default() -> Self {
        Self {
            quote: true,
            prefix: default_quote_prefix(),
            attribution: default_attribution(),
            signature: SignaturePlacement::default(),
        }
    }
}

fn default_quote_prefix() -> String {
    "> ".to_string()
}

fn default_attribution() -> String {
    "On {date}, {sender} wrote:".to_string()
}

/// Where a reply's signature goes.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePlacement {
    /// After the quote, at the end of the message.
    #[default]
    Below,
    /// Between what is written and the quote, so it ends what was written.
    Above,
}

/// An address an account sends as.
///
/// Changes what the recipient sees — `From`, `Reply-To` and the signature —
/// and nothing else: the message is filed in the account's Sent folder, and
/// goes out over the account's SMTP login unless the identity names a server
/// of its own (see [`AccountConfig::sending_as`]).
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    /// What the compose window shows. Unique within the account, and not the
    /// account's own name, which stands for the default identity.
    pub name: String,
    pub from: String,
    /// The signature as a file, like the account's.
    pub signature_file: Option<String>,
    /// The signature inline, instead of a file.
    pub signature: Option<String>,
    pub reply_to: Option<String>,
    /// The addresses that select this identity for a reply or forward when
    /// the original was sent to one of them. The address in `from` always
    /// does.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// The recipient domains mail is written to as this identity. Writing
    /// to one of them as another identity is warned about before sending.
    #[serde(default)]
    pub domains: Vec<String>,
    /// An SMTP server of the identity's own, for an address whose mail
    /// arrives in this account but must be sent elsewhere. The other `smtp_*`
    /// keys only count alongside it, and default as the account's do — except
    /// the username, which is required, and the password, which falls back to
    /// the Keychain rather than to the account's.
    pub smtp_server: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_encryption: Option<Encryption>,
    pub smtp_auth: Option<AuthMethod>,
    pub smtp_username: Option<String>,
    pub smtp_password_command: Option<String>,
}

impl IdentityConfig {
    /// The signature's text, from the file or inline. `None` when there is
    /// none or the file cannot be read.
    pub fn get_signature(&self) -> Option<String> {
        if let Some(text) = &self.signature {
            return Some(text.clone());
        }
        let path = self.signature_file.as_ref()?;
        match fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("Warning: Failed to read signature file '{}': {}", path, e);
                None
            }
        }
    }

    /// Whether mail sent to `address` was sent to this identity.
    pub fn owns(&self, address: &str) -> bool {
        let address = bare_address(address);
        std::iter::once(self.from.as_str())
            .chain(self.addresses.iter().map(String::as_str))
            .any(|own| bare_address(own).eq_ignore_ascii_case(&address))
    }

    /// Whether mail to `address` is written as this identity by its
    /// `domains`: one of them, or a subdomain of one.
    pub fn writes_to(&self, address: &str) -> bool {
        let address = bare_address(address).to_ascii_lowercase();
        let Some((_, domain)) = address.rsplit_once('@') else {
            return false;
        };
        self.domains.iter().any(|entry| {
            let entry = entry.trim().trim_start_matches('@').to_ascii_lowercase();
            !entry.is_empty() && (domain == entry || domain.ends_with(&format!(".{entry}")))
        })
    }
}

/// The address of a `Name <address>` entry, or the entry itself.
pub fn bare_address(entry: &str) -> String {
    entry
        .rsplit_once('<')
        .map(|(_, rest)| rest.trim_end_matches('>').trim().to_string())
        .unwrap_or_else(|| entry.trim().to_string())
}

/// How a message is rendered when it offers more than one form.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageView {
    /// As the sender laid it out. What most mail is written as.
    #[default]
    Html,
    /// The plain-text alternative: no images, no layout, no remote content.
    Plain,
}

impl MessageView {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageView::Html => "html",
            MessageView::Plain => "text",
        }
    }
}

/// Sort order for messages (configurable per folder or account)
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    DateNewest,
    DateOldest,
    SubjectAsc,
    SubjectDesc,
    SenderAsc,
    SenderDesc,
}

// =============================================================================
// Multi-Account Configuration
// =============================================================================

/// Sidebar display mode
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SidebarMode {
    /// Show only the current account's folders
    SingleAccount,
    /// Show all accounts' folders in a tree structure
    #[default]
    MultiAccount,
}

/// Configuration for a virtual folder that aggregates messages from multiple sources
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct VirtualFolderConfig {
    /// Display name for the virtual folder
    pub name: String,
    /// A Font Awesome icon name for the sidebar, `fa-inbox`; the search glass
    /// otherwise.
    pub icon: Option<String>,
    /// Source type for the virtual folder
    pub source: VirtualFolderSource,
    /// For custom sources: list of (account, folder) pairs
    #[serde(default)]
    pub folders: Vec<FolderRef>,
    /// For a `search` source: the query, in the search language.
    pub query: Option<String>,
}

impl VirtualFolderConfig {
    /// The view as a query in the search language, which is how every kind
    /// is answered: `all_inboxes` is every account's INBOX, `all_unread` is
    /// `is:unread`, `custom` is the folders joined with `OR`, and `search`
    /// is the query as written.
    pub fn query(&self) -> String {
        match self.source {
            VirtualFolderSource::AllInboxes => "in:INBOX".to_string(),
            VirtualFolderSource::AllUnread => "is:unread".to_string(),
            VirtualFolderSource::Custom => self
                .folders
                .iter()
                .map(|f| {
                    format!(
                        "acct:{} in:{}",
                        quote_term(&f.account),
                        quote_term(&f.folder)
                    )
                })
                .collect::<Vec<_>>()
                .join(" OR "),
            VirtualFolderSource::Search => self.query.clone().unwrap_or_default(),
        }
    }

    /// The headers the query asks about with `header:`, upper-cased as the
    /// sync names them. `List-Id` is fetched for every message anyway.
    pub fn header_names(&self) -> Vec<String> {
        let Ok(query) = crate::search::parse(&self.query()) else {
            return Vec::new();
        };
        query
            .all_clauses()
            .filter_map(|clause| match &clause.term {
                crate::search::Term::Header { name, .. } if name != "list-id" => {
                    Some(name.to_ascii_uppercase())
                }
                _ => None,
            })
            .collect()
    }
}

/// A value for the search language, quoted when it has a space in it.
fn quote_term(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("'{}'", value.replace('\'', ""))
    } else {
        value.to_string()
    }
}

/// Source type for virtual folders
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VirtualFolderSource {
    /// Collect INBOX folders from all accounts
    AllInboxes,
    /// Collect all unread messages from all accounts
    AllUnread,
    /// Custom selection of specific folders
    Custom,
    /// A saved search: whatever `query` finds.
    Search,
}

/// Reference to a specific folder in a specific account
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct FolderRef {
    /// Account name
    pub account: String,
    /// Folder name within the account
    pub folder: String,
}

/// One entry of `pinned`, as written: a built-in view by its name, a folder,
/// or one of the `virtual_folders` by its name.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum PinnedEntry {
    Name(String),
    Folder(PinnedFolder),
    View(PinnedView),
}

/// `{ view: All Inboxes }`: a view of `virtual_folders`, pinned.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PinnedView {
    pub view: String,
}

/// Something at the top of the sidebar, once the config has been read.
#[derive(Debug, Clone, PartialEq)]
pub enum Pin {
    /// A task view: `today`, `upcoming`, `someday` or `completed`.
    Tasks(String),
    /// A reading view: `list` (the reading list) or `must-read`.
    Reading(String),
    /// Chat's Waiting.
    Waiting,
    /// A view of `virtual_folders`, by its name.
    View(String),
    Folder(PinnedFolder),
}

/// The names a built-in view is pinned by, and what each one pins.
const PIN_NAMES: &[(&str, &str)] = &[
    ("today", "tasks"),
    ("upcoming", "tasks"),
    ("someday", "tasks"),
    ("completed", "tasks"),
    ("reading", "reading"),
    ("must-read", "reading"),
    ("waiting", "chat"),
];

/// Configuration for a pinned folder that appears at the top of the folder list
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PinnedFolder {
    /// Account name containing the folder
    pub account: String,
    /// Folder name within the account
    pub folder: String,
    /// Optional custom display name (defaults to "Account: Folder" format)
    pub display_name: Option<String>,
}

/// A web address a message points to: `pattern` is matched against the
/// original subject or the body, and `url` may name its capture groups as
/// `$1`, `$2`.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MessageLinkRule {
    /// What `open-link <name>` picks the rule by.
    #[serde(default)]
    pub name: Option<String>,
    pub pattern: String,
    /// The whole match, `$0`, when left out, for a pattern that matches the
    /// address itself.
    #[serde(default = "whole_match")]
    pub url: String,
    /// What `pattern` is matched against.
    #[serde(default, rename = "match")]
    pub target: MessageLinkTarget,
    /// An address glob the sender must match, as in `filters`.
    #[serde(default)]
    pub from: Option<String>,
}

/// The part of a message a `message_links` rule reads.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageLinkTarget {
    /// The subject as it arrived.
    #[default]
    Subject,
    /// The plain-text body, or the HTML one when there is no plain text.
    Body,
}

/// Configuration for a subject munging rule
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct SubjectMungeRule {
    /// Regex pattern to match against subject
    pub pattern: String,
    /// Replacement string (supports $1, $2, $3 for capture groups); the whole
    /// match, `$0`, when left out, so a rule can add an icon and nothing else.
    #[serde(default = "whole_match")]
    pub replacement: String,
    /// A Font Awesome icon drawn before the subject of every message the
    /// pattern matches: `fa-gears`, `gears` or Nerd Fonts' `nf-fa-gears`.
    #[serde(default)]
    pub icon: Option<String>,
    /// The icon's colour, in any form a highlight rule takes.
    #[serde(default)]
    pub icon_color: Option<String>,
}

fn whole_match() -> String {
    "$0".to_string()
}

/// The same defaults as the config: a rule with no replacement keeps what it
/// matched.
impl Default for SubjectMungeRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            replacement: whole_match(),
            icon: None,
            icon_color: None,
        }
    }
}

// =============================================================================
// Contact/Address Book Configuration
// =============================================================================

/// Contact and alias configuration for address autocompletion
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ContactsConfig {
    /// Whether to auto-extract contacts from sent/received emails (default: true)
    #[serde(default = "default_true")]
    pub auto_extract: bool,
    /// Individual contact aliases for quick address entry
    #[serde(default)]
    pub aliases: Vec<ContactAlias>,
    /// Group aliases that expand to multiple addresses
    #[serde(default)]
    pub groups: Vec<ContactGroup>,
}

/// Individual contact alias for quick address entry
///
/// Example config:
/// ```yaml
/// contacts:
///   aliases:
///     - shortcut: rf
///       email: roger.federer@example.com
///       name: Roger Federer
/// ```
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ContactAlias {
    /// Short name/trigger (e.g., "rf")
    pub shortcut: String,
    /// Full email address
    pub email: String,
    /// Optional display name
    pub name: Option<String>,
}

/// Group alias that expands to multiple addresses
///
/// Example config:
/// ```yaml
/// contacts:
///   groups:
///     - shortcut: team
///       emails: [alice@company.com, bob@company.com]
///       description: Development Team
/// ```
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ContactGroup {
    /// Short name/trigger (e.g., "team")
    pub shortcut: String,
    /// List of email addresses
    pub emails: Vec<String>,
    /// Optional group description
    pub description: Option<String>,
}

/// Per-folder configuration
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FolderConfig {
    /// Whether folder appears in GUI (default: true)
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Whether folder is synced from IMAP (default: true)
    #[serde(default = "default_true")]
    pub sync: bool,
    /// Display total message count badge (default: false)
    #[serde(default)]
    pub show_total: bool,
    /// Display unread message count badge (default: true)
    #[serde(default = "default_true")]
    pub show_unread: bool,
    /// Optional override name for GUI display
    pub display_name: Option<String>,
    /// Default sort order for this folder (inherits from account default if None)
    pub sort: Option<SortOrder>,
    /// Watch this folder with IMAP IDLE, so changes to it arrive as the
    /// server makes them (default: false). INBOX is always watched; the
    /// server's connection limit bounds how many others can be.
    #[serde(default)]
    pub watch: bool,
    /// Open this folder as conversations (default: the account's `threads`).
    pub threads: Option<bool>,
    /// The list's columns in this folder (default: `list.columns`), `to`
    /// for a Sent folder.
    pub columns: Option<Vec<String>>,
}

impl Default for FolderConfig {
    fn default() -> Self {
        Self {
            visible: true,
            sync: true,
            show_total: false,
            show_unread: true,
            display_name: None,
            sort: None,
            watch: false,
            threads: None,
            columns: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_drafts() -> String {
    "Drafts".to_string()
}

fn default_sent() -> String {
    "Sent".to_string()
}

fn default_trash() -> String {
    "Trash".to_string()
}

/// What `archive_folder` means when unset.
pub const DEFAULT_ARCHIVE_FOLDER: &str = "Archive";
/// What `junk_folder` means when unset.
pub const DEFAULT_JUNK_FOLDER: &str = "Junk";

/// Key bindings configuration
/// User-configured bindings take precedence over defaults
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct BindingsConfig {
    /// Bindings for index normal mode (browsing messages)
    #[serde(default)]
    pub index_normal: HashMap<String, String>,
    /// Bindings for index detail mode (viewing message)
    #[serde(default)]
    pub index_detail: HashMap<String, String>,
    /// Bindings for compose normal mode (compose tab, not editing)
    #[serde(default)]
    pub compose_normal: HashMap<String, String>,
    /// Bindings for compose terminal mode (embedded editor running)
    /// Only keys bound here are intercepted, all others go to terminal
    #[serde(default)]
    pub compose_terminal: HashMap<String, String>,
    /// Bindings for message detail terminal mode (pager running)
    /// Only keys bound here are intercepted, all others go to pager
    #[serde(default)]
    pub message_detail_terminal: HashMap<String, String>,
    /// Bindings for compose edit mode (editing header fields)
    #[serde(default)]
    pub compose_edit: HashMap<String, String>,
    /// Bindings for command mode
    #[serde(default)]
    pub command: HashMap<String, String>,
    /// Bindings for task normal mode (browsing tasks)
    #[serde(default)]
    pub task_normal: HashMap<String, String>,
    /// Bindings for task detail mode (viewing task detail)
    #[serde(default)]
    pub task_detail: HashMap<String, String>,
    /// Bindings for task detail terminal mode (body editor running)
    /// Only keys bound here are intercepted, all others go to editor
    #[serde(default)]
    pub task_detail_terminal: HashMap<String, String>,
    /// Bindings for task edit mode (editing task fields)
    #[serde(default)]
    pub task_edit: HashMap<String, String>,
    /// Bindings for date picker mode (fuzzy date input)
    #[serde(default)]
    pub date_picker: HashMap<String, String>,
    /// Bindings for the chat list and conversation, over `index_normal`
    #[serde(default)]
    pub chat_normal: HashMap<String, String>,
    /// Bindings for the chat composer, while it has the keyboard
    #[serde(default)]
    pub chat_compose: HashMap<String, String>,
    /// Commands of the user's own: a name for a command or a chain, which a
    /// key, the command line and the command palette use as a built-in one
    #[serde(default)]
    pub commands: HashMap<String, UserCommand>,
}

/// A command of the user's own (`bindings.commands`): what it runs, written
/// as a binding is, and optionally what the command palette calls it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum UserCommand {
    /// `file-and-next: stage-read && mark-move Archive && advance`
    Run(String),
    /// The same with a title: `{ run: …, title: File and Move On }`
    Titled {
        run: String,
        #[serde(default)]
        title: Option<String>,
    },
}

impl UserCommand {
    pub fn run(&self) -> &str {
        match self {
            UserCommand::Run(run) | UserCommand::Titled { run, .. } => run,
        }
    }

    pub fn title(&self) -> Option<&str> {
        match self {
            UserCommand::Run(_) => None,
            UserCommand::Titled { title, .. } => title.as_deref(),
        }
    }
}

// =============================================================================
// Theme Configuration
// =============================================================================

/// Theme configuration for colors, icons, and box drawing characters
#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    /// Color scheme (hex values like "#1e1e2e")
    pub colors: ThemeColors,
    /// Icon characters (Nerd Font Unicode)
    pub icons: ThemeIcons,
    /// Box drawing characters for borders
    pub borders: ThemeBorders,
    /// Date format in message list: `auto` or strftime-style
    pub date_format: String,
    /// Whether to use Nerd Font icons (false = ASCII fallback)
    pub use_nerd_fonts: bool,
    /// Column width configuration for message list
    pub column_widths: ColumnWidthsConfig,
}

/// Column width configuration for message list
#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ColumnWidthsConfig {
    /// From column width: "auto" for dynamic sizing, or a fixed number
    /// Auto mode calculates width from visible messages, clamped to min/max
    #[serde(default)]
    pub from: ColumnWidth,
    /// Maximum width for From column in auto mode (default: 30)
    #[serde(default = "default_from_max")]
    pub from_max: u16,
    /// Minimum width for From column in auto mode (default: 12)
    #[serde(default = "default_from_min")]
    pub from_min: u16,
    /// Date column width (default: 14)
    #[serde(default = "default_date_width")]
    pub date: u16,
}

fn default_from_max() -> u16 {
    30
}

fn default_from_min() -> u16 {
    12
}

fn default_date_width() -> u16 {
    14
}

impl Default for ColumnWidthsConfig {
    fn default() -> Self {
        Self {
            from: ColumnWidth::Auto,
            from_max: default_from_max(),
            from_min: default_from_min(),
            date: default_date_width(),
        }
    }
}

/// Column width mode
#[derive(Debug, Clone, Default)]
pub enum ColumnWidth {
    /// Fixed width in characters
    Fixed(u16),
    /// Auto-calculate from visible content
    #[default]
    Auto,
}

impl<'de> Deserialize<'de> for ColumnWidth {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct ColumnWidthVisitor;

        impl<'de> Visitor<'de> for ColumnWidthVisitor {
            type Value = ColumnWidth;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("\"auto\" or a positive integer")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "auto" {
                    Ok(ColumnWidth::Auto)
                } else {
                    Err(de::Error::invalid_value(
                        de::Unexpected::Str(value),
                        &"\"auto\"",
                    ))
                }
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value > 0 && value <= u16::MAX as i64 {
                    Ok(ColumnWidth::Fixed(value as u16))
                } else {
                    Err(de::Error::invalid_value(
                        de::Unexpected::Signed(value),
                        &"a positive integer <= 65535",
                    ))
                }
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value > 0 && value <= u16::MAX as u64 {
                    Ok(ColumnWidth::Fixed(value as u16))
                } else {
                    Err(de::Error::invalid_value(
                        de::Unexpected::Unsigned(value),
                        &"a positive integer <= 65535",
                    ))
                }
            }
        }

        deserializer.deserialize_any(ColumnWidthVisitor)
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            colors: ThemeColors::default(),
            icons: ThemeIcons::default(),
            borders: ThemeBorders::default(),
            date_format: "auto".to_string(),
            use_nerd_fonts: true,
            column_widths: ColumnWidthsConfig::default(),
        }
    }
}

/// Color scheme - Catppuccin Mocha by default
#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeColors {
    // Backgrounds
    /// Main application background
    pub bg_base: String,
    /// Elevated elements, selection highlight
    pub bg_surface: String,

    // Text hierarchy
    /// Primary text content
    pub text_primary: String,
    /// Secondary text, metadata
    pub text_secondary: String,
    /// Tertiary text, disabled states
    pub text_muted: String,

    // Semantic accents
    /// Selection, focus, active items
    pub accent_active: String,
    /// Unread indicators, errors, alerts
    pub accent_unread: String,
    /// Attachments, warnings
    pub accent_attachment: String,
    /// Success states, confirmations
    pub accent_success: String,
    /// Flags, special indicators
    pub accent_flagged: String,

    // UI elements
    /// Panel borders, dividers (inactive)
    pub border: String,
    /// Focused panel border
    pub border_active: String,
    /// Scrollbar track
    pub scrollbar_track: String,
    /// Scrollbar thumb
    pub scrollbar_thumb: String,
}

impl Default for ThemeColors {
    fn default() -> Self {
        // Catppuccin Mocha palette
        Self {
            bg_base: "#1e1e2e".to_string(),
            bg_surface: "#313244".to_string(),
            text_primary: "#cdd6f4".to_string(),
            text_secondary: "#a6adc8".to_string(),
            text_muted: "#6c7086".to_string(),
            accent_active: "#89b4fa".to_string(),
            accent_unread: "#f38ba8".to_string(),
            accent_attachment: "#f9e2af".to_string(),
            accent_success: "#a6e3a1".to_string(),
            accent_flagged: "#cba6f7".to_string(),
            border: "#45475a".to_string(),
            border_active: "#89b4fa".to_string(),
            scrollbar_track: "#313244".to_string(),
            scrollbar_thumb: "#6c7086".to_string(),
        }
    }
}

/// Icon characters - Nerd Font by default with ASCII fallbacks
#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeIcons {
    // Folder icons
    pub folder_inbox: String,
    pub folder_archive: String,
    pub folder_drafts: String,
    pub folder_sent: String,
    pub folder_trash: String,
    pub folder_default: String,
    pub folder_pinned: String,

    // Account icons
    pub account_personal: String,
    pub account_work: String,

    // Message status icons
    pub status_unread: String,
    pub status_read: String,
    pub status_attachment: String,
    pub status_flagged: String,

    // Pending action icons
    pub action_delete: String,
    pub action_move: String,

    // UI icons
    pub external_change: String,
    pub compose: String,
    pub check: String,

    // Spinner frames (animated)
    pub spinner_frames: Vec<String>,

    // ASCII fallbacks (used when use_nerd_fonts = false)
    pub fallback_folder_inbox: String,
    pub fallback_folder_archive: String,
    pub fallback_folder_drafts: String,
    pub fallback_folder_sent: String,
    pub fallback_folder_trash: String,
    pub fallback_folder_default: String,
    pub fallback_folder_pinned: String,
    pub fallback_account_personal: String,
    pub fallback_account_work: String,
    pub fallback_status_unread: String,
    pub fallback_status_read: String,
    pub fallback_status_attachment: String,
    pub fallback_status_flagged: String,
    pub fallback_action_delete: String,
    pub fallback_action_move: String,
    pub fallback_external_change: String,
    pub fallback_compose: String,
    pub fallback_check: String,
    pub fallback_spinner_frames: Vec<String>,
}

impl Default for ThemeIcons {
    fn default() -> Self {
        Self {
            // Nerd Font icons (Font Awesome subset)
            folder_inbox: "\u{f01c}".to_string(),   //
            folder_archive: "\u{f187}".to_string(), //
            folder_drafts: "\u{f15c}".to_string(),  //
            folder_sent: "\u{f1d8}".to_string(),    //
            folder_trash: "\u{f1f8}".to_string(),   //
            folder_default: "\u{f07b}".to_string(), //
            folder_pinned: "\u{f08d}".to_string(),  // (pin/thumbtack)

            account_personal: "\u{f007}".to_string(), //
            account_work: "\u{f0b1}".to_string(),     //

            status_unread: "\u{f111}".to_string(),     //
            status_read: "\u{f10c}".to_string(),       //
            status_attachment: "\u{f0c6}".to_string(), //
            status_flagged: "\u{f024}".to_string(),    //

            action_delete: "\u{f1f8}".to_string(), //
            action_move: "\u{f07b}".to_string(),   //

            external_change: "\u{f021}".to_string(), //
            compose: "\u{f040}".to_string(),         //
            check: "\u{f00c}".to_string(),           //

            // Braille spinner
            spinner_frames: vec![
                "⠋".to_string(),
                "⠙".to_string(),
                "⠹".to_string(),
                "⠸".to_string(),
                "⠼".to_string(),
                "⠴".to_string(),
                "⠦".to_string(),
                "⠧".to_string(),
                "⠇".to_string(),
                "⠏".to_string(),
            ],

            // ASCII fallbacks
            fallback_folder_inbox: ">".to_string(),
            fallback_folder_archive: ">".to_string(),
            fallback_folder_drafts: ">".to_string(),
            fallback_folder_sent: ">".to_string(),
            fallback_folder_trash: ">".to_string(),
            fallback_folder_default: ">".to_string(),
            fallback_folder_pinned: "*".to_string(),
            fallback_account_personal: "@".to_string(),
            fallback_account_work: "#".to_string(),
            fallback_status_unread: "*".to_string(),
            fallback_status_read: "o".to_string(),
            fallback_status_attachment: "@".to_string(),
            fallback_status_flagged: "!".to_string(),
            fallback_action_delete: "D".to_string(),
            fallback_action_move: "M".to_string(),
            fallback_external_change: "~".to_string(),
            fallback_compose: "+".to_string(),
            fallback_check: "v".to_string(),
            fallback_spinner_frames: vec![
                "|".to_string(),
                "/".to_string(),
                "-".to_string(),
                "\\".to_string(),
            ],
        }
    }
}

/// Box drawing characters for borders
#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeBorders {
    // Corners (rounded by default)
    pub top_left: String,
    pub top_right: String,
    pub bottom_left: String,
    pub bottom_right: String,

    // Lines
    pub horizontal: String,
    pub vertical: String,

    // T-junctions
    pub t_left: String,
    pub t_right: String,
    pub t_top: String,
    pub t_bottom: String,

    // Cross
    pub cross: String,
}

impl Default for ThemeBorders {
    fn default() -> Self {
        // Rounded corners by default
        Self {
            top_left: "╭".to_string(),
            top_right: "╮".to_string(),
            bottom_left: "╰".to_string(),
            bottom_right: "╯".to_string(),
            horizontal: "─".to_string(),
            vertical: "│".to_string(),
            t_left: "├".to_string(),
            t_right: "┤".to_string(),
            t_top: "┬".to_string(),
            t_bottom: "┴".to_string(),
            cross: "┼".to_string(),
        }
    }
}

/// Which of an account's two logins a password is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    Imap,
    Smtp,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::Imap => "IMAP",
            CredentialKind::Smtp => "SMTP",
        }
    }
}

/// A login: who, and where. Two logins with the same username and server are
/// the same credential, which is how an SMTP login that defaults to the IMAP
/// one shares its password rather than asking for it twice.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Credential {
    pub kind: CredentialKind,
    pub username: String,
    pub server: String,
}

impl Credential {
    /// The name the password is stored under in the Keychain.
    pub fn key(&self) -> String {
        format!("{}@{}", self.username, self.server)
    }

    /// Whether this is the same login as `other`, the kind aside.
    pub fn same_login(&self, other: &Credential) -> bool {
        self.username == other.username && self.server == other.server
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    pub imap_server: String,
    pub imap_port: u16,
    pub imap_username: String,
    /// Shell command that prints the IMAP password. Without one the password
    /// is looked up in the Keychain under `imap_username@imap_server` and,
    /// failing that, asked for when it is first needed; see
    /// [`crate::credentials::PasswordCache`].
    pub imap_password_command: Option<String>,
    /// How the IMAP connection is secured: `tls` (default, the usual port
    /// 993) or `starttls` (a plain connection, usually port 143, upgraded
    /// before anything else is said).
    #[serde(default)]
    pub imap_encryption: Encryption,
    /// How the IMAP login is made: `password` (default), or `xoauth2`, where
    /// the password command prints an OAuth 2.0 access token.
    #[serde(default)]
    pub imap_auth: AuthMethod,
    pub from: String,
    /// SMTP server for sending (defaults to imap_server if not set)
    pub smtp_server: Option<String>,
    /// SMTP port (defaults to 587 for STARTTLS)
    pub smtp_port: Option<u16>,
    /// How the SMTP connection is secured: `starttls` (port 587) or `tls`
    /// (port 465). Unset, the port decides: 465 means `tls`, anything else
    /// `starttls`.
    pub smtp_encryption: Option<Encryption>,
    /// How the SMTP login is made: `password` (default) or `xoauth2`, like
    /// `imap_auth`.
    #[serde(default)]
    pub smtp_auth: AuthMethod,
    /// SMTP username (defaults to imap_username if not set)
    pub smtp_username: Option<String>,
    /// SMTP password command (defaults to imap_password_command if not set)
    pub smtp_password_command: Option<String>,
    /// Path to the signature of the account's default identity. The `-- `
    /// line that marks it off is added unless the file starts with one.
    pub signature_file: Option<String>,
    /// Further addresses the account sends as, besides `from`.
    #[serde(default)]
    pub identities: Vec<IdentityConfig>,
    /// The recipient domains mail is written to as the account's own
    /// address, as an identity's `domains`.
    #[serde(default)]
    pub domains: Vec<String>,

    // Folder configuration
    /// Folder name for drafts (default: "Drafts")
    #[serde(default = "default_drafts")]
    pub drafts_folder: String,
    /// Folder name for sent messages (default: "Sent")
    #[serde(default = "default_sent")]
    pub sent_folder: String,
    /// Folder name for deleted messages (default: "Trash")
    #[serde(default = "default_trash")]
    pub trash_folder: String,
    /// Where `archive` files a message. Unset means "Archive"; only a folder
    /// named here is checked against the server at launch, since not every
    /// server has one.
    pub archive_folder: Option<String>,
    /// Where `spam` files a message, and where `not-spam` takes it from.
    /// Unset means "Junk"; checked at launch only when named, like
    /// `archive_folder`.
    pub junk_folder: Option<String>,
    /// Default message sort order for all folders (default: date_newest)
    #[serde(default)]
    pub default_sort: SortOrder,
    /// Ordered list of folder names for GUI display
    /// Folders listed here appear first in this order, remaining folders alphabetically after
    #[serde(default)]
    pub folder_order: Vec<String>,
    /// Per-folder configuration (optional, uses defaults if not specified)
    #[serde(default)]
    pub folders: HashMap<String, FolderConfig>,
    /// How often, in seconds, every synced folder of the account is synced
    /// whether or not the server said anything (default: 300). Also how
    /// folders not watched with IDLE, and accounts on servers without IDLE,
    /// get their mail.
    pub sync_interval_seconds: Option<u64>,
    /// Open the account's folders as conversations rather than as messages
    /// (default: false); a folder's own `threads` overrides it. `T` switches
    /// either way in the open folder.
    #[serde(default)]
    pub threads: bool,
    /// Accept any certificate the server presents. Never read from the file:
    /// it exists so the container-backed tests, whose Dovecot has a
    /// self-signed certificate, can drive the real sync and the real IDLE
    /// worker. See `ImapClient::connect_insecure`.
    #[serde(skip)]
    #[doc(hidden)]
    pub accept_invalid_certs: bool,
}

/// How a connection to a server is secured.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Encryption {
    /// TLS from the first byte: IMAP 993, SMTP 465.
    #[default]
    Tls,
    /// A plain connection upgraded with STARTTLS before any credential is
    /// sent: IMAP 143, SMTP 587.
    Starttls,
}

/// How a login is made.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// `LOGIN` with the password the command prints, the Keychain holds or
    /// the prompt gets.
    #[default]
    Password,
    /// SASL XOAUTH2 (Gmail, Office 365) with an OAuth 2.0 access token in
    /// the password's place, which the password command prints; a refused
    /// token is asked for again, so a command that refreshes it is enough.
    Xoauth2,
}

impl AccountConfig {
    /// How the SMTP connection is secured: as configured, else by the port.
    pub fn smtp_encryption(&self) -> Encryption {
        self.smtp_encryption.unwrap_or(if self.smtp_port() == 465 {
            Encryption::Tls
        } else {
            Encryption::Starttls
        })
    }

    /// How this account's connections treat the server's certificate.
    pub fn tls_mode(&self) -> crate::imap::TlsMode {
        if self.accept_invalid_certs {
            crate::imap::TlsMode::AcceptInvalid
        } else {
            crate::imap::TlsMode::Verify
        }
    }

    /// The IMAP password from the account's command; `None` when there is no
    /// command.
    pub fn get_imap_password(&self) -> Result<Option<SecretString>> {
        self.password_command(CredentialKind::Imap)
            .map(|command| self.run_password_command(command))
            .transpose()
    }

    /// The SMTP password from the account's command, which defaults to the
    /// IMAP one; `None` when there is neither.
    pub fn get_smtp_password(&self) -> Result<Option<SecretString>> {
        self.password_command(CredentialKind::Smtp)
            .map(|command| self.run_password_command(command))
            .transpose()
    }

    /// The command that produces the password of this kind, if any. SMTP falls
    /// back to the IMAP command, as it always has.
    pub fn password_command(&self, kind: CredentialKind) -> Option<&str> {
        match kind {
            CredentialKind::Imap => self.imap_password_command.as_deref(),
            CredentialKind::Smtp => self
                .smtp_password_command
                .as_deref()
                .or(self.imap_password_command.as_deref()),
        }
    }

    /// Who this account logs in as, for the Keychain and for a prompt.
    pub fn credential(&self, kind: CredentialKind) -> Credential {
        match kind {
            CredentialKind::Imap => Credential {
                kind,
                username: self.imap_username.clone(),
                server: self.imap_server.clone(),
            },
            CredentialKind::Smtp => Credential {
                kind,
                username: self.smtp_username().to_string(),
                server: self.smtp_server().to_string(),
            },
        }
    }

    #[cfg(not(feature = "desktop"))]
    fn run_password_command(&self, _password_command: &str) -> Result<SecretString> {
        Err(ConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Password commands cannot run on this platform; the password belongs in the Keychain",
        )))
    }

    #[cfg(feature = "desktop")]
    fn run_password_command(&self, password_command: &str) -> Result<SecretString> {
        // Validate password command is not empty
        let password_command = password_command.trim();
        if password_command.is_empty() {
            return Err(ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Password command is empty",
            )));
        }

        // Use shell_words for proper shell argument parsing (handles quotes and spaces)
        let parts = shell_words::split(password_command).map_err(|e| {
            ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Failed to parse password command: {}", e),
            ))
        })?;

        if parts.is_empty() {
            return Err(ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Password command is empty after parsing",
            )));
        }

        let command = &parts[0];
        let args = &parts[1..];

        let output = Command::new(command).args(args).output();

        match output {
            Ok(output) => {
                // Check command exit status
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let exit_code = output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(ConfigError::Io(std::io::Error::other(format!(
                        "Password command failed with exit code {}: {}",
                        exit_code,
                        stderr.trim()
                    ))));
                }

                let password = String::from_utf8(output.stdout)
                    .map_err(|e| {
                        ConfigError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Password command output is not valid UTF-8: {}", e),
                        ))
                    })?
                    .trim()
                    .to_string();
                // Wrap in SecretString for secure memory handling (zeroized on drop)
                Ok(SecretString::from(password))
            }
            Err(e) => Err(ConfigError::from(e)),
        }
    }

    /// Get SMTP server (defaults to IMAP server)
    pub fn smtp_server(&self) -> &str {
        self.smtp_server.as_deref().unwrap_or(&self.imap_server)
    }

    /// Get SMTP port (defaults to 587 for STARTTLS)
    pub fn smtp_port(&self) -> u16 {
        self.smtp_port.unwrap_or(587)
    }

    /// Get SMTP username (defaults to IMAP username)
    pub fn smtp_username(&self) -> &str {
        self.smtp_username.as_deref().unwrap_or(&self.imap_username)
    }

    /// Every identity of the account, the default one first.
    ///
    /// The default is the account's own `from` and `signature_file`, under the
    /// account's name: it is what a new message starts with, and what a reply
    /// falls back to when no other identity was written to.
    pub fn identities(&self) -> Vec<IdentityConfig> {
        let default = IdentityConfig {
            name: self.name.clone(),
            from: self.from.clone(),
            signature_file: self.signature_file.clone(),
            domains: self.domains.clone(),
            ..IdentityConfig::default()
        };
        std::iter::once(default)
            .chain(self.identities.iter().cloned())
            .collect()
    }

    /// The identity called `name`.
    pub fn identity(&self, name: &str) -> Option<IdentityConfig> {
        self.identities().into_iter().find(|i| i.name == name)
    }

    /// The account as SMTP sees it for mail sent as `from`: the account
    /// itself, or, when `from` is an identity's with a server of its own, a
    /// copy carrying that server, login and address — and a name of its own,
    /// so its password is cached apart from the account's. Matched by the
    /// address rather than the identity's name, so a message queued while
    /// offline still finds its server.
    pub fn sending_as(&self, from: Option<&str>) -> std::borrow::Cow<'_, AccountConfig> {
        let own_server = from.and_then(|from| {
            let address = bare_address(from);
            self.identities.iter().find(|i| {
                i.smtp_server.is_some() && bare_address(&i.from).eq_ignore_ascii_case(&address)
            })
        });
        let Some(identity) = own_server else {
            return std::borrow::Cow::Borrowed(self);
        };
        let mut account = self.clone();
        account.name = format!("{} ({})", self.name, identity.name);
        account.from = identity.from.clone();
        account.smtp_server = identity.smtp_server.clone();
        account.smtp_port = identity.smtp_port;
        account.smtp_encryption = identity.smtp_encryption;
        account.smtp_auth = identity.smtp_auth.unwrap_or_default();
        account.smtp_username = identity.smtp_username.clone();
        account.smtp_password_command = identity.smtp_password_command.clone();
        // Else the SMTP password would fall back to the account's command.
        account.imap_password_command = None;
        std::borrow::Cow::Owned(account)
    }

    /// Get folder config, returning defaults for unconfigured folders
    pub fn get_folder_config(&self, folder_name: &str) -> FolderConfig {
        self.folders.get(folder_name).cloned().unwrap_or_default()
    }

    /// Get effective sort order for a folder (folder-specific or account default)
    pub fn get_folder_sort(&self, folder_name: &str) -> SortOrder {
        self.get_folder_config(folder_name)
            .sort
            .unwrap_or(self.default_sort)
    }

    /// Get display name for a folder (custom name or original name)
    pub fn get_folder_display_name<'a>(&'a self, folder_name: &'a str) -> &'a str {
        self.folders
            .get(folder_name)
            .and_then(|c| c.display_name.as_deref())
            .unwrap_or(folder_name)
    }

    /// The folder `archive` files into.
    pub fn archive_folder_name(&self) -> &str {
        self.archive_folder
            .as_deref()
            .unwrap_or(DEFAULT_ARCHIVE_FOLDER)
    }

    /// The folder `spam` files into.
    pub fn junk_folder_name(&self) -> &str {
        self.junk_folder.as_deref().unwrap_or(DEFAULT_JUNK_FOLDER)
    }

    /// Check if folder should be visible in GUI
    pub fn is_folder_visible(&self, folder_name: &str) -> bool {
        self.get_folder_config(folder_name).visible
    }

    /// Whether a folder opens as conversations.
    pub fn threads_for(&self, folder_name: &str) -> bool {
        self.get_folder_config(folder_name)
            .threads
            .unwrap_or(self.threads)
    }

    /// Check if folder should be synced from IMAP
    pub fn should_sync_folder(&self, folder_name: &str) -> bool {
        self.get_folder_config(folder_name).sync
    }

    /// Sort folders according to configured order
    /// Folders in folder_order appear first in that order, remaining folders alphabetically
    pub fn sort_folder_names(&self, folder_names: &mut [String]) {
        let position_map: HashMap<&str, usize> = self
            .folder_order
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();

        folder_names.sort_by(|a, b| {
            let pos_a = position_map.get(a.as_str());
            let pos_b = position_map.get(b.as_str());
            match (pos_a, pos_b) {
                (Some(pa), Some(pb)) => pa.cmp(pb),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        });
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    /// There is no file at `path`: a first launch, which [`write_template`]
    /// answers.
    Missing {
        path: String,
    },
    /// The file at `path` is not a config: a YAML error, a missing key, a
    /// misspelled one or a value of the wrong type. The error names the key
    /// and the line.
    Parse {
        path: String,
        error: Box<serde_saphyr::Error>,
    },
    /// The file parses, but says something that cannot hold, such as two
    /// identities of an account under one name.
    Invalid {
        message: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "IO error: {}", e),
            ConfigError::Missing { path } => write!(f, "Config file not found at {path}"),
            ConfigError::Parse { path, error } => {
                write!(f, "Parse error in {path}:\n{error}")
            }
            ConfigError::Invalid { message } => write!(f, "Invalid config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

pub type Result<T> = std::result::Result<T, ConfigError>;

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

fn postprocess_and_validate(mut config: Config) -> Result<Config> {
    config.db_path = shellexpand::tilde(&config.db_path).to_string();

    if let Some(ref model) = config.embedding_model {
        config.embedding_model = Some(shellexpand::tilde(model).to_string());
    }

    // Expand log file path
    if let Some(ref log_file) = config.log_file {
        config.log_file = Some(shellexpand::tilde(log_file).to_string());
    }

    let expand = |path: &mut Option<String>| {
        if let Some(file) = path {
            *file = shellexpand::tilde(file).to_string();
        }
    };
    expand(&mut config.navigation.escalate_command);
    for account in &mut config.accounts {
        expand(&mut account.signature_file);
        for identity in &mut account.identities {
            expand(&mut identity.signature_file);
        }
        validate_identities(account)?;
    }

    resolve_tasks_account(&mut config);
    check_chat(&mut config);
    resolve_reading_account(&mut config);
    check_filters(&mut config);
    check_views_and_list(&mut config);
    check_dock_badges(&mut config);
    check_pins(&mut config);

    Ok(config)
}

/// Read `pinned` against the rest of the config. What it cannot give — a
/// name that is no view, a view of a section the config does not have, an
/// account or a view that does not exist — is dropped with a warning, as is
/// a second pin of the same thing. Without `pinned`, Today is pinned alone.
fn check_pins(config: &mut Config) {
    let Some(entries) = config.pinned.clone() else {
        config.pins = if config.tasks.is_some() {
            vec![Pin::Tasks("today".to_string())]
        } else {
            Vec::new()
        };
        return;
    };
    let mut warnings = Vec::new();
    let mut pins: Vec<Pin> = Vec::new();
    for entry in entries {
        let pin = match entry {
            PinnedEntry::Name(name) => {
                let name = name.trim().to_ascii_lowercase();
                match PIN_NAMES.iter().find(|(n, _)| *n == name) {
                    None => {
                        let names: Vec<&str> = PIN_NAMES.iter().map(|(n, _)| *n).collect();
                        warnings.push(format!(
                            "pinned: '{name}' is not a view; the views are {}, or a folder or a virtual folder",
                            names.join(", ")
                        ));
                        continue;
                    }
                    Some((_, "tasks")) if config.tasks.is_none() => {
                        warnings.push(format!("pinned: '{name}' needs a tasks section"));
                        continue;
                    }
                    Some((_, "reading")) if config.reading.is_none() => {
                        warnings.push(format!("pinned: '{name}' needs a reading section"));
                        continue;
                    }
                    Some((_, "chat")) if config.chat.is_none() => {
                        warnings.push(format!("pinned: '{name}' needs a chat section"));
                        continue;
                    }
                    Some((_, "tasks")) => Pin::Tasks(name),
                    Some((_, "chat")) => Pin::Waiting,
                    Some(_) if name == "reading" => Pin::Reading("list".to_string()),
                    Some(_) => Pin::Reading(name),
                }
            }
            PinnedEntry::Folder(folder) => {
                if !config.accounts.iter().any(|a| a.name == folder.account) {
                    warnings.push(format!(
                        "pinned: there is no account '{}' for the folder '{}'",
                        folder.account, folder.folder
                    ));
                    continue;
                }
                Pin::Folder(folder)
            }
            PinnedEntry::View(PinnedView { view }) => {
                if !config.virtual_folders.iter().any(|v| v.name == view) {
                    warnings.push(format!(
                        "pinned: there is no virtual folder called '{view}'"
                    ));
                    continue;
                }
                Pin::View(view)
            }
        };
        let same = |a: &Pin, b: &Pin| match (a, b) {
            (Pin::Folder(a), Pin::Folder(b)) => a.account == b.account && a.folder == b.folder,
            _ => a == b,
        };
        if pins.iter().any(|p| same(p, &pin)) {
            warnings.push(format!(
                "pinned: {} is pinned twice; the second is ignored",
                pin.describe()
            ));
            continue;
        }
        pins.push(pin);
    }
    config.pins = pins;
    config.warnings.extend(warnings);
}

impl Pin {
    /// The pin as a warning names it.
    fn describe(&self) -> String {
        match self {
            Pin::Tasks(name) => format!("'{name}'"),
            Pin::Reading(name) if name == "list" => "'reading'".to_string(),
            Pin::Reading(name) => format!("'{name}'"),
            Pin::Waiting => "'waiting'".to_string(),
            Pin::View(name) => format!("the view '{name}'"),
            Pin::Folder(f) => format!("the folder '{}' of {}", f.folder, f.account),
        }
    }
}

impl Config {
    /// The pinned folders, in the sidebar's order.
    pub fn pinned_folders(&self) -> impl Iterator<Item = &PinnedFolder> {
        self.pins.iter().filter_map(|pin| match pin {
            Pin::Folder(folder) => Some(folder),
            _ => None,
        })
    }
}

/// What the Dock badges ask for that cannot be given: an account that does
/// not exist drops its badge, a colour that is not `#rrggbb` falls back to
/// red, and a fifth badge has no corner left.
fn check_dock_badges(config: &mut Config) {
    let mut warnings = Vec::new();
    let accounts: Vec<String> = config.accounts.iter().map(|a| a.name.clone()).collect();
    config.dock_badges.retain(|badge| match &badge.account {
        Some(account) if !accounts.contains(account) => {
            warnings.push(format!(
                "dock_badges: there is no account '{account}'; its badge is left out"
            ));
            false
        }
        _ => true,
    });
    for badge in &mut config.dock_badges {
        if let Some(color) = &badge.color
            && !is_hex_color(color)
        {
            warnings.push(format!(
                "dock_badges: '{color}' is not a colour like #ff3b30; the badge is red"
            ));
            badge.color = None;
        }
    }
    if config.dock_badges.len() > MAX_DOCK_BADGES {
        warnings.push(format!(
            "dock_badges: the Dock icon has room for {MAX_DOCK_BADGES} badges; the rest are \
             left out"
        ));
        config.dock_badges.truncate(MAX_DOCK_BADGES);
    }
    config.warnings.extend(warnings);
}

/// `#rrggbb`.
fn is_hex_color(text: &str) -> bool {
    text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// What the virtual folders and the list settings ask for that cannot be
/// given: a view without its query or folders, a column that does not
/// exist. Each is a warning; the view is dropped, the column ignored.
fn check_views_and_list(config: &mut Config) {
    let mut warnings = Vec::new();
    config.virtual_folders.retain(|view| {
        let problem = match view.source {
            VirtualFolderSource::Search
                if view.query.as_deref().is_none_or(|q| q.trim().is_empty()) =>
            {
                Some("a `search` view needs a `query`")
            }
            VirtualFolderSource::Custom if view.folders.is_empty() => {
                Some("a `custom` view needs `folders`")
            }
            VirtualFolderSource::Search => {
                crate::search::parse(view.query.as_deref().unwrap_or(""))
                    .err()
                    .map(|_| "its `query` does not parse")
            }
            _ => None,
        };
        match problem {
            Some(problem) => {
                warnings.push(format!(
                    "virtual folder '{}' is ignored: {problem}",
                    view.name
                ));
                false
            }
            None => true,
        }
    });
    let check_columns = |names: &[String], what: String, warnings: &mut Vec<String>| {
        for name in names {
            if !LIST_COLUMNS.iter().any(|c| c.eq_ignore_ascii_case(name)) {
                warnings.push(format!(
                    "{what}: '{name}' is not a column; the columns are {}",
                    LIST_COLUMNS.join(", ")
                ));
            }
        }
    };
    if let Some(columns) = &config.list.columns {
        check_columns(columns, "list.columns".to_string(), &mut warnings);
    }
    for account in &config.accounts {
        for (folder, settings) in &account.folders {
            if let Some(columns) = &settings.columns {
                check_columns(
                    columns,
                    format!("{}: folders.{folder}.columns", account.name),
                    &mut warnings,
                );
            }
        }
    }
    config.warnings.extend(warnings);
}

/// What the filter and highlight rules ask for that this config cannot give
/// them. Each is a warning, not an error: the rule still runs where it can.
fn check_filters(config: &mut Config) {
    let mut warnings = Vec::new();
    for rule in &config.filters.rules {
        for account in &rule.accounts {
            if !config.accounts.iter().any(|a| &a.name == account) {
                warnings.push(format!(
                    "filter '{}': there is no account '{account}'",
                    rule.name
                ));
            }
        }
        if rule.then.reading && config.reading.is_none() {
            warnings.push(format!(
                "filter '{}': `reading` needs a `reading` section, so nothing is moved",
                rule.name
            ));
        }
        if rule.then.task && config.tasks.is_none() {
            warnings.push(format!(
                "filter '{}': `task` needs a `tasks` section, so no task is created",
                rule.name
            ));
        }
    }
    config.filters.warnings.extend(warnings);

    let mut warnings = Vec::new();
    for rule in &config.highlights.rules {
        for account in &rule.accounts {
            if !config.accounts.iter().any(|a| &a.name == account) {
                warnings.push(format!(
                    "highlight '{}': there is no account '{account}'",
                    rule.name
                ));
            }
        }
    }
    config.highlights.warnings.extend(warnings);

    let mut warnings = Vec::new();
    for rule in &config.load_remote_images.0.rules {
        for account in &rule.accounts {
            if !config.accounts.iter().any(|a| &a.name == account) {
                warnings.push(format!(
                    "load_remote_images '{}': there is no account '{account}'",
                    rule.name
                ));
            }
        }
    }
    config.load_remote_images.0.warnings.extend(warnings);
}

/// Identities are chosen by name, from the compose window and from a saved
/// draft, so a name that means two things would send from the wrong one.
fn validate_identities(account: &AccountConfig) -> Result<()> {
    let invalid = |message: String| Err(ConfigError::Invalid { message });
    let mut seen = std::collections::HashSet::from([account.name.as_str()]);
    for identity in &account.identities {
        if !seen.insert(identity.name.as_str()) {
            return invalid(format!(
                "account '{}' has more than one identity named '{}' (the account's own name \
                 is taken by its default identity)",
                account.name, identity.name
            ));
        }
        if identity.signature.is_some() && identity.signature_file.is_some() {
            return invalid(format!(
                "identity '{}' of account '{}' has both `signature` and `signature_file`; \
                 keep one",
                identity.name, account.name
            ));
        }
        let smtp_login = identity.smtp_port.is_some()
            || identity.smtp_encryption.is_some()
            || identity.smtp_auth.is_some()
            || identity.smtp_username.is_some()
            || identity.smtp_password_command.is_some();
        if identity.smtp_server.is_none() && smtp_login {
            return invalid(format!(
                "identity '{}' of account '{}' has `smtp_*` keys but no `smtp_server`",
                identity.name, account.name
            ));
        }
        if identity.smtp_server.is_some() && identity.smtp_username.is_none() {
            return invalid(format!(
                "identity '{}' of account '{}' has an `smtp_server` but no `smtp_username`",
                identity.name, account.name
            ));
        }
    }
    Ok(())
}

/// Wire the `tasks` section into the account it names.
///
/// Two failure modes are worth complaining about at startup rather than
/// discovering later. A `tasks` section naming an account that does not exist
/// leaves tasks silently disabled — there is nothing to sync against and no
/// error anywhere. An empty folder name would be sent to the server as
/// `SELECT ""`, which is not a mailbox.
///
/// Both are reported and then the section is dropped, so the rest of the mail
/// client keeps working: a mistyped account name must not stop mail from being
/// read. Reported through `eprintln!` as well as the log because the config is
/// read before the logger has a file to write to.
fn resolve_tasks_account(config: &mut Config) {
    let Some(tasks) = config.tasks.clone() else {
        return;
    };

    let known = config
        .accounts
        .iter()
        .any(|account| account.name == tasks.account);
    let problem = if !known {
        Some(format!(
            "tasks.account '{}' does not match any configured account",
            tasks.account
        ))
    } else if tasks.folder.trim().is_empty() {
        Some("tasks.folder is empty; it must name a server-side folder".to_string())
    } else {
        None
    };

    if let Some(problem) = problem {
        eprintln!("Warning: {problem}; task management is disabled");
        crate::logger::warn(&format!("{problem}; task management is disabled"));
        config.tasks = None;
        return;
    }

    config.hide_tasks_folder(&tasks.account, &tasks.folder);
}

/// Whether an http(s) URL names this machine: `localhost`, `127.0.0.0/8`
/// or `[::1]`, with or without a port.
fn is_loopback_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        &authority[1..=end]
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, port)| {
                if port.chars().all(|c| c.is_ascii_digit()) {
                    host
                } else {
                    authority
                }
            })
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A chat section Orb cannot use is reported and dropped, as a broken tasks
/// section is: chat being off must not stop mail from being read.
fn check_chat(config: &mut Config) {
    let Some(chat) = config.chat.as_mut() else {
        return;
    };
    chat.server = chat.server.trim().trim_end_matches('/').to_string();
    let problem = if !(chat.server.starts_with("https://") || chat.server.starts_with("http://")) {
        Some(format!(
            "chat.server '{}' is not an http(s) address",
            chat.server
        ))
    } else if chat.server.starts_with("http://") && !is_loopback_url(&chat.server) {
        // The session token and every message would travel in clear. Only a
        // server on this machine — a test container — is reached that way.
        Some(format!(
            "chat.server '{}' is http, which would send the session in clear; use https",
            chat.server
        ))
    } else if chat.server.splitn(4, '/').nth(3).is_some() {
        Some(format!(
            "chat.server '{}' has a path; give the host only",
            chat.server
        ))
    } else if chat.sign_in == ChatSignIn::TokenCommand && chat.token_command.is_none() {
        Some("chat.sign_in is token_command, but chat.token_command is not set".to_string())
    } else {
        None
    };
    if let Some(problem) = problem {
        eprintln!("Warning: {problem}; chat is off");
        crate::logger::warn(&format!("{problem}; chat is off"));
        config.warnings.push(format!("{problem}; chat is off"));
        config.chat = None;
        return;
    }
    // A link that cannot be followed is dropped alone; the others still work.
    let mut warnings = Vec::new();
    chat.links.retain(|link| {
        let problem = if link.pattern.is_empty() {
            "has no pattern"
        } else if !(link.url.starts_with("https://") || link.url.starts_with("http://")) {
            "has a url that is not an http(s) address"
        } else {
            return true;
        };
        warnings.push(format!("chat.links '{}' {problem}; ignored", link.pattern));
        false
    });
    for warning in warnings {
        eprintln!("Warning: {warning}");
        crate::logger::warn(&warning);
        config.warnings.push(warning);
    }
}

/// Check the `reading` section, and drop it with a warning when it names no
/// account or no folders, so that the rest of the client keeps working.
fn resolve_reading_account(config: &mut Config) {
    let Some(reading) = config.reading.clone() else {
        return;
    };
    let problem = if !config.accounts.iter().any(|a| a.name == reading.account) {
        Some(format!(
            "reading.account '{}' does not match any configured account",
            reading.account
        ))
    } else if reading.folder.trim().is_empty() || reading.archive_folder.trim().is_empty() {
        Some("reading.folder and reading.archive_folder must name a server-side folder".to_string())
    } else if names_folder(&reading.folder, &reading.archive_folder) {
        Some("reading.folder and reading.archive_folder name the same folder".to_string())
    } else if config.tasks.as_ref().is_some_and(|tasks| {
        tasks.account == reading.account
            && (tasks.matches_folder(&reading.folder)
                || tasks.matches_folder(&reading.archive_folder))
    }) {
        Some("a reading folder is the tasks folder".to_string())
    } else {
        None
    };
    if problem.is_none()
        && let Some(reading) = config.reading.as_mut()
    {
        let mut warnings = Vec::new();
        reading.feeds.retain(|feed| {
            let url = feed.url.trim();
            let usable = url.starts_with("https://") || url.starts_with("http://");
            if !usable {
                warnings.push(format!(
                    "reading.feeds: '{}' is not an http or https address; it is not polled",
                    feed.url
                ));
            }
            usable
        });
        reading.poll_minutes = reading.poll_minutes.max(5);
        config.warnings.extend(warnings);
    }
    if let Some(problem) = problem {
        eprintln!("Warning: {problem}; the reading list is off");
        crate::logger::warn(&format!("{problem}; the reading list is off"));
        config
            .warnings
            .push(format!("{problem}; the reading list is off"));
        config.reading = None;
    }
}

/// The config file Orb reads: `ORB_CONFIG` when it is set and not empty,
/// `~/.config/orb/config.yml` otherwise.
pub fn path() -> String {
    resolve_path(std::env::var("ORB_CONFIG").ok())
}

fn resolve_path(env: Option<String>) -> String {
    env.filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| CONFIG_PATH.to_string())
}

pub fn read() -> Result<Config> {
    read_from_path(&path())
}

/// Read config from a specific path (for FFI and testing)
pub fn read_from_path(config_path: &str) -> Result<Config> {
    let path = PathBuf::from(shellexpand::tilde(config_path).as_ref());
    if !path.exists() {
        return Err(ConfigError::Missing {
            path: path.display().to_string(),
        });
    }

    let raw_config = fs::read_to_string(&path)?;
    let config = parse(&raw_config, &path)?;

    postprocess_and_validate(config)
}

fn parse(raw: &str, path: &Path) -> Result<Config> {
    serde_saphyr::from_str::<Config>(raw).map_err(|error| ConfigError::Parse {
        path: path.display().to_string(),
        error: Box::new(error),
    })
}

/// What a first launch writes: the required keys with placeholder values, the
/// rest as a pointer to `config.example.yml`. It parses as it stands, so the
/// next launch gets as far as a server that does not exist rather than a
/// syntax error in a file the user has not touched yet.
pub const TEMPLATE: &str = include_str!("../config.template.yml");

/// Write [`TEMPLATE`] to `config_path`, creating the directory above it.
///
/// Refuses to overwrite: a file that exists is the user's, however broken.
pub fn write_template(config_path: &str) -> std::io::Result<PathBuf> {
    let path = PathBuf::from(shellexpand::tilde(config_path).as_ref());
    if path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", path.display()),
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, TEMPLATE)?;
    Ok(path)
}

/// The keys whose change a running Orb cannot follow, between the config it
/// runs on and a freshly read one.
///
/// Everything read at the moment it is used — bindings, the reply settings,
/// folder visibility, the special folder names, the remote-image allowlist —
/// takes effect as soon as the new config is swapped in. What is listed here
/// was turned into something at launch that is not rebuilt: the database, the
/// log, the search model, the connections and the sync schedule, the tasks
/// folder. Per-account entries name the account so the reader knows which
/// one; a folder's `sync` or `watch` flag counts as the account's, since the
/// schedule was built from it.
pub fn keys_needing_restart(old: &Config, new: &Config) -> Vec<String> {
    let mut keys = Vec::new();
    if old.db_path != new.db_path {
        keys.push("db_path".to_string());
    }
    if old.log_file != new.log_file {
        keys.push("log_file".to_string());
    }
    if old.embedding_model != new.embedding_model {
        keys.push("embedding_model".to_string());
    }
    // What Today shows is read afresh every time; where the tasks live is not.
    let place = |c: &Config| {
        c.tasks
            .as_ref()
            .map(|t| (t.account.clone(), t.folder.clone(), t.locale.clone()))
    };
    if place(old) != place(new) {
        keys.push("tasks".to_string());
    }
    // The rooms are read afresh every time; the server and the session are not.
    let server = |c: &Config| {
        c.chat.as_ref().map(|chat| {
            (
                chat.server.clone(),
                chat.sign_in,
                chat.token_command.clone(),
            )
        })
    };
    if server(old) != server(new) {
        keys.push("chat".to_string());
    }

    let old_names: Vec<&str> = old.accounts.iter().map(|a| a.name.as_str()).collect();
    let new_names: Vec<&str> = new.accounts.iter().map(|a| a.name.as_str()).collect();
    if old_names != new_names {
        keys.push("accounts".to_string());
        return keys;
    }
    for (before, after) in old.accounts.iter().zip(&new.accounts) {
        if !same_connection(before, after) {
            keys.push(format!("accounts[{}]", after.name));
        }
    }
    keys
}

/// Whether two versions of an account connect and sync the same way.
fn same_connection(a: &AccountConfig, b: &AccountConfig) -> bool {
    if a.imap_encryption != b.imap_encryption || a.imap_auth != b.imap_auth {
        return false;
    }
    let synced = |account: &AccountConfig| {
        let mut flags: Vec<(String, bool, bool)> = account
            .folders
            .iter()
            .map(|(name, folder)| (name.clone(), folder.sync, folder.watch))
            .collect();
        flags.sort();
        flags
    };
    a.imap_server == b.imap_server
        && a.imap_port == b.imap_port
        && a.imap_username == b.imap_username
        && a.imap_password_command == b.imap_password_command
        && a.smtp_server == b.smtp_server
        && a.smtp_port == b.smtp_port
        && a.smtp_username == b.smtp_username
        && a.smtp_password_command == b.smtp_password_command
        && a.sync_interval_seconds == b.sync_interval_seconds
        && synced(a) == synced(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orb_config_overrides_the_default_path() {
        assert_eq!(resolve_path(None), CONFIG_PATH);
        assert_eq!(resolve_path(Some(String::new())), CONFIG_PATH);
        assert_eq!(resolve_path(Some("  ".into())), CONFIG_PATH);
        assert_eq!(resolve_path(Some("/tmp/orb.yml".into())), "/tmp/orb.yml");
    }
    #[cfg(feature = "desktop")]
    use secrecy::ExposeSecret;

    /// The SMTP port says how the connection is secured unless the config
    /// does; the login defaults to a password.
    #[test]
    fn transport_settings_default_by_port_and_parse_by_name() {
        let mut account = test_account();
        assert_eq!(account.smtp_encryption(), Encryption::Starttls);
        account.smtp_port = Some(465);
        assert_eq!(account.smtp_encryption(), Encryption::Tls);
        account.smtp_encryption = Some(Encryption::Starttls);
        assert_eq!(account.smtp_encryption(), Encryption::Starttls);
        assert_eq!(account.imap_encryption, Encryption::Tls);
        assert_eq!(account.imap_auth, AuthMethod::Password);

        let config: Config = serde_saphyr::from_str(
            "db_path: x\naccounts:\n  - name: A\n    imap_server: s\n    imap_port: 143\n    \
             imap_username: u\n    from: a@x\n    imap_encryption: starttls\n    \
             imap_auth: xoauth2\n    smtp_auth: xoauth2\n    smtp_encryption: tls\n",
        )
        .expect("parse");
        let account = &config.accounts[0];
        assert_eq!(account.imap_encryption, Encryption::Starttls);
        assert_eq!(account.imap_auth, AuthMethod::Xoauth2);
        assert_eq!(account.smtp_auth, AuthMethod::Xoauth2);
        assert_eq!(account.smtp_encryption(), Encryption::Tls);
    }

    /// Helper to create a minimal AccountConfig for testing
    fn test_account() -> AccountConfig {
        AccountConfig {
            name: "Test".to_string(),
            imap_server: "imap.example.com".to_string(),
            imap_port: 993,
            imap_username: "user".to_string(),
            imap_password_command: Some("echo test".to_string()),
            from: "Test <test@example.com>".to_string(),
            smtp_server: None,
            smtp_port: None,
            smtp_username: None,
            smtp_password_command: None,
            signature_file: None,
            identities: Vec::new(),
            domains: Vec::new(),
            default_sort: SortOrder::default(),
            folder_order: Vec::new(),
            folders: HashMap::new(),
            drafts_folder: "Drafts".to_string(),
            sent_folder: "Sent".to_string(),
            trash_folder: "Trash".to_string(),
            archive_folder: None,
            junk_folder: None,
            accept_invalid_certs: false,
            sync_interval_seconds: None,
            threads: false,
            imap_encryption: Default::default(),
            imap_auth: Default::default(),
            smtp_encryption: None,
            smtp_auth: Default::default(),
        }
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn test_password_command_simple() {
        let account = test_account();
        let result = account.run_password_command("echo secret123");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), "secret123");
    }

    #[test]
    #[cfg(not(feature = "desktop"))]
    fn without_desktop_a_password_command_is_refused_rather_than_run() {
        let account = test_account();
        let error = account
            .run_password_command("echo secret123")
            .expect_err("no process is started");
        assert!(error.to_string().contains("cannot run on this platform"));
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn test_password_command_trims_newline() {
        let account = test_account();
        // echo adds a newline, which should be trimmed
        let result = account.run_password_command("printf 'password\\n\\n'");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), "password");
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn test_password_command_with_single_quote() {
        let account = test_account();
        // Password containing a single quote: pass'word
        let result = account.run_password_command("printf \"pass'word\"");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), "pass'word");
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn test_password_command_with_double_quote() {
        let account = test_account();
        // Password containing a double quote: pass"word
        let result = account.run_password_command("printf 'pass\"word'");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), "pass\"word");
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn test_password_command_with_spaces() {
        let account = test_account();
        // Password with spaces
        let result = account.run_password_command("printf 'my secret password'");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), "my secret password");
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn test_password_command_with_special_chars() {
        let account = test_account();
        // Password with various special characters: p@ss!w0rd$#%
        let result = account.run_password_command("printf 'p@ss!w0rd$#%%'");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret(), "p@ss!w0rd$#%");
    }

    #[test]
    fn test_password_command_empty() {
        let account = test_account();
        let result = account.run_password_command("");
        assert!(result.is_err());
    }

    #[test]
    fn test_password_command_whitespace_only() {
        let account = test_account();
        let result = account.run_password_command("   ");
        assert!(result.is_err());
    }

    #[test]
    fn test_password_command_failing() {
        let account = test_account();
        // Command that exits with non-zero status
        let result = account.run_password_command("false");
        assert!(result.is_err());
    }

    #[test]
    fn test_password_command_not_found() {
        let account = test_account();
        let result = account.run_password_command("nonexistent_command_12345");
        assert!(result.is_err());
    }

    fn parse_yaml(raw: &str) -> Config {
        postprocess_and_validate(parse(raw, Path::new("test.yml")).expect("config parses"))
            .expect("config is valid")
    }

    const WORK_ACCOUNT: &str = "\
accounts:
  - name: Work
    imap_server: imap.example.com
    imap_port: 993
    imap_username: user
    imap_password_command: echo x
    from: user@example.com
";

    fn config_with(tasks_section: &str) -> Config {
        parse_yaml(&format!(
            "db_path: ignored\n{WORK_ACCOUNT}{tasks_section}\n"
        ))
    }

    /// A chat server Orb cannot address turns chat off with a warning, and
    /// leaves the rest of the config standing.
    #[test]
    fn a_chat_section_orb_cannot_use_is_dropped() {
        let config = config_with("chat:\n  server: https://chat.example.com/\n");
        let chat = config.chat.expect("the chat section is kept");
        assert_eq!(chat.server, "https://chat.example.com");
        assert_eq!(chat.sign_in, ChatSignIn::Sso);

        for section in [
            "chat:\n  server: chat.example.com\n",
            "chat:\n  server: https://chat.example.com/rocket\n",
            "chat:\n  server: https://chat.example.com\n  sign_in: token_command\n",
            "chat:\n  server: http://chat.example.com\n",
            "chat:\n  server: http://localhost.example.com:3000\n",
        ] {
            let config = config_with(section);
            assert!(config.chat.is_none(), "{section}");
            assert_eq!(config.warnings.len(), 1, "{section}");
            assert_eq!(config.accounts.len(), 1);
        }
    }

    /// Plain http would send the session token in clear, so it is refused —
    /// except to this machine, where the test container listens.
    #[test]
    fn a_plain_http_chat_server_is_allowed_only_on_this_machine() {
        for server in [
            "http://localhost:3000",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
            "http://LOCALHOST",
        ] {
            let config = config_with(&format!("chat:\n  server: {server}\n"));
            assert!(config.chat.is_some(), "{server}");
        }
        assert!(!is_loopback_url("http://chat.example.com"));
        assert!(!is_loopback_url("http://localhost.example.com"));
        assert!(!is_loopback_url("http://127.example.com"));
        assert!(!is_loopback_url("http://evil.example.com:127"));
    }

    /// A chat link that cannot be followed is dropped with a warning; chat
    /// and the other links stay.
    #[test]
    fn a_chat_link_orb_cannot_follow_is_dropped() {
        let config = config_with(
            "chat:\n  server: https://chat.example.com\n  links:\n    - { pattern: 'a', url: 'https://a.example.com/$0' }\n    - { pattern: '', url: 'https://b.example.com' }\n    - { pattern: 'c', url: 'javascript:alert(1)' }\n",
        );
        let chat = config.chat.expect("the chat section is kept");
        assert_eq!(chat.links.len(), 1);
        assert_eq!(chat.links[0].pattern, "a");
        assert_eq!(config.warnings.len(), 2);
    }

    #[test]
    fn a_token_command_prints_a_user_and_a_token() {
        let config = config_with(
            "chat:\n  server: https://chat.example.com\n  sign_in: token_command\n  token_command: echo user123 token456\n",
        );
        let (user, token) = config.chat.unwrap().run_token_command().unwrap();
        assert_eq!(user, "user123");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&token), "token456");

        let config = config_with(
            "chat:\n  server: https://chat.example.com\n  sign_in: token_command\n  token_command: echo only-one\n",
        );
        assert!(config.chat.unwrap().run_token_command().is_err());
    }

    #[test]
    fn escalate_command_has_its_tilde_expanded() {
        assert!(config_with("").navigation.escalate_command.is_none());
        let config = config_with("navigation:\n  escalate_command: ~/bin/navigate");
        let command = config
            .navigation
            .escalate_command
            .expect("the command is kept");
        assert!(!command.starts_with('~'), "{command}");
        assert!(command.ends_with("/bin/navigate"), "{command}");
    }

    #[test]
    fn dock_badges_keep_what_can_be_shown() {
        let config = config_with(
            "dock_badges:
  - account: Work
    color: \"#0a84ff\"
  - folder: Archive
    color: blue
  - account: Nobody
",
        );
        assert_eq!(
            config.dock_badges,
            vec![
                DockBadge {
                    account: Some("Work".into()),
                    folder: "INBOX".into(),
                    color: Some("#0a84ff".into()),
                },
                DockBadge {
                    account: None,
                    folder: "Archive".into(),
                    color: None,
                },
            ]
        );
        assert_eq!(config.warnings.len(), 2, "{:?}", config.warnings);
    }

    /// `pinned` keeps its order and mixes every kind; what it cannot give is
    /// dropped with a warning, the rest stands.
    #[test]
    fn pins_keep_their_order_and_drop_what_cannot_be_given() {
        let config = config_with(
            "tasks: { account: Work, folder: Tasks }
chat: { server: https://chat.example.com }
virtual_folders:
  - { name: All Inboxes, source: all_inboxes }
pinned:
  - today
  - { account: Work, folder: INBOX, display_name: Work }
  - { view: All Inboxes }
  - waiting
  - reading
  - tomorrow
  - { account: Nobody, folder: INBOX }
  - { view: Nowhere }
  - { account: Work, folder: INBOX }
",
        );
        assert_eq!(
            config.pins,
            vec![
                Pin::Tasks("today".into()),
                Pin::Folder(PinnedFolder {
                    account: "Work".into(),
                    folder: "INBOX".into(),
                    display_name: Some("Work".into()),
                }),
                Pin::View("All Inboxes".into()),
                Pin::Waiting,
            ]
        );
        // reading without a reading section, the unknown name, the unknown
        // account, the unknown view and the second Work inbox.
        assert_eq!(config.warnings.len(), 5, "{:?}", config.warnings);
    }

    /// Without `pinned`, Today stands at the top alone; an empty list pins
    /// nothing; `pinned_folders` still reads as it did.
    #[test]
    fn pins_default_to_today_and_read_the_old_name() {
        let tasks = "tasks: { account: Work, folder: Tasks }\n";
        assert_eq!(config_with(tasks).pins, vec![Pin::Tasks("today".into())]);
        assert_eq!(config_with("").pins, vec![]);
        assert_eq!(config_with(&format!("{tasks}pinned: []\n")).pins, vec![]);
        let config = config_with("pinned_folders:\n  - { account: Work, folder: INBOX }\n");
        assert_eq!(config.pinned_folders().count(), 1);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn identities_are_read_and_the_default_comes_first() {
        let config = parse_yaml(&format!(
            "db_path: ignored\n{WORK_ACCOUNT}    signature_file: ~/work.txt
    identities:
      - name: Support
        from: Support <support@example.com>
        signature_file: ~/support.txt
        reply_to: support@example.com
        addresses: [help@example.com]
      - name: Short
        from: Me <user@example.com>
        signature: |
          Me
reply:
  signature: above
"
        ));
        assert_eq!(config.reply.signature, SignaturePlacement::Above);
        let identities = config.accounts[0].identities();
        let names: Vec<_> = identities.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Work", "Support", "Short"]);
        assert!(
            !identities[1]
                .signature_file
                .as_ref()
                .unwrap()
                .starts_with('~')
        );
        assert_eq!(identities[2].get_signature().as_deref(), Some("Me\n"));
        assert!(identities[1].owns("help@example.com"));
    }

    #[test]
    fn an_identity_name_must_say_which_identity() {
        let check = |identities: &str| {
            postprocess_and_validate(
                parse(
                    &format!("db_path: ignored\n{WORK_ACCOUNT}    identities:\n{identities}"),
                    Path::new("test.yml"),
                )
                .expect("parses"),
            )
            .map(|_| ())
        };
        let twice =
            "      - {name: A, from: a@example.com}\n      - {name: A, from: b@example.com}\n";
        assert!(matches!(check(twice), Err(ConfigError::Invalid { .. })));
        let account = "      - {name: Work, from: a@example.com}\n";
        assert!(matches!(check(account), Err(ConfigError::Invalid { .. })));
        let both = "      - {name: A, from: a@example.com, signature: x, signature_file: y}\n";
        assert!(matches!(check(both), Err(ConfigError::Invalid { .. })));
        assert!(check("      - {name: A, from: a@example.com}\n").is_ok());
        let no_server = "      - {name: A, from: a@example.com, smtp_username: a}\n";
        assert!(matches!(check(no_server), Err(ConfigError::Invalid { .. })));
        let no_username = "      - {name: A, from: a@example.com, smtp_server: smtp.a.example}\n";
        assert!(matches!(
            check(no_username),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn an_identity_with_a_server_of_its_own_is_sent_over_it() {
        let config = parse_yaml(&format!(
            "db_path: ignored\n{WORK_ACCOUNT}    smtp_password_command: echo account
    identities:
      - name: Side
        from: Me <me@side.example>
        smtp_server: smtp.side.example
        smtp_port: 465
        smtp_username: me@side.example
      - name: Support
        from: support@example.com
"
        ));
        let account = &config.accounts[0];

        let side = account.sending_as(Some("Someone Else <ME@side.example>"));
        assert_eq!(side.name, "Work (Side)");
        assert_eq!(side.from, "Me <me@side.example>");
        assert_eq!(side.smtp_server(), "smtp.side.example");
        assert_eq!(side.smtp_encryption(), Encryption::Tls);
        assert_eq!(side.smtp_username(), "me@side.example");
        // Neither the account's SMTP command nor its IMAP one.
        assert_eq!(side.password_command(CredentialKind::Smtp), None);

        for from in [None, Some("support@example.com"), Some("user@example.com")] {
            let own = account.sending_as(from);
            assert_eq!(own.name, "Work");
            assert_eq!(own.smtp_server(), "imap.example.com");
        }
    }

    /// The folder name is used exactly as written. A leading `.` is a
    /// Courier/Maildir convention, and inventing one on a server that does not
    /// use it creates a second folder next to the real one.
    #[test]
    fn the_tasks_folder_name_is_taken_verbatim() {
        let config = config_with("tasks:\n  account: Work\n  folder: Tasks");
        let tasks = config.tasks.as_ref().expect("section is kept");
        assert_eq!(tasks.folder, "Tasks");
        assert_eq!(config.tasks_folder_for("Work"), Some("Tasks"));
        assert_eq!(config.tasks_folder_for("Private"), None);

        // An omitted folder falls back to the plain name, still undotted.
        let defaulted = config_with("tasks:\n  account: Work");
        assert_eq!(defaulted.tasks.expect("section").folder, "Tasks");
    }

    /// The designated account stays an ordinary mail account; only the one
    /// folder leaves the mail world. Both predicates matter: `is_folder_visible`
    /// keeps it out of the sidebar and the move targets, `should_sync_folder`
    /// keeps its messages out of the store — and so out of the FTS index, the
    /// unread badges, All Inboxes and the new-mail notifications.
    #[test]
    fn the_tasks_folder_is_neither_shown_nor_synced_as_mail() {
        let config = config_with("tasks:\n  account: Work\n  folder: Tasks");
        let account = &config.accounts[0];

        assert!(!account.is_folder_visible("Tasks"));
        assert!(!account.should_sync_folder("Tasks"));
        assert!(!account.get_folder_config("Tasks").show_unread);

        // Every other folder is untouched.
        assert!(account.is_folder_visible("INBOX"));
        assert!(account.should_sync_folder("INBOX"));
    }

    /// The server may only ever mention the namespace-qualified name, and the
    /// resolved name is registered as well once the server has been asked.
    #[test]
    fn a_namespaced_tasks_folder_is_recognised_and_can_be_hidden() {
        let mut config = config_with("tasks:\n  account: Work\n  folder: Tasks");

        assert!(config.is_tasks_folder("Work", "Tasks"));
        assert!(config.is_tasks_folder("Work", "INBOX.Tasks"));
        assert!(config.is_tasks_folder("Work", "INBOX/Tasks"));
        // Another account's folder of the same name is ordinary mail.
        assert!(!config.is_tasks_folder("Private", "Tasks"));
        assert!(!config.is_tasks_folder("Work", "Tasklist"));
        assert!(!config.is_tasks_folder("Work", "INBOX.Tasks.Archive"));

        // Until the resolved name is registered, only the configured one is
        // excluded — which is what `hide_tasks_folder` is for.
        assert!(config.accounts[0].should_sync_folder("INBOX.Tasks"));
        config.hide_tasks_folder("Work", "INBOX.Tasks");
        assert!(!config.accounts[0].should_sync_folder("INBOX.Tasks"));
        assert!(!config.accounts[0].is_folder_visible("INBOX.Tasks"));
    }

    /// The reading folders stay ordinary, synced folders; only their role
    /// says what they are, under the name the server gives them.
    #[test]
    fn the_reading_folders_are_known_by_their_role() {
        let config = config_with("reading:\n  account: Work");
        let reading = config.reading.as_ref().expect("section is kept");
        assert_eq!(reading.folder, "Reading");
        assert_eq!(reading.archive_folder, "Reading Archive");

        assert_eq!(config.reading_role("Work", "Reading"), Some(READING_ROLE));
        assert_eq!(
            config.reading_role("Work", "INBOX.Reading"),
            Some(READING_ROLE)
        );
        assert_eq!(
            config.reading_role("Work", "INBOX/Reading Archive"),
            Some(READING_ARCHIVE_ROLE)
        );
        assert_eq!(config.reading_role("Private", "Reading"), None);
        assert_eq!(config.reading_role("Work", "INBOX"), None);
        assert!(config.accounts[0].should_sync_folder("Reading"));
        assert!(config.accounts[0].is_folder_visible("Reading"));
    }

    /// A feed that is not a web address is left out with a warning, and a
    /// poll interval too short to be polite is raised.
    #[test]
    fn feeds_that_cannot_be_polled_are_left_out() {
        let config = config_with(
            "reading:\n  account: Work\n  poll_minutes: 1\n  feeds:\n    - url: https://example.com/rss\n    - url: example.com/rss\n",
        );
        let reading = config.reading.as_ref().expect("section is kept");
        assert_eq!(reading.feeds.len(), 1);
        assert_eq!(reading.poll_minutes, 5);
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("example.com/rss") && w.contains("not polled")),
            "{:?}",
            config.warnings
        );
    }

    /// A reading section that cannot work is dropped with a warning, and the
    /// mail goes on.
    #[test]
    fn a_reading_section_that_cannot_work_is_dropped() {
        for section in [
            "reading:\n  account: Nobody",
            "reading:\n  account: Work\n  folder: News\n  archive_folder: News",
            "reading:\n  account: Work\n  folder: \"\"",
            "tasks:\n  account: Work\nreading:\n  account: Work\n  folder: Tasks",
        ] {
            let config = config_with(section);
            assert!(config.reading.is_none(), "{section}");
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|w| w.contains("reading list is off")),
                "{section}: {:?}",
                config.warnings
            );
        }
    }

    /// A folder cannot be both the task store and ordinary mail, so the
    /// exclusion wins over an explicit per-folder entry.
    #[test]
    fn an_explicit_folder_entry_cannot_re_enable_the_tasks_folder() {
        let config = parse_yaml(
            "db_path: ignored\n\
             accounts:\n  \
               - name: Work\n    \
                 imap_server: imap.example.com\n    \
                 imap_port: 993\n    \
                 imap_username: user\n    \
                 imap_password_command: echo x\n    \
                 from: user@example.com\n    \
                 folders:\n      \
                   Tasks:\n        \
                     visible: true\n        \
                     sync: true\n\
             tasks:\n  \
               account: Work\n  \
               folder: Tasks\n",
        );
        assert!(!config.accounts[0].is_folder_visible("Tasks"));
        assert!(!config.accounts[0].should_sync_folder("Tasks"));
    }

    /// A `tasks` section naming an account that does not exist would leave
    /// tasks silently disabled with nothing to sync against. It is reported and
    /// dropped — mail must keep working regardless.
    #[test]
    fn a_tasks_section_naming_an_unknown_account_is_dropped() {
        let config = config_with("tasks:\n  account: Nope\n  folder: Tasks");
        assert!(config.tasks.is_none());
        assert!(config.accounts[0].should_sync_folder("Tasks"));

        // `SELECT ""` is not a mailbox either.
        let empty = config_with("tasks:\n  account: Work\n  folder: '  '");
        assert!(empty.tasks.is_none());
    }

    /// No `tasks` section is the ordinary case: no folder is excluded and
    /// nothing else changes.
    #[test]
    fn without_a_tasks_section_nothing_is_excluded() {
        let config = config_with("");
        assert!(config.tasks.is_none());
        assert!(!config.is_tasks_folder("Work", "Tasks"));
        assert!(config.accounts[0].is_folder_visible("Tasks"));
    }

    /// The shipped example is parsed by the real structs, so a key that the
    /// code drops fails here (unknown keys are rejected), and a section the
    /// example documents is checked to have been read rather than defaulted.
    /// A search starts in the order the config names, as a folder's does;
    /// without one, by relevance.
    #[test]
    fn search_sort_names_a_folder_order_or_relevance() {
        let config = parse_yaml(&format!(
            "db_path: /tmp/orb.db\nsearch:\n  sort: date_newest\n{WORK_ACCOUNT}"
        ));
        assert_eq!(config.search.sort, SearchSort::DateNewest);
        assert_eq!(config.search.sort.as_str(), "date_newest");
        let config = parse_yaml(&format!("db_path: /tmp/orb.db\n{WORK_ACCOUNT}"));
        assert_eq!(config.search.sort, SearchSort::Relevance);
    }

    #[test]
    fn the_example_config_parses_and_every_section_is_read() {
        let raw = include_str!("../config.example.yml");
        let config = parse_yaml(raw);

        assert_eq!(
            config.db_path,
            shellexpand::tilde("~/.local/share/orb/mail.db")
        );
        assert!(config.log_file.is_some());
        assert!(config.embedding_model.is_some());
        assert_eq!(config.default_message_view, MessageView::Html);
        assert_eq!(config.mark_read_after_seconds, 2.0);

        assert_eq!(config.accounts.len(), 2);
        let personal = &config.accounts[0];
        assert_eq!(personal.name, "Personal");
        assert_eq!(personal.smtp_server(), "smtp.example.com");
        assert!(personal.signature_file.is_some());
        assert_eq!(personal.folder_order.len(), 5);
        assert_eq!(personal.get_folder_display_name("INBOX"), "Inbox");
        assert_eq!(
            personal.get_folder_sort("Mailing Lists"),
            SortOrder::DateOldest
        );
        assert!(!personal.get_folder_config("Mailing Lists").show_unread);

        let tasks = config.tasks.as_ref().expect("the tasks section is kept");
        assert_eq!(
            (tasks.account.as_str(), tasks.folder.as_str()),
            ("Work", "Tasks")
        );

        let chat = config.chat.as_ref().expect("the chat section is kept");
        assert_eq!(chat.server, "https://chat.example.com");
        assert_eq!(chat.sign_in, ChatSignIn::Sso);
        assert_eq!(chat.waiting_days, 14);
        assert!(!chat.room("general").waiting);
        assert!(!chat.room("ci-builds").notify);
        assert!(chat.room("support").waiting && !chat.room("support").notify);
        assert_eq!(chat.room("orb-dev"), ChatRoomConfig::default());
        assert_eq!(chat.reactions, [":+1:", ":pray:", ":tada:", ":ok_hand:"]);
        assert_eq!(
            chat.links,
            [ChatLinkConfig {
                pattern: r"#(\d+)".to_string(),
                url: "https://tracker.example.com/issues/$1".to_string(),
            }]
        );
        let reading = config
            .reading
            .as_ref()
            .expect("the reading section is kept");
        assert_eq!(
            (reading.account.as_str(), reading.archive_folder.as_str()),
            ("Personal", "Reading Archive")
        );
        assert_eq!(reading.feeds.len(), 2);
        assert_eq!(reading.feeds[1].name.as_deref(), Some("Example"));
        assert_eq!(reading.poll_minutes, 60);

        assert_eq!(config.reply.prefix, "> ");
        assert_eq!(config.contacts.aliases.len(), 1);
        assert_eq!(config.contacts.groups[0].emails.len(), 2);
        assert_eq!(config.virtual_folders.len(), 3);
        assert_eq!(config.virtual_folders[1].folders.len(), 2);
        assert_eq!(config.pinned_folders().count(), 1);
        assert_eq!(config.subject_munges.len(), 3);
        assert_eq!(config.subject_munges[0].icon.as_deref(), Some("fa-bug"));
        assert_eq!(config.subject_munges[2].replacement, "$0");
        assert_eq!(config.message_links.len(), 2);
        assert_eq!(config.message_links[0].name.as_deref(), Some("issue"));
        assert_eq!(config.message_links[0].target, MessageLinkTarget::Subject);
        assert_eq!(config.message_links[1].target, MessageLinkTarget::Body);
        assert_eq!(
            config.message_links[1].from.as_deref(),
            Some("*@ci.example.com")
        );
        assert_eq!(config.message_links[1].url, "$0");
        assert_eq!(
            config.subject_munges[0].pattern,
            r"^\[JIRA\] \((\w+-\d+)\) "
        );

        assert_eq!(config.bindings.index_normal["<c-s>"], "sync");
        assert_eq!(config.bindings.index_normal["!"], "toggle-flag");
        assert_eq!(config.bindings.index_detail["q"], "back");
        assert_eq!(config.bindings.task_edit["<c-s>"], "accept-edit");
        assert_eq!(config.bindings.date_picker["<c-t>"], "today");

        assert_eq!(config.theme.date_format, "auto");
        assert_eq!(config.list.date_format.as_deref(), Some("auto"));
        assert_eq!(config.search.sort, SearchSort::Relevance);
        assert_eq!(config.theme.colors.bg_base, "#1e1e2e");
        assert_eq!(config.theme.icons.folder_inbox, "\u{f01c}");
        assert_eq!(config.theme.icons.fallback_spinner_frames[3], "\\");
        assert_eq!(config.theme.borders.top_left, "╭");
        assert!(matches!(config.theme.column_widths.from, ColumnWidth::Auto));
    }

    /// An entry on the allow list is an address or a domain; a domain covers
    /// its subdomains, and never a domain that merely ends in it.
    #[test]
    fn remote_images_are_allowed_by_address_or_domain() {
        let config = parse_yaml(&format!(
            "db_path: ignored\n{WORK_ACCOUNT}\
load_remote_images_from:\n  - Alice@Example.com\n  - shop.example\n  - '@news.example.org'\n"
        ));
        assert!(config.allows_remote_images_from("Alice <alice@example.com>"));
        assert!(!config.allows_remote_images_from("Bob <bob@example.com>"));
        assert!(config.allows_remote_images_from("orders@shop.example"));
        assert!(config.allows_remote_images_from("\"Shop\" <no-reply@mail.shop.example>"));
        assert!(!config.allows_remote_images_from("x@notshop.example"));
        assert!(config.allows_remote_images_from("Weekly <weekly@news.example.org>"));
        assert!(!config.allows_remote_images_from("no address here"));

        let unset = parse_yaml(&format!("db_path: ignored\n{WORK_ACCOUNT}"));
        assert!(!unset.allows_remote_images_from("alice@example.com"));
    }

    /// A misspelled key is an error that names it and says where it is, not a
    /// setting that silently does nothing.
    #[test]
    fn an_unknown_key_is_rejected_by_name_and_line() {
        let raw = format!("db_path: ignored\nmark_read_after_second: 3.0\n{WORK_ACCOUNT}");
        let error = parse(&raw, Path::new("config.yml")).expect_err("the typo is rejected");
        let message = error.to_string();
        assert!(message.contains("mark_read_after_second"), "{message}");
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("config.yml"), "{message}");

        let nested = format!(
            "db_path: ignored\n{WORK_ACCOUNT}    folders:\n      INBOX:\n        visble: false\n"
        );
        let error = parse(&nested, Path::new("config.yml")).expect_err("a nested typo is rejected");
        let message = error.to_string();
        assert!(message.contains("visble"), "{message}");
        assert!(message.contains("line 11"), "{message}");
    }

    /// The keys and commands a binding table holds are mostly plain YAML; the
    /// ones that collide with its syntax work once quoted.
    #[test]
    fn binding_keys_need_quoting_only_where_yaml_collides() {
        let config = parse_yaml(&format!(
            "db_path: ignored\n{WORK_ACCOUNT}\
             bindings:\n  \
               index_normal:\n    \
                 <c-r>: sync\n    \
                 <enter>: open\n    \
                 d: stage-read && mark-move Archive && next-message\n    \
                 '!': toggle-flag\n    \
                 \"#\": noop\n    \
                 '[': prev-unread\n    \
                 '~': toggle-view\n    \
                 gg: top\n    \
                 G: bottom\n"
        ));
        let normal = &config.bindings.index_normal;
        assert_eq!(normal["<c-r>"], "sync");
        assert_eq!(
            normal["d"],
            "stage-read && mark-move Archive && next-message"
        );
        assert_eq!(normal["!"], "toggle-flag");
        assert_eq!(normal["#"], "noop");
        assert_eq!(normal["["], "prev-unread");
        assert_eq!(normal["~"], "toggle-view");
        assert_eq!(normal["G"], "bottom");
    }

    /// A command of the user's own is a chain alone, or a chain with a title.
    #[test]
    fn user_commands_are_a_chain_or_a_chain_with_a_title() {
        let config = parse_yaml(&format!(
            "db_path: ignored\n{WORK_ACCOUNT}\
             bindings:\n  \
               commands:\n    \
                 archive-next: stage-read && mark-move Archive && advance\n    \
                 file-and-next:\n      \
                   run: stage-read && mark-move Archive\n      \
                   title: File and Move On\n  \
               index_normal:\n    \
                 a: archive-next\n"
        ));
        let commands = &config.bindings.commands;
        assert_eq!(
            commands["archive-next"].run(),
            "stage-read && mark-move Archive && advance"
        );
        assert_eq!(commands["archive-next"].title(), None);
        assert_eq!(
            commands["file-and-next"].run(),
            "stage-read && mark-move Archive"
        );
        assert_eq!(commands["file-and-next"].title(), Some("File and Move On"));
        assert_eq!(config.bindings.index_normal["a"], "archive-next");
    }

    /// A missing file is reported as such, naming the path that was looked for.
    #[test]
    fn a_missing_config_is_an_error_naming_the_yaml_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml_path = dir.path().join("config.yml");
        let error = read_from_path(yaml_path.to_str().unwrap()).expect_err("missing");
        assert!(matches!(error, ConfigError::Missing { .. }), "{error:?}");
        assert!(error.to_string().contains("config.yml"), "{error}");
    }

    /// The first launch writes a template that the second launch can read.
    #[test]
    fn the_template_is_written_once_and_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml_path = dir.path().join("orb").join("config.yml");
        let written = write_template(yaml_path.to_str().unwrap()).expect("written");
        assert_eq!(written, yaml_path);

        let config = read_from_path(yaml_path.to_str().unwrap()).expect("the template parses");
        assert_eq!(config.accounts.len(), 1);
        assert!(config.accounts[0].imap_password_command.is_none());

        let again = write_template(yaml_path.to_str().unwrap()).expect_err("exists");
        assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);
    }

    /// Without a command the account still describes a login: the password
    /// comes from the Keychain or a prompt, and SMTP shares the IMAP login
    /// unless told otherwise.
    #[test]
    fn an_account_without_a_password_command_names_its_credentials() {
        let config = parse_yaml(
            "db_path: /tmp/x.db\naccounts:\n  - name: A\n    imap_server: imap.example.com\n    \
             imap_port: 993\n    imap_username: me\n    from: me@example.com\n",
        );
        let account = &config.accounts[0];
        assert!(account.password_command(CredentialKind::Imap).is_none());
        assert!(account.password_command(CredentialKind::Smtp).is_none());
        assert!(account.get_imap_password().unwrap().is_none());
        let imap = account.credential(CredentialKind::Imap);
        let smtp = account.credential(CredentialKind::Smtp);
        assert_eq!(imap.key(), "me@imap.example.com");
        assert!(smtp.same_login(&imap));

        let config = parse_yaml(
            "db_path: /tmp/x.db\naccounts:\n  - name: A\n    imap_server: imap.example.com\n    \
             imap_port: 993\n    imap_username: me\n    from: me@example.com\n    \
             imap_password_command: echo imap\n    smtp_server: smtp.example.com\n",
        );
        let account = &config.accounts[0];
        assert_eq!(
            account.password_command(CredentialKind::Smtp),
            Some("echo imap")
        );
        assert!(
            !account
                .credential(CredentialKind::Smtp)
                .same_login(&account.credential(CredentialKind::Imap))
        );
    }

    /// Only what was turned into something at launch asks for a restart.
    #[test]
    fn a_reload_names_the_keys_that_need_a_restart() {
        let base = "db_path: /tmp/x.db\nmark_read_after_seconds: 2\naccounts:\n  - name: A\n    \
                    imap_server: imap.example.com\n    imap_port: 993\n    imap_username: me\n    \
                    from: me@example.com\n    folders:\n      Lists: { sync: false }\n";
        let old = parse_yaml(base);

        let live = parse_yaml(
            &base
                .replace("mark_read_after_seconds: 2", "mark_read_after_seconds: 5")
                .replace(
                    "Lists: { sync: false }",
                    "Lists: { sync: false, visible: false }",
                ),
        );
        assert!(keys_needing_restart(&old, &live).is_empty());

        let moved = parse_yaml(&base.replace("/tmp/x.db", "/tmp/y.db"));
        assert_eq!(keys_needing_restart(&old, &moved), ["db_path"]);

        let resynced = parse_yaml(&base.replace("sync: false", "sync: true"));
        assert_eq!(keys_needing_restart(&old, &resynced), ["accounts[A]"]);

        let renamed = parse_yaml(&base.replace("name: A", "name: B"));
        assert_eq!(keys_needing_restart(&old, &renamed), ["accounts"]);

        // The tasks folder is also taken out of the account's sync, so the
        // account is named as well.
        let with_tasks = parse_yaml(&format!("{base}tasks:\n  account: A\n  folder: Tasks\n"));
        assert_eq!(
            keys_needing_restart(&old, &with_tasks),
            ["tasks", "accounts[A]"]
        );
    }

    /// The archive and junk folders have defaults, and only a configured name
    /// is one the launch check should hold the server to.
    #[test]
    fn archive_and_junk_folders_default_and_can_be_named() {
        let yaml = "db_path: /tmp/x.db\naccounts:\n  - name: A\n    imap_server: s\n    \
                    imap_port: 993\n    imap_username: u\n    imap_password_command: echo\n    \
                    from: a@b.c\n  - name: B\n    imap_server: s\n    imap_port: 993\n    \
                    imap_username: u\n    imap_password_command: echo\n    from: a@b.c\n    \
                    archive_folder: Done\n    junk_folder: Spam\n";
        let config: Config = serde_saphyr::from_str(yaml).expect("parses");
        assert_eq!(config.accounts[0].archive_folder_name(), "Archive");
        assert_eq!(config.accounts[0].junk_folder_name(), "Junk");
        assert!(config.accounts[0].archive_folder.is_none());
        assert_eq!(config.accounts[1].archive_folder_name(), "Done");
        assert_eq!(config.accounts[1].junk_folder_name(), "Spam");
    }

    /// The look is Hybrid unless the config names Terminal, and a misspelt
    /// one is refused rather than quietly read as the default.
    #[test]
    fn the_look_defaults_to_hybrid_and_takes_terminal() {
        let base = "db_path: /tmp/x.db\naccounts:\n  - name: A\n    imap_server: s\n    \
                    imap_port: 993\n    imap_username: u\n    imap_password_command: echo\n    \
                    from: a@b.c\n";
        let config: Config = serde_saphyr::from_str(base).expect("parses");
        assert_eq!(config.appearance.look, Look::Hybrid);
        let config: Config =
            serde_saphyr::from_str(&format!("{base}appearance:\n  look: terminal\n"))
                .expect("parses");
        assert_eq!(config.appearance.look, Look::Terminal);
        assert!(
            serde_saphyr::from_str::<Config>(&format!("{base}appearance:\n  look: tty\n")).is_err()
        );
    }
}
