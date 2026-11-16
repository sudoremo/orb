use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Option<i64>, // None if not saved yet
    pub folder_id: i64,
    pub uid: u32,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub date: Option<String>, // RFC3339/ISO8601 format in UTC
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub rfc822: Option<Vec<u8>>, // Raw message bytes
    pub size: Option<u32>,
    pub flags: Vec<String>,
    pub modseq: Option<u64>, // CONDSTORE modification sequence
    /// What the server said the message is made of, from BODYSTRUCTURE.
    ///
    /// Recorded during the header sync so the body worker knows which part to
    /// ask for. `None` for a message synced before this existed, or by a code
    /// path that does not request BODYSTRUCTURE.
    pub shape: Option<crate::imap::structure::MessageShape>,
    /// `In-Reply-To`, `References` and `List-Id`, fetched beside the envelope.
    /// `None` for a message synced before Orb asked for them; the header
    /// backfill fills them in.
    pub headers: Option<crate::threads::ThreadHeaders>,
}

/// Lightweight message metadata for list views
///
/// Excludes heavy body content fields (body_text, body_html, rfc822)
/// for efficient rendering of message lists in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageListItem {
    pub id: i64,
    pub folder_id: i64,
    pub uid: u32,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub date: Option<String>, // RFC3339/ISO8601 format in UTC
    pub size: Option<u32>,
    pub flags: Vec<String>,
    pub modseq: Option<u64>,
}

/// A full-text search hit: a message metadata item plus the account and folder
/// it lives in.
///
/// Search spans the whole mailbox, so results can come from any folder of any
/// account. The owning `account_name` / `folder_name` are carried alongside the
/// [`MessageListItem`] so the UI can present context and open the message in its
/// folder regardless of which page is currently loaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultItem {
    pub message: MessageListItem,
    pub account_name: String,
    pub folder_name: String,
}

impl Message {
    pub fn new(folder_id: i64, uid: u32) -> Self {
        Self {
            id: None,
            folder_id,
            uid,
            message_id: None,
            subject: None,
            from: None,
            to: None,
            cc: None,
            date: None,
            body_text: None,
            body_html: None,
            rfc822: None,
            size: None,
            flags: Vec::new(),
            modseq: None,
            shape: None,
            headers: None,
        }
    }
}
