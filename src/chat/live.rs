//! The server's realtime API (DDP over a websocket), used only to learn
//! *that* something changed: which room has a new or edited message, or that
//! the subscriptions changed. What changed is then read through REST, so the
//! stream's own encoding of a message never has to agree with the database.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::ChatError;
use super::client::Session;

/// What the stream reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// The connection is up and subscribed.
    Connected,
    /// A message of this room was written, edited or deleted.
    Room(String),
    /// The subscriptions or rooms changed: a room joined or left, renamed,
    /// read elsewhere.
    Subscriptions,
    /// Someone's status changed: `online`, `away`, `busy` or `offline`.
    Presence { username: String, status: String },
}

/// Silence after which the connection counts as dead. The server pings every
/// 25 seconds or so, and Orb pings too.
const SILENCE_LIMIT: Duration = Duration::from_secs(70);
const PING_EVERY: Duration = Duration::from_secs(25);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// The websocket address of `server`.
pub fn websocket_url(server: &str) -> String {
    let server = server.trim_end_matches('/');
    let rest = server
        .strip_prefix("https://")
        .map(|r| format!("wss://{r}"))
        .or_else(|| server.strip_prefix("http://").map(|r| format!("ws://{r}")))
        .unwrap_or_else(|| server.to_string());
    format!("{rest}/websocket")
}

/// Connect, sign in with the session, subscribe, and report changes until the
/// connection fails or `shutdown` is cancelled. Returns `Ok` only on shutdown.
pub async fn run(
    server: &str,
    session: &Session,
    events: &mpsc::UnboundedSender<LiveEvent>,
    shutdown: &CancellationToken,
) -> Result<(), ChatError> {
    let url = websocket_url(server);
    let (mut socket, _) = timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(&url))
        .await
        .map_err(|_| ChatError::Network("the realtime connection timed out".into()))?
        .map_err(|e| ChatError::Network(e.to_string()))?;

    let send = |value: Value| Message::Text(value.to_string().into());
    socket
        .send(send(
            json!({"msg": "connect", "version": "1", "support": ["1"]}),
        ))
        .await
        .map_err(|e| ChatError::Network(e.to_string()))?;
    socket
        .send(send(json!({
            "msg": "method", "method": "login", "id": "login",
            "params": [{ "resume": session.token.expose_secret() }],
        })))
        .await
        .map_err(|e| ChatError::Network(e.to_string()))?;

    let user = &session.user_id;
    let subscriptions = [
        (
            "stream-notify-user",
            format!("{user}/subscriptions-changed"),
        ),
        ("stream-notify-user", format!("{user}/rooms-changed")),
        ("stream-room-messages", "__my_messages__".to_string()),
        ("stream-notify-logged", "user-status".to_string()),
    ];
    let mut subscribed = false;
    let mut last_heard = Instant::now();
    let mut pings = interval(PING_EVERY);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = socket.close(None).await;
                return Ok(());
            }
            _ = pings.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    return Err(ChatError::Network("the realtime connection went silent".into()));
                }
                socket.send(send(json!({"msg": "ping"}))).await
                    .map_err(|e| ChatError::Network(e.to_string()))?;
            }
            frame = socket.next() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(e)) => return Err(ChatError::Network(e.to_string())),
                    None => return Err(ChatError::Network("the realtime connection closed".into())),
                };
                last_heard = Instant::now();
                let text = match frame {
                    Message::Text(text) => text.to_string(),
                    Message::Close(_) => return Err(ChatError::Network("the server closed the realtime connection".into())),
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                match value.get("msg").and_then(Value::as_str) {
                    Some("ping") => {
                        socket.send(send(json!({"msg": "pong"}))).await
                            .map_err(|e| ChatError::Network(e.to_string()))?;
                    }
                    Some("result") if value.get("id").and_then(Value::as_str) == Some("login") => {
                        if value.get("error").is_some() {
                            return Err(ChatError::SignedOut);
                        }
                        for (i, (name, event)) in subscriptions.iter().enumerate() {
                            socket.send(send(json!({
                                "msg": "sub", "id": format!("sub-{i}"), "name": name,
                                "params": [event, false],
                            }))).await.map_err(|e| ChatError::Network(e.to_string()))?;
                        }
                    }
                    Some("ready") if !subscribed => {
                        subscribed = true;
                        let _ = events.send(LiveEvent::Connected);
                    }
                    Some("nosub") => {
                        let reason = value.get("error").map(Value::to_string).unwrap_or_default();
                        return Err(ChatError::Server(format!("a realtime subscription was refused {reason}")));
                    }
                    Some("changed") => {
                        if let Some(event) = event_of(&value) {
                            let _ = events.send(event);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// What a `changed` frame means for the sync.
fn event_of(frame: &Value) -> Option<LiveEvent> {
    match frame.get("collection").and_then(Value::as_str)? {
        "stream-room-messages" => {
            let message = frame.get("fields")?.get("args")?.get(0)?;
            let rid = message.get("rid")?.as_str()?;
            Some(LiveEvent::Room(rid.to_string()))
        }
        "stream-notify-user" => Some(LiveEvent::Subscriptions),
        "stream-notify-logged" => {
            // `[[user id, username, status, status text]]`, the status a number.
            let fields = frame.get("fields")?;
            if fields.get("eventName")?.as_str()? != "user-status" {
                return None;
            }
            let user = fields.get("args")?.get(0)?;
            let username = user.get(1)?.as_str()?.to_string();
            let status = match user.get(2)?.as_i64()? {
                1 => "online",
                2 => "away",
                3 => "busy",
                _ => "offline",
            };
            Some(LiveEvent::Presence {
                username,
                status: status.to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_websocket_lives_beside_the_rest_api() {
        assert_eq!(
            websocket_url("https://chat.example.com"),
            "wss://chat.example.com/websocket"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:3900/"),
            "ws://127.0.0.1:3900/websocket"
        );
    }

    #[test]
    fn a_message_frame_names_its_room() {
        let frame = json!({
            "msg": "changed", "collection": "stream-room-messages", "id": "id",
            "fields": { "eventName": "__my_messages__", "args": [{ "_id": "m", "rid": "room1" }] }
        });
        assert_eq!(event_of(&frame), Some(LiveEvent::Room("room1".into())));
        let frame = json!({
            "msg": "changed", "collection": "stream-notify-user",
            "fields": { "eventName": "u/subscriptions-changed", "args": ["updated", {}] }
        });
        assert_eq!(event_of(&frame), Some(LiveEvent::Subscriptions));
        let frame = json!({
            "msg": "changed", "collection": "stream-notify-logged",
            "fields": { "eventName": "user-status", "args": [["u1", "lukas", 2, ""]] }
        });
        assert_eq!(
            event_of(&frame),
            Some(LiveEvent::Presence {
                username: "lukas".into(),
                status: "away".into()
            })
        );
    }
}
