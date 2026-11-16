//! Chat (PLAN.md, Phase 15): the commands the frontend calls, the events the
//! chat service raises, and the window the server's own sign-in page opens in.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use orb_core::chat::store::ChatPick;
use orb_core::chat::{
    ChatListener, ChatRoom, ChatStatus, Conversation, OlderMessages, WaitingThread,
};
use orb_core::ffi::{ChatFileInput, OrbError, OrbHandle, TaskDetail};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{offload, open_on_disk, OpenedAttachment};

type Handle = Arc<OrbHandle>;

/// The chat tables changed: the frontend re-reads what it shows.
pub const CHAT_CHANGED_EVENT: &str = "chat-changed";
/// The chat status changed; the payload is the new [`ChatStatus`].
pub const CHAT_STATUS_EVENT: &str = "chat-status";

const SIGN_IN_LABEL: &str = "chat-signin";
/// Where the sign-in window's script sends the session it found. The host is
/// reserved (`.invalid`), so the navigation can only ever be this hand-over,
/// and the navigation handler cancels it before it goes anywhere.
const SIGN_IN_HOST: &str = "orb-chat-signin.invalid";

pub struct ChatBridge {
    pub app: AppHandle,
}

/// The newest a thread started waiting that has been announced, in ms.
pub struct ChatWatermark(pub AtomicI64);

impl ChatListener for ChatBridge {
    fn on_changed(&self) {
        let _ = self.app.emit(CHAT_CHANGED_EVENT, ());
        // Off the chat loop: what started waiting is announced, and the
        // badge counts Waiting when the config says so.
        let app = self.app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let Some(orb) = app.try_state::<Handle>().map(|s| s.inner().clone()) else {
                return;
            };
            if let Some(watermark) = app.try_state::<ChatWatermark>() {
                let since = watermark.0.load(Ordering::Relaxed);
                match orb.chat_new_waiting_since(since) {
                    Ok((threads, next)) => {
                        // Only the call that moves the watermark announces.
                        if watermark.0.fetch_max(next, Ordering::Relaxed) < next {
                            crate::notifications::notify_chat(&threads);
                        }
                    }
                    Err(error) => {
                        orb_core::logger::warn(&format!("chat: nothing to announce: {error}"))
                    }
                }
            }
            crate::update_badge(&app, &orb);
        });
    }

    fn on_status(&self, status: &ChatStatus) {
        let _ = self.app.emit(CHAT_STATUS_EVENT, status.clone());
    }
}

/// The chat server, or `None` when the config has no chat section.
#[tauri::command]
pub async fn chat_server(orb: State<'_, Handle>) -> Result<Option<String>, OrbError> {
    Ok(orb.chat_server())
}

#[tauri::command]
pub async fn chat_status(orb: State<'_, Handle>) -> Result<ChatStatus, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_status", move || orb.chat_status()).await
}

#[tauri::command]
pub async fn chat_waiting(orb: State<'_, Handle>) -> Result<Vec<WaitingThread>, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_waiting", move || orb.chat_waiting()).await
}

#[tauri::command]
pub async fn chat_rooms(orb: State<'_, Handle>) -> Result<Vec<ChatRoom>, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_rooms", move || orb.chat_rooms()).await
}

#[tauri::command]
pub async fn chat_conversation(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
) -> Result<Option<Conversation>, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_conversation", move || {
        orb.chat_conversation(rid, thread)
    })
    .await
}

/// Every thread of a room, its own timeline first.
#[tauri::command]
pub async fn chat_room_threads(
    orb: State<'_, Handle>,
    rid: String,
) -> Result<Vec<WaitingThread>, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_room_threads", move || orb.chat_room_threads(rid)).await
}

/// What the room picker offers for what was typed.
#[tauri::command]
pub async fn chat_find(orb: State<'_, Handle>, term: String) -> Result<Vec<ChatPick>, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_find", move || orb.chat_find(term)).await
}

/// The direct message room with a person, made if need be.
#[tauri::command]
pub async fn chat_open_direct(
    orb: State<'_, Handle>,
    username: String,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_open_direct", move || orb.chat_open_direct(username)).await
}

/// Open the conversation with several people, starting it if there is none.
#[tauri::command]
pub async fn chat_open_group(
    orb: State<'_, Handle>,
    usernames: Vec<String>,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_open_group", move || orb.chat_open_group(usernames)).await
}

