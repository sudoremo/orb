//! Rocket.Chat: one server, read into the database and written back through
//! its REST API (PLAN.md, Phase 15).
//!
//! The unit is the thread. A thread is named by its room and the id of its
//! first message; a room's own timeline, outside any thread, by its room and
//! an empty thread id. A thread *waits* when its newest message is someone
//! else's and nothing has taken it out of Waiting since ([`store::waiting`]).

pub mod client;
pub mod live;
pub mod marks;
pub mod service;
pub mod store;

pub use client::{RestClient, Session};
pub use service::{ChatListener, ChatService, ChatStatus, ChatUpload};
pub use store::{
    ChatFile, ChatMessage, ChatReaction, ChatRoom, ChatTaskChip, Conversation, OlderMessages,
    WaitingThread,
};

/// What can go wrong talking to the chat server.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ChatError {
    /// The server no longer accepts the session, or there is none.
    #[error("not signed in to the chat server")]
    SignedOut,
    /// The server could not be reached.
    #[error("the chat server cannot be reached: {0}")]
    Network(String),
    /// The server answered with an error.
    #[error("the chat server refused: {0}")]
    Server(String),
    /// Orb's own database failed.
    #[error("chat database: {0}")]
    Database(String),
}

impl From<rusqlite::Error> for ChatError {
    fn from(e: rusqlite::Error) -> Self {
        ChatError::Database(e.to_string())
    }
}

impl From<crate::Error> for ChatError {
    fn from(e: crate::Error) -> Self {
        ChatError::Database(e.to_string())
    }
}

/// The Keychain item a server's session is kept under.
pub fn keychain_key(server: &str) -> String {
    let host = server
        .split_once("://")
        .map_or(server, |(_, rest)| rest)
        .trim_end_matches('/');
    format!("chat:{host}")
}

/// A task's link to one chat message (todo-spec §28), written as the link's
/// id: `chat://<host>/<room id>/<message id>`, and `?thread=<thread id>` for a
/// message of a thread. A mail link's id is a bracketed `Message-ID`, so the
/// two never meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLink {
    pub host: String,
    pub rid: String,
    pub message: String,
    /// The thread the message is in; empty for a room's own timeline.
    pub thread: String,
}

impl ChatLink {
    const SCHEME: &'static str = "chat://";

    pub fn new(server: &str, rid: &str, message: &str, thread: &str) -> Self {
        let host = server
            .split_once("://")
            .map_or(server, |(_, rest)| rest)
            .trim_end_matches('/');
        Self {
            host: host.to_string(),
            rid: rid.to_string(),
            message: message.to_string(),
            thread: thread.to_string(),
        }
    }

    /// The link a task's link id names, when it names a chat message.
    pub fn parse(id: &str) -> Option<Self> {
        let rest = id.strip_prefix(Self::SCHEME)?;
        let (path, thread) = rest.split_once("?thread=").unwrap_or((rest, ""));
        let mut parts = path.split('/');
        let (host, rid, message) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || [host, rid, message].iter().any(|p| p.is_empty()) {
            return None;
        }
        Some(Self::new(host, rid, message, thread))
    }

    pub fn id(&self) -> String {
        let mut id = format!(
            "{}{}/{}/{}",
            Self::SCHEME,
            self.host,
            self.rid,
            self.message
        );
        if !self.thread.is_empty() {
            id.push_str("?thread=");
            id.push_str(&self.thread);
        }
        id
    }
}

/// A link that opens a thread, or one message of it, in Rocket.Chat.
pub fn permalink(server: &str, room: &ChatRoom, thread: &str, message: Option<&str>) -> String {
    let server = server.trim_end_matches('/');
    let path = match room.kind.as_str() {
        "c" => format!("channel/{}", room.name.as_deref().unwrap_or(&room.rid)),
        "p" => format!("group/{}", room.name.as_deref().unwrap_or(&room.rid)),
        _ => format!("direct/{}", room.rid),
    };
    let mut url = format!("{server}/{path}");
    if !thread.is_empty() {
        url.push_str(&format!("/thread/{thread}"));
    }
    if let Some(id) = message {
        url.push_str(&format!("?msg={id}"));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_link_reads_back_what_it_wrote() {
        let link = ChatLink::new("https://chat.example.com/", "r1", "m1", "t1");
        assert_eq!(link.id(), "chat://chat.example.com/r1/m1?thread=t1");
        assert_eq!(ChatLink::parse(&link.id()), Some(link));
        let timeline = ChatLink::new("https://chat.example.com", "r1", "m2", "");
        assert_eq!(timeline.id(), "chat://chat.example.com/r1/m2");
        assert_eq!(ChatLink::parse(&timeline.id()), Some(timeline));
        assert_eq!(ChatLink::parse("<m1@example.com>"), None);
        assert_eq!(ChatLink::parse("chat://host/r1"), None);
    }
}
