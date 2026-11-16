//! A Neovim held at a prompt by the user's configuration must not hang the
//! compose window: it opens, its draft can be read, and it can be closed.
//! Its own test binary, because it needs its own environment. Skipped when
//! Neovim is not installed.
#![cfg(feature = "desktop")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use orb_core::editor::Editors;

#[tokio::test(flavor = "multi_thread")]
async fn a_neovim_at_a_prompt_does_not_hang_the_window() {
    let installed = std::process::Command::new("nvim")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !installed {
        eprintln!("nvim is not installed; skipped");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config/nvim");
    std::fs::create_dir_all(&config).unwrap();
    // An error long enough to stop at "Press ENTER".
    std::fs::write(
        config.join("init.lua"),
        "error(string.rep('a broken configuration ', 40))\n",
    )
    .unwrap();
    // SAFETY: the only test in this binary, setting the environment before
    // anything else runs.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
        std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    }

    let editors = Editors::default();
    let started = Instant::now();
    editors
        .open(
            "prompt-1",
            "compose-1",
            "Kept\n",
            None,
            60,
            10,
            Arc::new(|_| {}),
        )
        .await
        .expect("a prompt is not a failure to start");
    assert!(started.elapsed() < Duration::from_secs(10), "open returns");

    let read = Instant::now();
    assert_eq!(editors.buffer("prompt-1").await.unwrap(), "Kept\n");
    assert!(
        read.elapsed() < Duration::from_secs(5),
        "the draft is read from its file"
    );

    let closing = Instant::now();
    editors.close("prompt-1").await.unwrap();
    assert!(
        closing.elapsed() < Duration::from_secs(6),
        "close does not wait on the prompt"
    );
}