/// The reactions the config offers on a message, as shortcodes.
#[tauri::command]
pub async fn chat_reactions(orb: State<'_, Handle>) -> Result<Vec<String>, OrbError> {
    Ok(orb.chat_reactions())
}

/// Whether the chat server has threads turned on.
#[tauri::command]
pub async fn chat_threads(orb: State<'_, Handle>) -> Result<bool, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_threads", move || Ok(orb.chat_threads())).await
}

/// The text the config turns into links in a message.
#[tauri::command]
pub async fn chat_links(
    orb: State<'_, Handle>,
) -> Result<Vec<orb_core::config::ChatLinkConfig>, OrbError> {
    Ok(orb.chat_links())
}

/// Post files, a path each or a pasted one's bytes, `text` with the first.
#[tauri::command]
pub async fn chat_send_files(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
    files: Vec<ChatFileInput>,
    text: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_send_files", move || {
        orb.chat_send_files(rid, thread, files, text)
    })
    .await
}

/// Replace the text of one of the user's messages.
#[tauri::command]
pub async fn chat_edit(
    orb: State<'_, Handle>,
    rid: String,
    id: String,
    text: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_edit", move || orb.chat_edit(rid, id, text)).await
}

/// Delete one of the user's messages.
#[tauri::command]
pub async fn chat_delete(orb: State<'_, Handle>, rid: String, id: String) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_delete", move || orb.chat_delete(rid, id)).await
}

/// Add or take back the user's reaction to a message.
#[tauri::command]
pub async fn chat_react(
    orb: State<'_, Handle>,
    rid: String,
    id: String,
    emoji: String,
    on: bool,
    undo: Option<String>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_react", move || {
        orb.chat_react(rid, id, emoji, on, undo)
    })
    .await
}

/// Mark a thread read or unread, in Orb and on the server.
#[tauri::command]
pub async fn chat_set_read(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
    read: bool,
    undo: Option<String>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_set_read", move || {
        orb.chat_set_read(rid, thread, read, undo)
    })
    .await
}

/// A message's image preview as a `data:` URL; `None` for a file without one.
#[tauri::command]
pub async fn chat_file_preview(
    orb: State<'_, Handle>,
    id: String,
    index: usize,
) -> Result<Option<String>, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_file_preview", move || {
        orb.chat_file_preview(id, index)
    })
    .await
}

/// Open a message's file with its default application, fetching it first —
/// or reveal it in Finder when opening it would run it.
#[tauri::command]
pub async fn chat_open_file(
    orb: State<'_, Handle>,
    id: String,
    index: usize,
) -> Result<OpenedAttachment, OrbError> {
    let orb = orb.inner().clone();
    let path = offload("chat_open_file", move || orb.chat_file_path(id, index)).await?;
    open_on_disk(path)
}

/// Leave a room, or hide a direct message.
#[tauri::command]
pub async fn chat_leave(
    orb: State<'_, Handle>,
    rid: String,
    undo: Option<String>,
) -> Result<bool, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_leave", move || orb.chat_leave(rid, undo)).await
}

/// Create a channel, or a private group; returns its id.
#[tauri::command]
pub async fn chat_create_room(
    orb: State<'_, Handle>,
    name: String,
    private: bool,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_create_room", move || {
        orb.chat_create_room(name, private)
    })
    .await
}

/// Join a public channel.
#[tauri::command]
pub async fn chat_join(orb: State<'_, Handle>, rid: String) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_join", move || orb.chat_join(rid)).await
}

/// The page of messages before `before`, for scrolling back in a conversation.
#[tauri::command]
pub async fn chat_older(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
    before: i64,
) -> Result<OlderMessages, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_older", move || orb.chat_older(rid, thread, before)).await
}

#[tauri::command]
pub async fn chat_mark_seen(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_mark_seen", move || orb.chat_mark_seen(rid, thread)).await
}

#[tauri::command]
pub async fn chat_done(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
    undo: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_done", move || orb.chat_done(rid, thread, undo)).await
}

/// Mute a room until `ends` (ms), or until it is unmuted.
#[tauri::command]
pub async fn chat_mute(
    orb: State<'_, Handle>,
    rid: String,
    ends: Option<i64>,
    undo: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_mute", move || orb.chat_mute(rid, ends, undo)).await
}

#[tauri::command]
pub async fn chat_unmute(
    orb: State<'_, Handle>,
    rid: String,
    undo: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_unmute", move || orb.chat_unmute(rid, undo)).await
}

