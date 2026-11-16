//! The embedded Neovim against a real `nvim --embed`, with no user
//! configuration. Skipped when Neovim is not installed.
#![cfg(feature = "desktop")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use orb_core::editor::{EditorEvent, Editors};

fn has_nvim() -> bool {
    std::process::Command::new("nvim")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

async fn until(events: &Arc<Mutex<Vec<EditorEvent>>>, what: impl Fn(&EditorEvent) -> bool) -> bool {
    for _ in 0..100 {
        if events.lock().unwrap().iter().any(&what) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_is_drawn_typed_into_written_and_read_back() {
    if !has_nvim() {
        eprintln!("nvim is not installed; skipped");
        return;
    }
    // The user's configuration stays out of it: an empty config directory,
    // and no shada or swap files left behind.
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary, and it sets the
    // environment before it starts any thread that reads it.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
        std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    }

    let events: Arc<Mutex<Vec<EditorEvent>>> = Arc::default();
    let sink = {
        let events = events.clone();
        Arc::new(move |e: EditorEvent| events.lock().unwrap().push(e))
    };
    let editors = Editors::default();
    editors
        .open(
            "test-1",
            "compose-1",
            "Hello\n\n-- \nMe\n",
            None,
            40,
            10,
            sink.clone(),
        )
        .await
        .expect("open");

    // The screen arrives in batches ending in a flush, the body among the
    // cells: `grid_line` sends a cell per character.
    assert!(
        until(&events, |e| matches!(e, EditorEvent::Redraw(batch)
            if batch.last().and_then(|e| e[0].as_str()) == Some("flush")
                && batch.iter().any(|event| event[0] == "grid_line"
                    && event.to_string().contains(r#"["H""#))))
        .await,
        "a redraw batch with the body"
    );

    editors
        .input("test-1", "ggAthere, <lt>you><Esc>")
        .await
        .unwrap();
    let mut buffer = String::new();
    for _ in 0..40 {
        buffer = editors.buffer("test-1").await.unwrap();
        if buffer.starts_with("Hellothere") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        buffer, "Hellothere, <you>\n\n-- \nMe\n",
        "the buffer, unwritten"
    );

    editors.input("test-1", ":w<CR>").await.unwrap();
    assert!(
        until(&events, |e| matches!(e, EditorEvent::Written)).await,
        ":w is reported"
    );

    editors
        .set_buffer("test-1", "Replaced\n", None)
        .await
        .unwrap();
    assert_eq!(editors.buffer("test-1").await.unwrap(), "Replaced\n");

    editors.input("test-1", ":q!<CR>").await.unwrap();
    assert!(
        until(&events, |e| matches!(e, EditorEvent::Exited)).await,
        "quitting is reported"
    );
    editors.close("test-1").await.unwrap();
    assert!(
        editors.input("test-1", "x").await.is_err(),
        "a closed editor is gone"
    );

    // The signature is drawn below the text, not in it: what is typed at the
    // end goes above it, and trimming every line's trailing space leaves its
    // `-- ` alone.
    events.lock().unwrap().clear();
    editors
        .open(
            "test-2",
            "compose-1",
            "Hello\n\n-- \nMe\n",
            Some("-- \nMe"),
            40,
            10,
            sink,
        )
        .await
        .expect("open");
    assert!(
        until(&events, |e| matches!(e, EditorEvent::Redraw(batch)
            if batch.iter().any(|event| event[0] == "grid_line"
                    && event.to_string().contains(r#"["M""#))))
        .await,
        "the signature is drawn"
    );
    editors
        .input("test-2", "GoPS<Esc>:%s/\\s\\+$//e<CR>")
        .await
        .unwrap();
    let mut buffer = String::new();
    for _ in 0..40 {
        buffer = editors.buffer("test-2").await.unwrap();
        if buffer.contains("PS") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        buffer, "Hello\n\nPS\n-- \nMe\n",
        "typed above the signature"
    );

    // Above a quote, it stays above the quote as lines come and go around it,
    // and is still there once Neovim has gone.
    editors
        .set_buffer("test-2", "Hi\n\n-- \nMe\n\n> quote\n", Some("-- \nMe"))
        .await
        .unwrap();
    assert_eq!(
        editors.buffer("test-2").await.unwrap(),
        "Hi\n\n-- \nMe\n\n> quote\n"
    );
    editors.input("test-2", "ggOtop<Esc>:x<CR>").await.unwrap();
    assert!(
        until(&events, |e| matches!(e, EditorEvent::Exited)).await,
        "quitting is reported"
    );
    assert_eq!(
        editors.buffer("test-2").await.unwrap(),
        "top\nHi\n\n-- \nMe\n\n> quote\n",
        "read back from the file"
    );
    editors.close("test-2").await.unwrap();
}
