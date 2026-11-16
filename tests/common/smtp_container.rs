//! MailHog container wrapper for SMTP integration testing
//!
//! Provides a containerized MailHog SMTP server for testing
//! email sending without actually delivering to real mailboxes.
//! MailHog captures all emails and provides a REST API to query them.

use serde::Deserialize;
use std::time::Duration;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{ContainerPort, WaitFor},
    runners::AsyncRunner,
};

const SMTP_PORT: u16 = 1025;
const HTTP_PORT: u16 = 8025;

/// Wrapper around a MailHog container for SMTP testing
pub struct MailHogContainer {
    #[allow(dead_code)]
    container: ContainerAsync<GenericImage>,
    pub host: String,
    pub smtp_port: u16,
    pub http_port: u16,
}

/// Message from MailHog API
#[derive(Debug, Deserialize)]
pub struct MailHogMessage {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "From")]
    pub from: MailHogAddress,
    #[serde(rename = "To")]
    pub to: Vec<MailHogAddress>,
    #[serde(rename = "Content")]
    pub content: MailHogContent,
}

#[derive(Debug, Deserialize)]
pub struct MailHogAddress {
    #[serde(rename = "Relays")]
    pub relays: Option<Vec<String>>,
    #[serde(rename = "Mailbox")]
    pub mailbox: String,
    #[serde(rename = "Domain")]
    pub domain: String,
    #[serde(rename = "Params")]
    pub params: String,
}

impl MailHogAddress {
    pub fn email(&self) -> String {
        format!("{}@{}", self.mailbox, self.domain)
    }
}

#[derive(Debug, Deserialize)]
pub struct MailHogContent {
    #[serde(rename = "Headers")]
    pub headers: MailHogHeaders,
    #[serde(rename = "Body")]
    pub body: String,
    #[serde(rename = "Size")]
    pub size: i64,
}

#[derive(Debug, Deserialize)]
pub struct MailHogHeaders {
    #[serde(rename = "Subject")]
    pub subject: Option<Vec<String>>,
    #[serde(rename = "From")]
    pub from: Option<Vec<String>>,
    #[serde(rename = "To")]
    pub to: Option<Vec<String>>,
    #[serde(rename = "Cc")]
    pub cc: Option<Vec<String>>,
    #[serde(rename = "Content-Type")]
    pub content_type: Option<Vec<String>>,
    #[serde(rename = "In-Reply-To")]
    pub in_reply_to: Option<Vec<String>>,
    #[serde(rename = "References")]
    pub references: Option<Vec<String>>,
}

/// MailHog API response for listing messages
#[derive(Debug, Deserialize)]
pub struct MailHogMessages {
    pub total: i64,
    pub count: i64,
    pub start: i64,
    pub items: Vec<MailHogMessage>,
}

impl MailHogContainer {
    /// Start a new MailHog container
    pub async fn start() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let image = GenericImage::new("mailhog/mailhog", "latest")
            .with_exposed_port(ContainerPort::Tcp(SMTP_PORT))
            .with_exposed_port(ContainerPort::Tcp(HTTP_PORT))
            .with_wait_for(WaitFor::message_on_stdout("Creating API v2"))
            .with_startup_timeout(Duration::from_secs(60));

        let container = image.start().await?;

        let host = container.get_host().await?.to_string();
        let smtp_port = container.get_host_port_ipv4(SMTP_PORT).await?;
        let http_port = container.get_host_port_ipv4(HTTP_PORT).await?;

        let mailhog = Self {
            container,
            host,
            smtp_port,
            http_port,
        };

        // The wait strategy above is satisfied by a line MailHog logs while it
        // is still setting up, and the HTTP listener binds a moment later. On an
        // idle machine the gap is invisible; when several containers start at
        // once — which is what `--test-threads` does — a test can send its mail
        // successfully and then time out waiting for an API that is not up yet.
        mailhog.wait_until_ready().await?;