/// Make a task of a chat message, and take its thread out of Waiting.
#[tauri::command]
pub async fn chat_create_task(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
    message: Option<String>,
    postpone: Option<String>,
) -> Result<TaskDetail, OrbError> {
    let orb = orb.inner().clone();
    offload("chat_create_task", move || {
        orb.chat_create_task(rid, thread, message, postpone)
    })
    .await
}

#[tauri::command]
pub async fn chat_send(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
    text: String,
    also_in_room: bool,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_send", move || {
        orb.chat_send(rid, thread, text, also_in_room)
    })
    .await
}

#[tauri::command]
pub async fn chat_sync(orb: State<'_, Handle>) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_sync", move || orb.chat_sync()).await
}

#[tauri::command]
pub async fn chat_sign_out(orb: State<'_, Handle>) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("chat_sign_out", move || orb.chat_sign_out()).await
}

/// Open the thread in Rocket.Chat, in the browser.
#[tauri::command]
pub async fn chat_open_in_browser(
    orb: State<'_, Handle>,
    rid: String,
    thread: String,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    let url = offload("chat_permalink", move || orb.chat_permalink(rid, thread)).await?;
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| OrbError::SyncError {
            message: format!("could not open {url}: {e}"),
        })?;
    Ok(())
}

/// The script the sign-in window runs on every page: once the server's web
/// client holds a session, hand it over by navigating to [`SIGN_IN_HOST`].
/// Pages of other origins (the identity provider's) are left alone.
fn sign_in_script(server: &str) -> String {
    let origin = serde_json::to_string(server.trim_end_matches('/')).unwrap_or_default();
    format!(
        r#"(function () {{
  if (location.origin !== {origin}) return;
  var look = function () {{
    var token = localStorage.getItem("Meteor.loginToken");
    var user = localStorage.getItem("Meteor.userId");
    if (token && user) {{
      location.href = "https://{SIGN_IN_HOST}/?user=" + encodeURIComponent(user)
        + "&token=" + encodeURIComponent(token);
      return;
    }}
    setTimeout(look, 500);
  }};
  look();
}})();"#
    )
}

/// Open the server's sign-in page in a window of its own, or bring forward
/// the one already open. The window closes itself once it has a session.
#[tauri::command]
pub async fn chat_sign_in_window(app: AppHandle, orb: State<'_, Handle>) -> Result<(), OrbError> {
    if let Some(open) = app.get_webview_window(SIGN_IN_LABEL) {
        let _ = open.show();
        let _ = open.set_focus();
        return Ok(());
    }
    let server = orb.chat_server().ok_or_else(|| OrbError::ConfigError {
        message: "chat is not configured".to_string(),
    })?;
    let url: tauri::Url = format!("{server}/home")
        .parse()
        .map_err(|e| OrbError::ConfigError {
            message: format!("chat.server is not an address: {e}"),
        })?;
    let handover = app.clone();
    let orb = orb.inner().clone();
    tauri::WebviewWindowBuilder::new(&app, SIGN_IN_LABEL, tauri::WebviewUrl::External(url))
        .title("Sign In to Chat")
        .inner_size(520.0, 720.0)
        .initialization_script(sign_in_script(&server))
        .on_navigation(move |url| {
            if url.host_str() != Some(SIGN_IN_HOST) {
                return true;
            }
            let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            if let (Some(user), Some(token)) = (pairs.get("user"), pairs.get("token")) {
                take_session(handover.clone(), orb.clone(), user.clone(), token.clone());
            }
            false
        })
        .build()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not open the sign-in window: {e}"),
        })?;
    Ok(())
}

/// Check the session the window caught and keep it; on success close the
/// window, otherwise forget the session in the page so that the server asks
/// for a sign-in again.
fn take_session(app: AppHandle, orb: Handle, user: String, token: String) {
    std::thread::spawn(move || {
        let window = app.get_webview_window(SIGN_IN_LABEL);
        match orb.chat_sign_in(user, token) {
            Ok(()) => {
                if let Some(window) = window {
                    let _ = window.close();
                }
            }
            Err(error) => {
                eprintln!("orb: chat sign-in: {error}");
                if let Some(window) = window {
                    let _ = window
                        .eval("localStorage.removeItem('Meteor.loginToken'); location.reload();");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sign_in_script_only_looks_at_the_servers_own_pages() {
        let script = sign_in_script("https://chat.example.com/");
        assert!(script.contains(r#"location.origin !== "https://chat.example.com""#));
        assert!(script.contains(SIGN_IN_HOST));
    }
}
