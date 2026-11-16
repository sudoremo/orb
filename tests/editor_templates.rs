//! A mail template put into the embedded Neovim and its placeholders filled,
//! against a real `nvim --embed` with no user configuration. Skipped when
//! Neovim is not installed.
#![cfg(feature = "desktop")]

use std::sync::Arc;
use std::time::Duration;

use orb_core::editor::{EditorEvent, Editors};
use orb_core::templates::expand;

fn has_nvim() -> bool {
    std::process::Command::new("nvim")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The buffer once `ready` holds for it, or what it was when waiting ran out.
async fn buffer_when(editors: &Editors, id: &str, ready: impl Fn(&str) -> bool) -> String {
    let mut buffer = String::new();
    for _ in 0..60 {
        buffer = editors.buffer(id).await.unwrap();
        if ready(&buffer) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    buffer
}

#[tokio::test(flavor = "multi_thread")]
async fn a_template_goes_in_at_the_cursor_and_its_placeholders_fill_where_they_stand() {
    if !has_nvim() {
        eprintln!("nvim is not installed; skipped");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary, and it sets the
    // environment before it starts any thread that reads it.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
        std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    }
    let sink = Arc::new(|_: EditorEvent| {});
    let editors = Editors::default();
    let reply = "\n\nOn Monday, Jonas wrote:\n> Wie bereite ich mich vor? {Datum}\n\n-- \nSam\n";
    editors
        .open("t", "compose-1", reply, Some("-- \nSam"), 60, 20, sink)
        .await
        .expect("open");
    buffer_when(&editors, "t", |b| b.contains("Jonas wrote")).await;

    // On the blank first line, where a reply opens: the template takes it.
    let text = expand("Lieber {Vorname}\n\nAm {Datum} um {Zeit}, also {Datum}.");
    assert!(editors.insert_template("t", &text).await.unwrap());
    let inserted = buffer_when(&editors, "t", |b| b.starts_with("Lieber")).await;
    assert_eq!(
        inserted,
        "Lieber {Vorname}\n\nAm {Datum} um {Zeit}, also {Datum}.\n\nOn Monday, Jonas wrote:\n> Wie bereite ich mich vor? {Datum}\n\n-- \nSam\n"
    );

    // Every place the placeholder stands, and nowhere else — not the quote.
    editors
        .fill_placeholder("t", "Datum", "Donnerstag")
        .await
        .unwrap();
    editors
        .fill_placeholder("t", "Vorname", "Jonas")
        .await
        .unwrap();
    let filled = editors.buffer("t").await.unwrap();
    assert!(filled.starts_with("Lieber Jonas\n\nAm Donnerstag um {Zeit}, also Donnerstag.\n"));
    assert!(filled.contains("> Wie bereite ich mich vor? {Datum}"));

    // Typed over in the field, again and again; and cleared, it is back in braces.
    editors.fill_placeholder("t", "Datum", "Do").await.unwrap();
    editors
        .fill_placeholder("t", "Datum", "Dienstag, 3. März")
        .await
        .unwrap();
    let typed = editors.buffer("t").await.unwrap();
    assert!(typed.contains("Am Dienstag, 3. März um {Zeit}, also Dienstag, 3. März."));
    editors.fill_placeholder("t", "Datum", "").await.unwrap();
    assert!(
        editors
            .buffer("t")
            .await
            .unwrap()
            .contains("Am {Datum} um {Zeit}, also {Datum}.")
    );

    // Text typed in front moves the placeholders along.
    editors.input("t", "ggIHallo, <Esc>").await.unwrap();
    buffer_when(&editors, "t", |b| b.starts_with("Hallo, ")).await;
    editors
        .fill_placeholder("t", "Vorname", "Jonas K.")
        .await
        .unwrap();
    assert!(
        editors
            .buffer("t")
            .await
            .unwrap()
            .starts_with("Hallo, Lieber Jonas K.\n")
    );

    // A placeholder edited by hand is the hand's.
    editors.input("t", "3G0fZcwZ<Esc>").await.unwrap();
    let edited = buffer_when(&editors, "t", |b| !b.contains("{Zeit")).await;
    assert!(edited.contains("um {Z},"), "{edited}");
    editors
        .fill_placeholder("t", "Zeit", "14:00")
        .await
        .unwrap();
    assert!(editors.buffer("t").await.unwrap().contains("um {Z},"));

    assert!(editors.goto_placeholder("t", "Datum").await.unwrap());
    assert!(!editors.goto_placeholder("t", "Unknown").await.unwrap());

    // On a line with text, below it.
    editors.input("t", "gg").await.unwrap();
    assert!(
        editors
            .insert_template("t", &expand("Second"))
            .await
            .unwrap()
    );
    let below = buffer_when(&editors, "t", |b| b.contains("Second")).await;
    assert!(
        below.starts_with("Hallo, Lieber Jonas K.\nSecond\n"),
        "{below}"
    );

    editors.close("t").await.unwrap();
}
