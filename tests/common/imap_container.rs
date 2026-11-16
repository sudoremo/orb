//! Dovecot container wrapper for IMAP integration testing
//!
//! Provides a containerized Dovecot IMAP server with CONDSTORE support
//! for testing IMAP operations against a real server.
//!
//! Uses the official `dovecot/dovecot` image which is ARM64 native.

use async_imap::Session;
use async_native_tls::TlsStream;
use futures::TryStreamExt;
use std::time::Duration;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{CmdWaitFor, ContainerPort, ExecCommand, WaitFor},
    runners::AsyncRunner,
};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

// Official dovecot/dovecot image uses non-standard ports
const IMAP_PORT: u16 = 31143;
const IMAPS_PORT: u16 = 31993;

/// The password every account on the container authenticates with.
///
/// The image's passdb is static, so the username is irrelevant and only the
/// password is checked; it has to be handed to the container as an environment
/// variable and to the client as a credential, and one constant keeps the two
/// from drifting apart.
const TEST_PASSWORD: &str = "supersecret";

/// Type alias for IMAP session over TLS
pub type ImapSession = Session<TlsStream<Compat<TcpStream>>>;

/// Wrapper around a Dovecot container for IMAP testing
pub struct DovecotContainer {
    #[allow(dead_code)]
    container: ContainerAsync<GenericImage>,
    pub host: String,
    pub imap_port: u16,
    #[allow(dead_code)]
    pub imaps_port: u16,
}

impl DovecotContainer {
    /// Start a new Dovecot container with CONDSTORE support
    ///
    /// Uses the official `dovecot/dovecot` image which is ARM64 native
    /// and includes CONDSTORE, QRESYNC, and other modern IMAP extensions.
    pub async fn start() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let image = GenericImage::new("dovecot/dovecot", "latest")
            .with_exposed_port(ContainerPort::Tcp(IMAP_PORT))
            .with_exposed_port(ContainerPort::Tcp(IMAPS_PORT))
            .with_wait_for(WaitFor::message_on_stdout("Dovecot"))
            // The image's `passdb static` reads its password from the
            // environment and defaults to `{CRYPT}*`, which matches nothing.
            // Without this every LOGIN fails with AUTHENTICATIONFAILED and
            // every container-backed suite dies at `start()` rather than at an
            // assertion — a failure mode that looks like a broken test and is
            // actually a missing environment variable.
            .with_env_var("USER_PASSWORD", TEST_PASSWORD)
            .with_startup_timeout(Duration::from_secs(60));

        let container = image.start().await?;

        let host = container.get_host().await?.to_string();
        let imap_port = container.get_host_port_ipv4(IMAP_PORT).await?;
        let imaps_port = container.get_host_port_ipv4(IMAPS_PORT).await?;

        let dovecot = Self {
            container,
            host,
            imap_port,
            imaps_port,
        };

        // Verify CONDSTORE support - this is a critical requirement for orb.
        // Retried, because the wait strategy above is satisfied by the first
        // line Dovecot logs ("... starting up") and the IMAP listener binds a
        // moment after that. A single attempt therefore succeeds on an idle
        // machine and fails with ECONNREFUSED whenever several containers start
        // at once — which is exactly what `--test-threads` does, so the
        // flakiness only appears when the suite is run the way CI runs it.
        dovecot.wait_until_ready().await?;