        Ok(mailhog)
    }

    /// Poll the API until it answers, so a caller's timeout is spent waiting for
    /// the *message* rather than for the server.
    async fn wait_until_ready(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self.get_messages().await {
                Ok(_) => return Ok(()),
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    /// Get the SMTP connection string
    pub fn smtp_addr(&self) -> String {
        format!("{}:{}", self.host, self.smtp_port)
    }

    /// Get the HTTP API base URL
    pub fn api_url(&self) -> String {
        format!("http://{}:{}", self.host, self.http_port)
    }

    /// Get all messages from MailHog
    pub async fn get_messages(
        &self,
    ) -> Result<MailHogMessages, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/v2/messages", self.api_url());
        let response = reqwest::get(&url).await?;
        let messages: MailHogMessages = response.json().await?;
        Ok(messages)
    }

    /// Get a specific message by ID
    pub async fn get_message(
        &self,
        id: &str,
    ) -> Result<MailHogMessage, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/v1/messages/{}", self.api_url(), id);
        let response = reqwest::get(&url).await?;
        let message: MailHogMessage = response.json().await?;
        Ok(message)
    }

    /// Delete all messages
    pub async fn delete_all_messages(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/v1/messages", self.api_url());
        let client = reqwest::Client::new();
        client.delete(&url).send().await?;
        Ok(())
    }

    /// Wait for a message to arrive (with timeout)
    pub async fn wait_for_message(
        &self,
        timeout: Duration,
    ) -> Result<MailHogMessage, Box<dyn std::error::Error + Send + Sync>> {
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(100);

        while start.elapsed() < timeout {
            let messages = self.get_messages().await?;
            if !messages.items.is_empty() {
                return Ok(messages.items.into_iter().next().unwrap());
            }
            tokio::time::sleep(poll_interval).await;
        }

        Err("Timeout waiting for message".into())
    }

    /// Send a test email using lettre directly (for testing SMTP connection)
    pub async fn send_test_email(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use lettre::message::Message;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let email = Message::builder()
            .from(from.parse()?)
            .to(to.parse()?)
            .subject(subject)
            .body(body.to_string())?;

        // MailHog doesn't require authentication and uses plain SMTP
        let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host)
            .port(self.smtp_port)
            .build();

        mailer.send(email).await?;
        Ok(())
    }

    /// Send a test email with Cc recipients
    pub async fn send_test_email_with_cc(
        &self,
        from: &str,
        to: &[&str],
        cc: &[&str],
        subject: &str,
        body: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use lettre::message::Message;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let mut builder = Message::builder().from(from.parse()?).subject(subject);

        for addr in to {
            builder = builder.to(addr.parse()?);
        }

        for addr in cc {
            builder = builder.cc(addr.parse()?);
        }

        let email = builder.body(body.to_string())?;

        let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host)
            .port(self.smtp_port)
            .build();

        mailer.send(email).await?;
        Ok(())
    }

    /// Send an email with In-Reply-To and References headers (for threading)
    pub async fn send_reply_email(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
        in_reply_to: &str,
        references: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use lettre::message::Message;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let mut builder = Message::builder()
            .from(from.parse()?)
            .to(to.parse()?)
            .subject(subject)
            .in_reply_to(in_reply_to.to_string());

        if let Some(refs) = references {
            builder = builder.references(refs.to_string());
        }

        let email = builder.body(body.to_string())?;

        let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host)
            .port(self.smtp_port)
            .build();

        mailer.send(email).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_container_starts() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");
        assert!(!container.host.is_empty());
        assert!(container.smtp_port > 0);
        assert!(container.http_port > 0);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_send_and_receive() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        container
            .send_test_email(
                "sender@test.local",
                "recipient@test.local",
                "Test Subject",
                "Test body content",
            )
            .await
            .expect("Failed to send email");

        let message = container
            .wait_for_message(Duration::from_secs(5))
            .await
            .expect("Failed to receive message");

        assert_eq!(message.from.email(), "sender@test.local");
        assert_eq!(message.to[0].email(), "recipient@test.local");
        assert!(
            message
                .content
                .headers
                .subject
                .as_ref()
                .map(|s| s.contains(&"Test Subject".to_string()))
                .unwrap_or(false)
        );
    }
}
