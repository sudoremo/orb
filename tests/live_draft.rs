//! Saving a draft against the real server.
//!
//! Ignored by default: it needs the machine's own `~/.config/orb/config.yml`
//! and it writes to a real Drafts folder. Run with
//! `cargo test --test live_draft -- --ignored --nocapture`.
//!
//! What it checks is the one thing a unit test cannot: that a server accepts
//! the bytes. A draft's headers are written by hand rather than by lettre, and
//! a server is entitled to refuse a message it dislikes.

use mailparse::MailHeaderMap;
use orb_core::smtp::{OutgoingEmail, draft_bytes};

/// Unique enough to identify this test's own leavings and nothing else.
const SUBJECT: &str = "Orb draft round-trip probe";

#[tokio::test]
#[ignore]
async fn server_accepts_a_draft_with_a_half_typed_recipient() {
    let config = orb_core::config::read().expect("config");
    let account = config.accounts.first().expect("at least one account");

    let email = OutgoingEmail {
        to: vec!["foo".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: SUBJECT.to_string(),
        body: "Written by a test. Safe to delete.".to_string(),
        in_reply_to: None,
        references: None,
        forwards: None,
        from: None,
        reply_to: None,
        attachments: vec![],
        message_id: None,
    };
    let bytes = draft_bytes(account, &email).expect("build");
    println!("--- {} bytes ---", bytes.len());

    let password = account
        .get_imap_password()
        .expect("password")
        .expect("a password command");
    let mut client = orb_core::imap::ImapClient::connect(
        &account.imap_server,
        account.imap_port,
        &account.imap_username,
        &password,
        "live draft test",
    )
    .await
    .expect("connect");

    client
        .select_mailbox(&account.drafts_folder)
        .await
        .expect("select drafts");
    client
        .append_with_flags(&account.drafts_folder, &bytes, Some("(\\Draft \\Seen)"))
        .await
        .expect("the server must accept a draft with an unparseable recipient");
    println!("appended to {} on {}", account.drafts_folder, account.name);

    // The Message-ID this draft was stamped with. Everything below is matched
    // against it: the folder is a real one with real mail in it, and "the UID
    // that appeared last" is a guess, not an identification.
    let parsed = mailparse::parse_mail(&bytes).expect("parse");
    let message_id = parsed
        .headers
        .get_first_value("Message-ID")
        .expect("the draft is stamped with one");

    // Read it back the way the client does, so the check covers the whole round
    // trip and not just the server's willingness to store the bytes.
    let uids = client.fetch_all_uids().await.expect("uids");
    let recent: Vec<u32> = uids.into_iter().rev().take(30).collect();
    let fetched = client.fetch_uids(0, &recent).await.expect("fetch");

    // Everything this test has ever left behind, identified by the subject it
    // writes and nothing else. Swept before the assertions below, so a failing
    // check does not leave a draft sitting in a real folder.
    let litter: Vec<u32> = fetched
        .iter()
        .filter(|m| m.subject.as_deref() == Some(SUBJECT))
        .map(|m| m.uid)
        .collect();
    let mine = fetched
        .iter()
        .find(|m| m.message_id.as_deref() == Some(message_id.as_str()))
        .map(|m| (m.uid, m.subject.clone(), m.to.clone()));
    println!("read back: {mine:?}");
    println!("removing probe drafts: {litter:?}");
    client.delete_messages(&litter).await.expect("cleanup");

    let (_, subject, to) = mine.expect("the draft just appended must be readable");
    assert_eq!(subject.as_deref(), Some(SUBJECT));
    // The point of the exercise: a recipient that is not an address yet
    // survives being saved and read back.
    assert_eq!(to.as_deref(), Some("foo"));
}