        Ok(dovecot)
    }

    /// Poll until the server answers, then confirm it is one orb can use.
    async fn wait_until_ready(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self.verify_condstore_support().await {
                Ok(()) => return Ok(()),
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    /// Verify the container supports CONDSTORE (required by orb)
    async fn verify_condstore_support(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;

        let caps = session.capabilities().await?;
        let has_condstore = caps.iter().any(|c| {
            if let async_imap::types::Capability::Atom(name) = c {
                name.to_uppercase() == "CONDSTORE"
            } else {
                false
            }
        });

        session.logout().await?;

        if !has_condstore {
            return Err("Container does not support CONDSTORE - required by orb".into());
        }

        Ok(())
    }

    /// Get test user credentials (default for dovecot/dovecot image)
    pub fn credentials() -> (&'static str, &'static str) {
        ("testuser", TEST_PASSWORD)
    }

    /// Connect to IMAP server over TLS
    /// Note: The official dovecot/dovecot image requires TLS for login
    pub async fn connect_plain(
        &self,
    ) -> Result<Session<TlsStream<Compat<TcpStream>>>, Box<dyn std::error::Error + Send + Sync>>
    {
        let addr = format!("{}:{}", self.host, self.imaps_port);
        let tcp_stream = TcpStream::connect(&addr).await?;

        // Accept self-signed certificates for testing
        let tls = async_native_tls::TlsConnector::new()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true);

        let tls_stream = tls.connect(&self.host, tcp_stream.compat()).await?;
        let client = async_imap::Client::new(tls_stream);

        let (user, pass) = Self::credentials();
        let session = client.login(user, pass).await.map_err(|e| e.0)?;

        Ok(session)
    }

    /// Prepopulate mailbox with test messages via IMAP APPEND
    pub async fn prepopulate_mailbox(
        &self,
        folder: &str,
        messages: &[Vec<u8>],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;

        // Create folder if not INBOX
        if folder != "INBOX" {
            // Ignore error if folder already exists
            let _ = session.create(folder).await;
        }

        for msg in messages {
            session.append(folder, None, None, msg).await?;
        }

        session.logout().await?;
        Ok(())
    }

    /// Write `count` messages straight into the user's INBOX maildir.
    ///
    /// APPEND over IMAP manages about sixty messages a second against the
    /// container, which puts a hundred-thousand-message folder half an hour
    /// away. Writing the files is what delivery does; Dovecot indexes them on
    /// the next SELECT. The image has no coreutils, so the script is shell
    /// builtins and `doveadm` only.
    pub async fn populate_inbox_maildir(
        &self,
        count: usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let script = format!(
            "set -e; \
             doveadm mailbox create -u testuser INBOX >/dev/null 2>&1 || true; \
             p=$(doveadm mailbox path -u testuser INBOX); \
             i=0; while [ $i -lt {count} ]; do \
               printf 'From: sender@example.com\r\nTo: testuser@example.com\r\n\
Subject: Scale message %d\r\nDate: Tue, 14 Oct 2026 10:00:00 +0000\r\n\
Message-ID: <scale-%d@test.local>\r\nMIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\n\
A short body. The envelope is what the sync fetches; the body only has to exist.\r\n' \
               $i $i > \"$p/new/1760000000.$i.scale\"; \
               i=$((i+1)); \
             done"
        );
        // Waiting for the exit is explicit: without it `exec` returns while
        // the script is still running and the exit code is not yet known.
        let mut result = self
            .container
            .exec(
                ExecCommand::new(["sh", "-c", &script])
                    .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
            )
            .await?;
        let code = result.exit_code().await?;
        if code != Some(0) {
            let stderr = result.stderr_to_vec().await.unwrap_or_default();
            return Err(format!(
                "populating the maildir failed with {code:?}: {}",
                String::from_utf8_lossy(&stderr)
            )
            .into());
        }
        Ok(())
    }

    /// Create a folder on the server
    pub async fn create_folder(
        &self,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        session.create(name).await?;
        session.logout().await?;
        Ok(())
    }

    /// Get message count in a folder
    pub async fn get_message_count(
        &self,
        folder: &str,
    ) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        let mailbox = session.select(folder).await?;
        let count = mailbox.exists;
        session.logout().await?;
        Ok(count)
    }

    /// List all folders on the server
    pub async fn list_folders(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        let list_stream = session.list(Some(""), Some("*")).await?;
        let names: Vec<_> = list_stream.try_collect().await?;
        let folders: Vec<String> = names.iter().map(|n| n.name().to_string()).collect();
        session.logout().await?;
        Ok(folders)
    }

    /// Get capabilities from the server
    pub async fn get_capabilities(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        let caps = session.capabilities().await?;
        let cap_strings: Vec<String> = caps
            .iter()
            .map(|c| match c {
                async_imap::types::Capability::Imap4rev1 => "IMAP4rev1".to_string(),
                async_imap::types::Capability::Atom(name) => name.to_string(),
                async_imap::types::Capability::Auth(name) => format!("AUTH={}", name),
            })
            .collect();
        session.logout().await?;
        Ok(cap_strings)
    }

    /// Set flags on a message by sequence number
    pub async fn store_flags(
        &self,
        folder: &str,
        sequence: &str,
        flags: &str,
        add: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        session.select(folder).await?;

        let flag_cmd = if add {
            format!("+FLAGS ({})", flags)
        } else {
            format!("-FLAGS ({})", flags)
        };

        let store_stream = session.store(sequence, &flag_cmd).await?;
        // Consume the stream to complete the operation
        let _: Vec<_> = store_stream.try_collect().await?;
        session.logout().await?;
        Ok(())
    }

    /// The flags of a sequence range, in order, as the server has them.
    pub async fn fetch_flags(
        &self,
        folder: &str,
        sequence: &str,
    ) -> Result<Vec<Vec<String>>, Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        session.select(folder).await?;
        let stream = session.fetch(sequence, "(FLAGS)").await?;
        let fetched: Vec<_> = stream.try_collect().await?;
        let flags = fetched
            .iter()
            .map(|fetch| {
                fetch
                    .flags()
                    .map(|flag| format!("{flag:?}"))
                    .collect::<Vec<_>>()
            })
            .collect();
        session.logout().await?;
        Ok(flags)
    }

    /// Delete messages (set \Deleted flag) and expunge
    pub async fn delete_and_expunge(
        &self,
        folder: &str,
        sequence: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut session = self.connect_plain().await?;
        session.select(folder).await?;
        let store_stream = session.store(sequence, "+FLAGS (\\Deleted)").await?;
        let _: Vec<_> = store_stream.try_collect().await?;
        let expunge_stream = session.expunge().await?;
        let _: Vec<_> = expunge_stream.try_collect().await?;
        session.logout().await?;
        Ok(())
    }
}

/// Create a simple RFC822 message for testing
pub fn create_rfc822_message(from: &str, to: &str, subject: &str, body: &str) -> Vec<u8> {
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000");
    let message_id = format!(
        "<{}.{}@test.local>",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        rand::random::<u32>()
    );

    format!(
        "From: {}\r\n\
         To: {}\r\n\
         Subject: {}\r\n\
         Date: {}\r\n\
         Message-ID: {}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {}",
        from, to, subject, date, message_id, body
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_container_starts() {
        let container = DovecotContainer::start()
            .await
            .expect("Failed to start container");
        assert!(!container.host.is_empty());
        assert!(container.imap_port > 0);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_container_has_condstore() {
        let container = DovecotContainer::start()
            .await
            .expect("Failed to start container");
        let caps = container
            .get_capabilities()
            .await
            .expect("Failed to get capabilities");
        assert!(
            caps.iter().any(|c| c.to_uppercase() == "CONDSTORE"),
            "CONDSTORE not found in capabilities: {:?}",
            caps
        );
    }
}
