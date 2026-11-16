//! A Rocket.Chat server for the chat suite: MongoDB as a one-member replica
//! set, which Rocket.Chat requires, and Rocket.Chat itself, on a network of
//! their own.
//!
//! `ORB_TEST_ROCKETCHAT=http://127.0.0.1:3900` uses a server that is already
//! running instead, which saves the
//! minute Rocket.Chat takes to start. Either way the server must have
//! two-factor authentication off and an admin `orbadmin` / `orb-admin-pass`.

use std::time::Duration;

use serde_json::{Value, json};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{CmdWaitFor, ContainerPort, ExecCommand, WaitFor},
    runners::AsyncRunner,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const ADMIN: (&str, &str) = ("orbadmin", "orb-admin-pass");

pub struct RocketChat {
    _containers: Option<(ContainerAsync<GenericImage>, ContainerAsync<GenericImage>)>,
    pub url: String,
    http: reqwest::Client,
}

/// A user signed in through the REST API.
#[derive(Clone, Debug)]
pub struct TestUser {
    pub id: String,
    pub username: String,
    pub token: String,
}

impl RocketChat {
    pub async fn start() -> Result<Self, BoxError> {
        let http = reqwest::Client::new();
        if let Ok(url) = std::env::var("ORB_TEST_ROCKETCHAT") {
            return Ok(Self {
                _containers: None,
                url: url.trim_end_matches('/').to_string(),
                http,
            });
        }

        let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let network = format!("orb-rc-{suffix}");
        let mongo_name = format!("orb-mongo-{suffix}");
        let mongo = GenericImage::new("mongo", "8.0")
            .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
            .with_cmd(["--replSet", "rs0", "--bind_ip_all"])
            .with_network(&network)
            .with_container_name(&mongo_name)
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;
        let initiate =
            format!("rs.initiate({{_id:'rs0',members:[{{_id:0,host:'{mongo_name}:27017'}}]}})");
        mongo
            .exec(
                ExecCommand::new(["mongosh", "--quiet", "--eval", &initiate])
                    .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
            )
            .await?;

        let rocketchat = GenericImage::new("rocketchat/rocket.chat", "8.9.0")
            .with_exposed_port(ContainerPort::Tcp(3000))
            .with_wait_for(WaitFor::message_on_stdout("SERVER RUNNING"))
            .with_network(&network)
            .with_env_var(
                "MONGO_URL",
                format!("mongodb://{mongo_name}:27017/rocketchat?replicaSet=rs0"),
            )
            .with_env_var("ROOT_URL", "http://localhost:3000")
            .with_env_var("PORT", "3000")
            .with_env_var("ADMIN_USERNAME", ADMIN.0)
            .with_env_var("ADMIN_PASS", ADMIN.1)
            .with_env_var("ADMIN_EMAIL", "admin@orb.test")
            .with_env_var("OVERWRITE_SETTING_Show_Setup_Wizard", "completed")
            .with_env_var("OVERWRITE_SETTING_API_Enable_Rate_Limiter", "false")
            .with_env_var(
                "OVERWRITE_SETTING_Accounts_TwoFactorAuthentication_Enabled",
                "false",
            )
            .with_startup_timeout(Duration::from_secs(240))
            .start()
            .await?;
        let port = rocketchat.get_host_port_ipv4(3000).await?;
        let url = format!("http://127.0.0.1:{port}");
        let server = Self {
            _containers: Some((mongo, rocketchat)),
            url,
            http,
        };
        server.wait_until_ready().await?;
        Ok(server)
    }

