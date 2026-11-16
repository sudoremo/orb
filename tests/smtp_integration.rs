//! SMTP integration tests using MailHog container
//!
//! These tests require Docker to be running and are marked with `#[ignore]`
//! to avoid running them during regular `cargo test` runs.
//!
//! Run with: `cargo test --test smtp_integration -- --ignored`

mod common;

use common::smtp_container::MailHogContainer;
use orb_core::smtp::{OutgoingEmail, SmtpError};
use std::time::Duration;

// ============================================================================
// Container Tests
// ============================================================================

mod container_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_container_starts_successfully() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        assert!(!container.host.is_empty(), "Host should not be empty");
        assert!(container.smtp_port > 0, "SMTP port should be mapped");
        assert!(container.http_port > 0, "HTTP port should be mapped");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_container_api_responds() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        // API should return empty messages list
        let messages = container
            .get_messages()
            .await
            .expect("Failed to get messages");

        assert_eq!(messages.total, 0, "Should have no messages initially");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_smtp_connection() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        // Sending a test email should work
        container
            .send_test_email(
                "test@example.com",
                "recipient@example.com",
                "Connection Test",
                "Test body",
            )
            .await
            .expect("Failed to send test email");

        // Verify message was received
        let message = container
            .wait_for_message(Duration::from_secs(5))
            .await
            .expect("Failed to receive message");

        assert_eq!(message.from.email(), "test@example.com");
    }
}

// ============================================================================
// Email Sending Tests
// ============================================================================

mod email_sending_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_send_simple_email() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        container
            .send_test_email(
                "sender@test.local",
                "recipient@test.local",
                "Simple Email Test",
                "This is a simple test email body.",
            )
            .await
            .expect("Failed to send email");

        let message = container
            .wait_for_message(Duration::from_secs(5))
            .await
            .expect("Failed to receive message");

        // Verify From
        assert_eq!(message.from.email(), "sender@test.local");

        // Verify To
        assert_eq!(message.to.len(), 1);
        assert_eq!(message.to[0].email(), "recipient@test.local");

        // Verify Subject
        let subject = message.content.headers.subject.as_ref();
        assert!(subject.is_some(), "Subject header should exist");
        assert!(
            subject
                .unwrap()
                .iter()
                .any(|s| s.contains("Simple Email Test")),
            "Subject should match"
        );

        // Verify Body
        assert!(
            message.content.body.contains("simple test email body"),
            "Body should contain expected text"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_send_email_with_cc() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        container
            .send_test_email_with_cc(
                "sender@test.local",
                &["to1@test.local", "to2@test.local"],
                &["cc1@test.local", "cc2@test.local"],
                "Email with CC",
                "Body with multiple recipients",
            )
            .await
            .expect("Failed to send email");

        let message = container
            .wait_for_message(Duration::from_secs(5))
            .await
            .expect("Failed to receive message");

        // Verify To recipients (MailHog combines all recipients)
        let to_emails: Vec<String> = message.to.iter().map(|a| a.email()).collect();
        assert!(to_emails.contains(&"to1@test.local".to_string()));
        assert!(to_emails.contains(&"to2@test.local".to_string()));

        // Note: MailHog may show CC addresses in the To field as it's all envelope recipients
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_send_reply_email() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        let original_message_id = "<original-123@test.local>";

        container
            .send_reply_email(
                "replier@test.local",
                "original-sender@test.local",
                "Re: Original Subject",
                "This is a reply to your email.",
                original_message_id,
                Some(original_message_id),
            )
            .await
            .expect("Failed to send reply");

        let message = container
            .wait_for_message(Duration::from_secs(5))
            .await
            .expect("Failed to receive message");

        // Verify threading headers
        let in_reply_to = message.content.headers.in_reply_to.as_ref();
        assert!(in_reply_to.is_some(), "In-Reply-To header should exist");
        assert!(
            in_reply_to
                .unwrap()
                .iter()
                .any(|s| s.contains("original-123")),
            "In-Reply-To should contain original message ID"
        );

        let references = message.content.headers.references.as_ref();
        assert!(references.is_some(), "References header should exist");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_send_multiple_emails() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        // Send 5 emails
        for i in 1..=5 {
            container
                .send_test_email(
                    &format!("sender{}@test.local", i),
                    "recipient@test.local",
                    &format!("Email Number {}", i),
                    &format!("Body of email {}", i),
                )
                .await
                .expect("Failed to send email");
        }

        // Wait a bit for all to be processed
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Check all were received
        let messages = container
            .get_messages()
            .await
            .expect("Failed to get messages");

        assert_eq!(messages.total, 5, "Should have received 5 emails");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn test_delete_all_messages() {
        let container = MailHogContainer::start()
            .await
            .expect("Failed to start container");

        // Send some emails
        for _ in 0..3 {
            container
                .send_test_email(
                    "sender@test.local",
                    "recipient@test.local",
                    "To Be Deleted",
                    "This will be deleted",
                )
                .await
                .expect("Failed to send email");
        }

        // Wait for messages
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Verify they exist
        let messages = container
            .get_messages()
            .await
            .expect("Failed to get messages");
        assert_eq!(messages.total, 3);

        // Delete all
        container
            .delete_all_messages()
            .await
            .expect("Failed to delete messages");

        // Verify empty
        let messages = container
            .get_messages()
            .await
            .expect("Failed to get messages");
        assert_eq!(messages.total, 0, "Should have no messages after deletion");
    }
}

