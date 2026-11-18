//! The part of the Rocket.Chat REST API Orb reads and writes.
//!
//! Reading leaves the server's read state alone; only [`RestClient::mark_read`]
//! and [`RestClient::mark_thread_read`] change it, and only for what the user
//! dealt with (PLAN.md, Phase 15).

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{Client, Method, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::ChatError;

/// A session on the server: the user it belongs to and its token.
#[derive(Clone)]
pub struct Session {
    pub user_id: String,
    pub token: SecretString,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

/// The signed-in user.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    #[serde(rename = "_id")]
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireUser {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireMention {
    #[serde(rename = "_id", default)]
    pub id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireFile {
    #[serde(rename = "_id", default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireAttachment {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_link: Option<String>,
    /// An image's preview: the thumbnail the server made of it, or the image
    /// itself when it made none.
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub image_dimensions: Option<WireDimensions>,
    /// Set on a quote: the message quoted, its author and its text.
    #[serde(default)]
    pub message_link: Option<String>,
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct WireDimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireReaction {
    #[serde(default)]
    pub usernames: Vec<String>,
}

/// A message as the REST API and the realtime stream give it.
#[derive(Debug, Clone, Deserialize)]
pub struct WireMessage {
    #[serde(rename = "_id")]
    pub id: String,
    pub rid: String,
    #[serde(default)]
    pub tmid: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(deserialize_with = "millis")]
    pub ts: i64,
    pub u: WireUser,
    #[serde(rename = "_updatedAt", deserialize_with = "millis")]
    pub updated_at: i64,
    #[serde(rename = "editedAt", default, deserialize_with = "maybe_millis")]
    pub edited_at: Option<i64>,
    /// The kind of a system message (someone joined, a topic changed …);
    /// absent on what people wrote.
    #[serde(default)]
    pub t: Option<String>,
    #[serde(default)]
    pub tcount: Option<i64>,
    #[serde(default, deserialize_with = "maybe_millis")]
    pub tlm: Option<i64>,
    #[serde(default)]
    pub mentions: Vec<WireMention>,
    #[serde(default)]
    pub files: Vec<WireFile>,
    #[serde(default)]
    pub attachments: Vec<WireAttachment>,
    #[serde(default)]
    pub reactions: Option<HashMap<String, WireReaction>>,
}

/// A subscription: the user's membership of a room, with its unread state.
#[derive(Debug, Clone, Deserialize)]
pub struct WireSubscription {
    pub rid: String,
    #[serde(default)]
    pub t: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fname: Option<String>,
    #[serde(default)]
    pub open: Option<bool>,
    #[serde(default)]
    pub unread: Option<i64>,
    #[serde(rename = "userMentions", default)]
    pub user_mentions: Option<i64>,
    /// The room's notifications are off, whatever else it says.
    #[serde(rename = "disableNotifications", default)]
    pub disable_notifications: Option<bool>,
    /// `all`, `mentions`, `nothing`, or `default` for the account's own.
    #[serde(rename = "desktopNotifications", default)]
    pub desktop_notifications: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireRoom {
    #[serde(rename = "_id")]
    pub id: String,
    pub t: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fname: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(rename = "usersCount", default)]
    pub users_count: Option<i64>,
    #[serde(default, deserialize_with = "maybe_millis")]
    pub lm: Option<i64>,
    #[serde(rename = "_updatedAt", default, deserialize_with = "maybe_millis")]
    pub updated_at: Option<i64>,
}

/// What the server's search found.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Spotlight {
    #[serde(default)]
    pub users: Vec<SpotlightUser>,
    #[serde(default)]
    pub rooms: Vec<SpotlightRoom>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotlightUser {
    #[serde(rename = "_id")]
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotlightRoom {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fname: Option<String>,
    #[serde(default)]
    pub t: Option<String>,
}

/// What changed since a point in time: the records updated, and the ids of
/// those removed.
#[derive(Debug, Clone, Default)]
pub struct Changes<T> {
    pub updated: Vec<T>,
    pub removed: Vec<String>,
}

#[derive(Deserialize)]
struct UpdateRemove<T> {
    #[serde(default = "Vec::new")]
    update: Vec<T>,
    #[serde(default)]
    remove: Vec<Value>,
}

/// The ids of removed records: the API gives them as objects with an `_id`
/// (subscriptions carry the room's id as `rid`).
fn removed_ids(values: Vec<Value>, field: &str) -> Vec<String> {
    values
        .into_iter()
        .filter_map(|v| v.get(field).and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// A time the API gives either as an ISO 8601 string (REST) or as
/// `{"$date": millis}` (the realtime stream's EJSON).
fn millis<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    maybe_millis(d)?.ok_or_else(|| serde::de::Error::custom("a time is missing"))
}

fn maybe_millis<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let value = Option::<Value>::deserialize(d)?;
    Ok(value.as_ref().and_then(parse_time))
}

pub(crate) fn parse_time(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis()),
        Value::Object(o) => o.get("$date").and_then(Value::as_i64),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// An ISO 8601 time the API accepts as a parameter.
pub fn iso(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A client signed in to one server.
#[derive(Clone)]
pub struct RestClient {
    server: String,
    http: Client,
    session: Session,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The most an API answer may be: a room's history page is kilobytes, a
/// directory of users at most a few megabytes.
const MAX_ANSWER_BYTES: usize = 64 * 1024 * 1024;

/// The most a file the server keeps may be when fetched: above the upload
/// limit of any server Orb has met, and still a bound on the memory one
/// message's attachment can take.
const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;

impl RestClient {
    pub fn new(server: &str, session: Session) -> Result<Self, ChatError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("Orb/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ChatError::Network(e.to_string()))?;
        Ok(Self {
            server: server.trim_end_matches('/').to_string(),
            http,
            session,
        })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<T, ChatError> {
        let url = format!("{}/api/v1/{path}", self.server);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-User-Id", &self.session.user_id)
            .header("X-Auth-Token", self.session.token.expose_secret())
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ChatError::Network(e.to_string()))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ChatError::SignedOut);
        }
        let text = crate::net::body_bounded(response, MAX_ANSWER_BYTES)
            .await
            .map(|body| String::from_utf8_lossy(&body).into_owned())
            .map_err(ChatError::Network)?;
        if !status.is_success() {
            let reason = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(ChatError::Server(format!("{path}: {status}: {reason}")));
        }
        serde_json::from_str(&text)
            .map_err(|e| ChatError::Server(format!("{path}: unexpected answer: {e}")))
    }

    /// Who the session belongs to; fails with [`ChatError::SignedOut`] when
    /// the server no longer accepts it.
    pub async fn me(&self) -> Result<Me, ChatError> {
        self.request(Method::GET, "me", &[], None).await
    }

    /// Whether the server has threads turned on. A public setting; a server
    /// that does not name it has them.
    pub async fn threads_enabled(&self) -> Result<bool, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            settings: Vec<Setting>,
        }
        #[derive(Deserialize)]
        struct Setting {
            #[serde(rename = "_id")]
            id: String,
            #[serde(default)]
            value: Value,
        }
        let answer: Answer = self
            .request(
                Method::GET,
                "settings.public",
                &[("_id", "Threads_enabled".to_string())],
                None,
            )
            .await?;
        Ok(answer
            .settings
            .iter()
            .find(|s| s.id == "Threads_enabled")
            .and_then(|s| s.value.as_bool())
            .unwrap_or(true))
    }

    /// The subscriptions changed since `since` (milliseconds), or all of them.
    pub async fn subscriptions(
        &self,
        since: Option<i64>,
    ) -> Result<Changes<WireSubscription>, ChatError> {
        let query: Vec<_> = since
            .map(|s| ("updatedSince", iso(s)))
            .into_iter()
            .collect();
        let answer: UpdateRemove<WireSubscription> = self
            .request(Method::GET, "subscriptions.get", &query, None)
            .await?;
        Ok(Changes {
            updated: answer.update,
            removed: removed_ids(answer.remove, "rid"),
        })
    }

    /// The rooms changed since `since` (milliseconds), or all of them.
    pub async fn rooms(&self, since: Option<i64>) -> Result<Changes<WireRoom>, ChatError> {
        let query: Vec<_> = since
            .map(|s| ("updatedSince", iso(s)))
            .into_iter()
            .collect();
        let answer: UpdateRemove<WireRoom> =
            self.request(Method::GET, "rooms.get", &query, None).await?;
        Ok(Changes {
            updated: answer.update,
            removed: removed_ids(answer.remove, "_id"),
        })
    }

    /// The messages of a room written, edited or deleted since `since`,
    /// thread replies included.
    pub async fn sync_messages(
        &self,
        rid: &str,
        since: i64,
    ) -> Result<Changes<WireMessage>, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            result: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            #[serde(default)]
            updated: Vec<WireMessage>,
            #[serde(default)]
            deleted: Vec<Value>,
        }
        let answer: Answer = self
            .request(
                Method::GET,
                "chat.syncMessages",
                &[("roomId", rid.to_string()), ("lastUpdate", iso(since))],
                None,
            )
            .await?;
        Ok(Changes {
            updated: answer.result.updated,
            removed: removed_ids(answer.result.deleted, "_id"),
        })
    }

    /// One message, by its id; how a thread's first message older than the
    /// sync's reach is fetched.
    pub async fn message(&self, id: &str) -> Result<WireMessage, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            message: WireMessage,
        }
        let answer: Answer = self
            .request(
                Method::GET,
                "chat.getMessage",
                &[("msgId", id.to_string())],
                None,
            )
            .await?;
        Ok(answer.message)
    }

    /// Up to `count` messages of a room's own timeline older than `before`,
    /// newest first. Each kind of room has its own endpoint.
    pub async fn history(
        &self,
        kind: &str,
        rid: &str,
        before: i64,
        count: i64,
    ) -> Result<Vec<WireMessage>, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            messages: Vec<WireMessage>,
        }
        let endpoint = match kind {
            "c" => "channels.history",
            "p" => "groups.history",
            _ => "im.history",
        };
        let answer: Answer = self
            .request(
                Method::GET,
                endpoint,
                &[
                    ("roomId", rid.to_string()),
                    ("latest", iso(before)),
                    ("count", count.to_string()),
                ],
                None,
            )
            .await?;
        Ok(answer.messages)
    }

    /// People and rooms whose name matches `term`, as the server's own search
    /// finds them: the rooms include public ones the user is not in.
    pub async fn spotlight(&self, term: &str) -> Result<Spotlight, ChatError> {
        self.request(
            Method::GET,
            "spotlight",
            &[("query", term.to_string())],
            None,
        )
        .await
    }

    /// The direct message room with `username`, made if there is none yet.
    pub async fn open_direct(&self, username: &str) -> Result<String, ChatError> {
        let answer: Value = self
            .request(
                Method::POST,
                "im.create",
                &[],
                Some(json!({ "username": username })),
            )
            .await?;
        // The room carries its id as `_id`, and on some versions as `rid` too.
        let room = &answer["room"];
        room.get("_id")
            .or_else(|| room.get("rid"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ChatError::Server("im.create: no room in the answer".into()))
    }

    /// The direct message room with everyone in `usernames` and the user,
    /// made if there is none yet; the server hands back the one that exists.
    pub async fn open_group(&self, usernames: &[String]) -> Result<String, ChatError> {
        let answer: Value = self
            .request(
                Method::POST,
                "im.create",
                &[],
                Some(json!({ "usernames": usernames.join(",") })),
            )
            .await?;
        let room = &answer["room"];
        room.get("_id")
            .or_else(|| room.get("rid"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ChatError::Server("im.create: no room in the answer".into()))
    }

    /// Join a public channel.
    pub async fn join(&self, rid: &str) -> Result<(), ChatError> {
        let _: Value = self
            .request(
                Method::POST,
                "channels.join",
                &[],
                Some(json!({ "roomId": rid })),
            )
            .await?;
        Ok(())
    }

    /// Everyone's status, by username: `online`, `away`, `busy` or `offline`.
    pub async fn presence(&self) -> Result<Vec<(String, String)>, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            users: Vec<User>,
        }
        #[derive(Deserialize)]
        struct User {
            #[serde(default)]
            username: Option<String>,
            #[serde(default)]
            status: Option<String>,
        }
        let answer: Answer = self
            .request(Method::GET, "users.presence", &[], None)
            .await?;
        Ok(answer
            .users
            .into_iter()
            .filter_map(|u| Some((u.username?, u.status.unwrap_or_else(|| "offline".into()))))
            .collect())
    }

    /// Leave a room; a direct message is hidden instead, as Rocket.Chat does.
    pub async fn leave(&self, kind: &str, rid: &str) -> Result<(), ChatError> {
        let endpoint = match kind {
            "c" => "channels.leave",
            "p" => "groups.leave",
            _ => "im.close",
        };
        let _: Value = self
            .request(Method::POST, endpoint, &[], Some(json!({ "roomId": rid })))
            .await?;
        Ok(())
    }

    /// Show a direct message hidden with [`RestClient::leave`] again.
    pub async fn reopen_direct(&self, rid: &str) -> Result<(), ChatError> {
        let _: Value = self
            .request(Method::POST, "im.open", &[], Some(json!({ "roomId": rid })))
            .await?;
        Ok(())
    }

    /// Create a channel, or a private group; returns its id.
    pub async fn create_room(&self, name: &str, private: bool) -> Result<String, ChatError> {
        let (endpoint, key) = if private {
            ("groups.create", "group")
        } else {
            ("channels.create", "channel")
        };
        let answer: Value = self
            .request(Method::POST, endpoint, &[], Some(json!({ "name": name })))
            .await?;
        answer[key]["_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ChatError::Server(format!("{endpoint}: no room in the answer")))
    }

    /// Replace the text of one of the user's messages.
    pub async fn update_message(&self, rid: &str, id: &str, text: &str) -> Result<(), ChatError> {
        let _: Value = self
            .request(
                Method::POST,
                "chat.update",
                &[],
                Some(json!({ "roomId": rid, "msgId": id, "text": text })),
            )
            .await?;
        Ok(())
    }

    /// Delete one of the user's messages.
    pub async fn delete_message(&self, rid: &str, id: &str) -> Result<(), ChatError> {
        let _: Value = self
            .request(
                Method::POST,
                "chat.delete",
                &[],
                Some(json!({ "roomId": rid, "msgId": id })),
            )
            .await?;
        Ok(())
    }

    /// Add the user's reaction `emoji` (a shortcode, `:+1:`) to a message, or
    /// take it back.
    pub async fn react(&self, id: &str, emoji: &str, on: bool) -> Result<(), ChatError> {
        let _: Value = self
            .request(
                Method::POST,
                "chat.react",
                &[],
                Some(json!({ "messageId": id, "emoji": emoji, "shouldReact": on })),
            )
            .await?;
        Ok(())
    }

    /// Upload a file to a room and post it, into the thread `tmid` if given,
    /// with `text` as its message: the two steps of `rooms.media`.
    pub async fn upload(
        &self,
        rid: &str,
        tmid: Option<&str>,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
        text: &str,
    ) -> Result<WireMessage, ChatError> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_string())
            .mime_str(mime)
            .map_err(|e| ChatError::Server(format!("{name}: {e}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let response = self
            .http
            .post(format!("{}/api/v1/rooms.media/{rid}", self.server))
            .header("X-User-Id", &self.session.user_id)
            .header("X-Auth-Token", self.session.token.expose_secret())
            // A file takes longer than a request: a minute and a half for a big one.
            .timeout(Duration::from_secs(90))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChatError::Network(e.to_string()))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ChatError::SignedOut);
        }
        let answer: Value = response
            .json()
            .await
            .map_err(|e| ChatError::Network(e.to_string()))?;
        if !status.is_success() {
            let reason = answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("upload failed");
            return Err(ChatError::Server(format!("{name}: {reason}")));
        }
        let file_id = answer["file"]["_id"]
            .as_str()
            .ok_or_else(|| ChatError::Server(format!("{name}: no file in the answer")))?
            .to_string();
        let mut confirm = json!({ "msg": text });
        if let Some(tmid) = tmid {
            confirm["tmid"] = json!(tmid);
        }
        #[derive(Deserialize)]
        struct Answer {
            message: WireMessage,
        }
        let answer: Answer = self
            .request(
                Method::POST,
                &format!("rooms.mediaConfirm/{rid}/{file_id}"),
                &[],
                Some(confirm),
            )
            .await?;
        Ok(answer.message)
    }

    /// Every reply of a thread, oldest first.
    pub async fn thread_messages(&self, tmid: &str) -> Result<Vec<WireMessage>, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            messages: Vec<WireMessage>,
        }
        let answer: Answer = self
            .request(
                Method::GET,
                "chat.getThreadMessages",
                &[
                    ("tmid", tmid.to_string()),
                    ("count", "0".to_string()),
                    ("sort", r#"{"ts":1}"#.to_string()),
                ],
                None,
            )
            .await?;
        Ok(answer.messages)
    }

    /// Send `text` to a room, into the thread `tmid` if given; with `tshow`
    /// a thread reply shows in the room as well.
    pub async fn send(
        &self,
        rid: &str,
        tmid: Option<&str>,
        text: &str,
        tshow: bool,
    ) -> Result<WireMessage, ChatError> {
        #[derive(Deserialize)]
        struct Answer {
            message: WireMessage,
        }
        let mut message = json!({ "rid": rid, "msg": text });
        if let Some(tmid) = tmid {
            message["tmid"] = json!(tmid);
            if tshow {
                message["tshow"] = json!(true);
            }
        }
        let answer: Answer = self
            .request(
                Method::POST,
                "chat.sendMessage",
                &[],
                Some(json!({ "message": message })),
            )
            .await?;
        Ok(answer.message)
    }

    /// Mark a room read on the server, as reading it there would.
    pub async fn mark_read(&self, rid: &str) -> Result<(), ChatError> {
        let _: Value = self
            .request(
                Method::POST,
                "subscriptions.read",
                &[],
                Some(json!({ "rid": rid })),
            )
            .await?;
        Ok(())
    }

    /// Mark a room unread on the server from `message` on, as Rocket.Chat's
    /// own Mark Unread does.
    pub async fn mark_unread(&self, message: &str) -> Result<(), ChatError> {
        let _: Value = self
            .request(
                Method::POST,
                "subscriptions.unread",
                &[],
                Some(json!({ "firstUnreadMessage": { "_id": message } })),
            )
            .await?;
        Ok(())
    }

    /// The bytes of a file the server keeps, by its path on the server
    /// (`/file-upload/…`).
    pub async fn download(&self, path: &str) -> Result<Vec<u8>, ChatError> {
        // The session goes to this server and nowhere else.
        let path = path.strip_prefix(self.server.as_str()).unwrap_or(path);
        if !path.starts_with('/') {
            return Err(ChatError::Server(format!(
                "not a file of the server: {path}"
            )));
        }
        let response = self
            .http
            .get(format!("{}{path}", self.server))
            .header("X-User-Id", &self.session.user_id)
            .header("X-Auth-Token", self.session.token.expose_secret())
            .send()
            .await
            .map_err(|e| ChatError::Network(e.to_string()))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ChatError::SignedOut);
        }
        if !status.is_success() {
            return Err(ChatError::Server(format!("{path}: {status}")));
        }
        crate::net::body_bounded(response, MAX_FILE_BYTES)
            .await
            .map_err(ChatError::Network)
    }

    /// Mark one thread read on the server. The REST API has no endpoint of its
    /// own for it, so the web client's method is called through `method.call`.
    pub async fn mark_thread_read(&self, tmid: &str) -> Result<(), ChatError> {
        let call = json!({
            "msg": "method",
            "id": "1",
            "method": "readThreads",
            "params": [tmid],
        });
        let answer: Value = self
            .request(
                Method::POST,
                "method.call/readThreads",
                &[],
                Some(json!({ "message": call.to_string() })),
            )
            .await?;
        // A failed method still answers 200, with the error inside.
        let inner = answer
            .get("message")
            .and_then(Value::as_str)
            .and_then(|m| serde_json::from_str::<Value>(m).ok());
        match inner.as_ref().and_then(|m| m.get("error")) {
            Some(error) => Err(ChatError::Server(format!("readThreads: {error}"))),
            None => Ok(()),
        }
    }
}