    /// "SERVER RUNNING" is logged before the port answers, and the admin
    /// account is made a moment after that: wait until the admin can sign in.
    async fn wait_until_ready(&self) -> Result<(), BoxError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            match self.login(ADMIN.0, ADMIN.1).await {
                Ok(admin) if !admin.token.is_empty() => return Ok(()),
                Ok(_) | Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Ok(_) => return Err("the admin account never appeared".into()),
                Err(error) => return Err(error),
            }
        }
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        auth: Option<&TestUser>,
    ) -> Result<Value, BoxError> {
        let mut request = self
            .http
            .request(method, format!("{}/api/v1/{path}", self.url));
        if let Some(user) = auth {
            request = request
                .header("X-User-Id", &user.id)
                .header("X-Auth-Token", &user.token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let value: Value = response.json().await?;
        if !status.is_success() {
            return Err(format!("{path}: {status}: {value}").into());
        }
        Ok(value)
    }

    async fn login(&self, username: &str, password: &str) -> Result<TestUser, BoxError> {
        let answer = self
            .call(
                reqwest::Method::POST,
                "login",
                Some(json!({ "user": username, "password": password })),
                None,
            )
            .await?;
        Ok(TestUser {
            id: answer["data"]["userId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            username: username.to_string(),
            token: answer["data"]["authToken"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// A new user with a name unique to the run, signed in.
    pub async fn user(&self, name: &str) -> Result<TestUser, BoxError> {
        let admin = self.login(ADMIN.0, ADMIN.1).await?;
        let username = format!("{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
        let password = format!("{username}-pass");
        self.call(
            reqwest::Method::POST,
            "users.create",
            Some(json!({
                "username": username, "email": format!("{username}@orb.test"),
                "password": password, "name": name, "verified": true,
            })),
            Some(&admin),
        )
        .await?;
        self.login(&username, &password).await
    }

    /// A channel with these members, created by the first.
    pub async fn channel(&self, members: &[&TestUser]) -> Result<String, BoxError> {
        let name = format!("room-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let usernames: Vec<&str> = members.iter().map(|u| u.username.as_str()).collect();
        let answer = self
            .call(
                reqwest::Method::POST,
                "channels.create",
                Some(json!({ "name": name, "members": usernames })),
                Some(members[0]),
            )
            .await?;
        Ok(answer["channel"]["_id"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// A direct message room between two users.
    pub async fn direct(&self, from: &TestUser, to: &TestUser) -> Result<String, BoxError> {
        let answer = self
            .call(
                reqwest::Method::POST,
                "im.create",
                Some(json!({ "username": to.username })),
                Some(from),
            )
            .await?;
        Ok(answer["room"]["_id"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Send a message as `user`; returns its id.
    pub async fn send(
        &self,
        user: &TestUser,
        rid: &str,
        tmid: Option<&str>,
        text: &str,
    ) -> Result<String, BoxError> {
        let mut message = json!({ "rid": rid, "msg": text });
        if let Some(tmid) = tmid {
            message["tmid"] = json!(tmid);
        }
        let answer = self
            .call(
                reqwest::Method::POST,
                "chat.sendMessage",
                Some(json!({ "message": message })),
                Some(user),
            )
            .await?;
        Ok(answer["message"]["_id"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// React to a message as `user` with `emoji`, a shortcode (`:+1:`).
    pub async fn react(&self, user: &TestUser, message: &str, emoji: &str) -> Result<(), BoxError> {
        self.call(
            reqwest::Method::POST,
            "chat.react",
            Some(json!({ "messageId": message, "emoji": emoji, "shouldReact": true })),
            Some(user),
        )
        .await?;
        Ok(())
    }

    /// Set `user`'s status: `online`, `away`, `busy` or `offline`.
    pub async fn set_status(&self, user: &TestUser, status: &str) -> Result<(), BoxError> {
        self.call(
            reqwest::Method::POST,
            "users.setStatus",
            Some(json!({ "status": status, "message": "" })),
            Some(user),
        )
        .await?;
        Ok(())
    }

    /// Set `user`'s desktop notifications for a room: `all`, `mentions`,
    /// `nothing` or `default`.
    pub async fn set_notifications(
        &self,
        user: &TestUser,
        rid: &str,
        desktop: &str,
    ) -> Result<(), BoxError> {
        self.call(
            reqwest::Method::POST,
            "rooms.saveNotification",
            Some(json!({ "roomId": rid, "notifications": { "desktopNotifications": desktop } })),
            Some(user),
        )
        .await?;
        Ok(())
    }

    /// Whether the server shows the room as unread to `user`: its `alert`,
    /// which a channel sets even where it counts only mentions as unread.
    pub async fn unread(&self, user: &TestUser, rid: &str) -> Result<bool, BoxError> {
        let answer = self
            .call(
                reqwest::Method::GET,
                &format!("subscriptions.getOne?roomId={rid}"),
                None,
                Some(user),
            )
            .await?;
        Ok(answer["subscription"]["alert"].as_bool().unwrap_or(false))
    }
}