// ============================================================================
// OutgoingEmail Validation Tests (Unit tests, no Docker)
// ============================================================================

mod validation_tests {
    use super::*;

    #[test]
    fn test_valid_email() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body content".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_valid_email_with_cc() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec!["cc1@example.com".to_string(), "cc2@example.com".to_string()],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body content".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_valid_reply_email() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Re: Original Subject".to_string(),
            body: "Reply content".to_string(),
            in_reply_to: Some("<msg-123@example.com>".to_string()),
            references: Some("<msg-123@example.com>".to_string()),
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_empty_recipients() {
        let email = OutgoingEmail {
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
        if let Err(SmtpError::InvalidAddress(msg)) = result {
            assert!(msg.contains("No recipients"));
        } else {
            panic!("Expected InvalidAddress error");
        }
    }

    #[test]
    fn test_invalid_to_address() {
        let email = OutgoingEmail {
            to: vec!["invalid-email".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
        if let Err(SmtpError::InvalidAddress(msg)) = result {
            assert!(msg.contains("Invalid To address"));
        } else {
            panic!("Expected InvalidAddress error");
        }
    }

    #[test]
    fn test_invalid_cc_address() {
        let email = OutgoingEmail {
            to: vec!["valid@example.com".to_string()],
            cc: vec!["invalid-cc".to_string()],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
        if let Err(SmtpError::InvalidAddress(msg)) = result {
            assert!(msg.contains("Invalid Cc address"));
        } else {
            panic!("Expected InvalidAddress error");
        }
    }

    #[test]
    fn test_empty_subject() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "".to_string(),
            body: "Test body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
        if let Err(SmtpError::BuildError(msg)) = result {
            assert!(msg.contains("Subject is required"));
        } else {
            panic!("Expected BuildError error");
        }
    }

    #[test]
    fn test_empty_body() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
        if let Err(SmtpError::BuildError(msg)) = result {
            assert!(msg.contains("Message body is required"));
        } else {
            panic!("Expected BuildError error");
        }
    }

    #[test]
    fn test_whitespace_only_body() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "   \n\t  ".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_multiple_to_addresses() {
        let email = OutgoingEmail {
            to: vec![
                "recipient1@example.com".to_string(),
                "recipient2@example.com".to_string(),
                "recipient3@example.com".to_string(),
            ],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_mixed_valid_invalid_addresses() {
        let email = OutgoingEmail {
            to: vec!["valid@example.com".to_string(), "invalid".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            body: "Test body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        let result = email.validate();
        assert!(result.is_err());
    }
}

// ============================================================================
// Email Address Validation Tests (Unit tests, no Docker)
// ============================================================================

mod address_validation_tests {
    // These test the internal is_valid_email_address function via OutgoingEmail

    use super::*;

    fn validate_address(addr: &str) -> Result<(), SmtpError> {
        let email = OutgoingEmail {
            to: vec![addr.to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test".to_string(),
            body: "Body".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };
        email.validate()
    }

    #[test]
    fn test_valid_addresses() {
        let valid = vec![
            "user@example.com",
            "user.name@example.com",
            "user@sub.example.com",
            "user+tag@example.com",
            "user123@example.com",
        ];

        for addr in valid {
            assert!(validate_address(addr).is_ok(), "Should be valid: {}", addr);
        }
    }

    #[test]
    fn test_invalid_addresses() {
        let invalid = vec![
            "",
            "user",
            "user@",
            "@example.com",
            "user@example",      // no dot in domain
            "user@@example.com", // double @
            "user@.example.com", // leading dot
            "user@example.com.", // trailing dot
            "user@example..com", // consecutive dots
        ];

        for addr in invalid {
            assert!(
                validate_address(addr).is_err(),
                "Should be invalid: {}",
                addr
            );
        }
    }
}

// ============================================================================
// SmtpError Tests (Unit tests, no Docker)
// ============================================================================

mod error_tests {
    use super::*;

    #[test]
    fn test_smtp_error_display() {
        let errors = vec![
            (
                SmtpError::InvalidAddress("bad@".to_string()),
                "Invalid email address: bad@",
            ),
            (
                SmtpError::BuildError("missing field".to_string()),
                "Failed to build message: missing field",
            ),
            (
                SmtpError::SendError("connection refused".to_string()),
                "Failed to send email: connection refused",
            ),
            (
                SmtpError::ConfigError("no password".to_string()),
                "Configuration error: no password",
            ),
        ];

        for (error, expected) in errors {
            assert_eq!(error.to_string(), expected);
        }
    }
}

// ============================================================================
// Message Content Tests (Unit tests, no Docker)
// ============================================================================

mod message_content_tests {
    use super::*;

    #[test]
    fn test_email_with_special_characters_in_subject() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test: Special chars äöü éàè 日本語".to_string(),
            body: "Body content".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_email_with_multiline_body() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Multiline Test".to_string(),
            body: "Line 1\nLine 2\nLine 3\n\nParagraph 2".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_email_with_unicode_body() {
        let email = OutgoingEmail {
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Unicode Test".to_string(),
            body: "Hello 世界! Привет мир! مرحبا بالعالم".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: Vec::new(),
            message_id: None,
        };

        assert!(email.validate().is_ok());
    }
}
