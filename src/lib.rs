// Core library modules
pub mod attachments;
pub mod chat;
pub mod commands;
pub mod config;
pub mod constants;
pub mod contacts;
pub mod credentials;
pub mod db;
pub mod draft;
#[cfg(feature = "desktop")]
pub mod editor;
#[cfg(feature = "desktop")]
pub mod environment;
pub mod error;
pub mod feeds;
pub mod ffi;
pub mod files;
pub mod filters;
pub mod focus;
pub mod highlights;
pub mod imap;
pub mod keychain;
pub mod logger;
pub mod message_links;
pub mod mime;
pub mod models;
pub mod net;
pub mod rules;
pub mod safety;
pub mod search;
pub mod services;
pub mod setup_code;
pub mod smtp;
pub mod store;
pub mod subject_munger;
pub mod tasks;
pub mod templates;
pub mod threads;
pub mod undo;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

// Public API re-exports
pub use commands::{StateUpdate, SyncStage};
pub use config::Config;
pub use contacts::{ContactService, Recipient, Suggestion as ContactSuggestion};
pub use db::Database;
pub use error::Error;
pub use models::{FolderDisplay, Message, MessageListItem};
pub use smtp::OutgoingEmail;
pub use store::{
    AccountInfo, MessageSort, PendingActionCount, PendingActionRecord, PendingScope, Store,
    SyncReport,
};
pub use tasks::{
    CachedEmailMetadata, ChangeContext, RecurrenceRule, Task, TaskChange, TaskDates, TaskError,
    TaskEvent, TaskListItem, TaskLogEntry, TaskRepository, TaskTag, TaskUpdates, TaskView,
};

pub type Result<T> = std::result::Result<T, error::Error>;
