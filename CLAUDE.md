# Orb Development Guide

Orb is a keyboard-driven IMAP mail client and IMAP-synced task tracker: a Rust
core library (`src/`, crate `orb-core`) under a Tauri app (`app/`), and an iOS
app for the tasks (`ios/`).

## Stability Comes First

Orb is a tool for people who live in their mail, chat and tasks all day.
Reliability, speed and correctness come before everything else, new features
included. For every feature and every bug fix:

- Build on the structures already there; never bolt a change on.
- Cover it with automated tests. A bug fix starts with a test that fails
  without the fix.
- If the code should be refactored first so that the change fits, say so and
  ask before restructuring.

## Documents

- `README.md` — the user manual, describing what the app does **today**.
  Update it in the same change as any behaviour it documents. Never document
  planned behaviour as present.
- `PLAN.md` — everything still missing, broken or rough, with a checklist of
  numbered subphases. Delete an item when it is done, tick a subphase when it
  is empty, and commit the plan in the same commit as the work.
- `todo-spec.md` and `chat-spec.md` — the task and chat specifications. Code
  cites their sections (`§3.1`), so **never renumber them**; append new ones
  at the end. Read the spec before touching `src/tasks/`, `src/chat/` or their
  UI, and §16 of `todo-spec.md` (invariants) before touching task sync.
- `docs/CONCURRENCY_DESIGN.md` — the concurrency and multi-instance model.
  Read it before touching sync or the database.

## Running the App

Never launch the app against your own mail config. Point `ORB_CONFIG` at the
config of a dedicated test account instead, e.g. `.claude/orb/config.yml`,
kept out of git:

```bash
cd app && ORB_CONFIG="$PWD/../.claude/orb/config.yml" pnpm tauri dev
```

## Gates

Run these before committing; CI enforces all but the iOS ones.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib
cargo test --test store_integration
cargo fmt --manifest-path app/src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path app/src-tauri/Cargo.toml --all-targets -- -D warnings
(cd app/src-tauri && cargo test)
(cd app && pnpm build)          # tsc --strict plus the Vite bundle
(cd app && pnpm test)           # Playwright against app/harness.html
scripts/test-ios.sh [--ui]      # macOS only; for changes to src/, crates/orb-mobile/ or ios/
```

`scripts/prune-target.sh` keeps `target/` from growing without bound.

Suites that need Docker are `#[ignore]`d:

```bash
cargo test --test imap_integration --test smtp_integration --test task_sync_integration \
  --test filter_integration --test feed_integration --test task_export_roundtrip -- --ignored
cargo test --test chat_integration -- --ignored --test-threads 2   # Rocket.Chat; or ORB_TEST_ROCKETCHAT
cargo test --test sync_faults --test sync_scale -- --ignored --nocapture
```

The `live_*` tests and `tests/address_book.rs` talk to real accounts; run them
only when asked, and only against a test account.

## Code Standards

- `cargo fmt` and `cargo clippy -D warnings` clean in both crates;
  `pnpm build` clean under `tsc --strict`.
- A Tauri command is a thin shim over an `OrbHandle` method in `src/ffi.rs`;
  the logic lives in the core.

## Project Structure

- `src/` — `orb-core`: IMAP sync and IDLE, the SQLite cache and search, SMTP,
  drafts, tasks (`src/tasks/`), chat (`src/chat/`), and `src/ffi.rs`, the API
  the app is built on.
- `app/src-tauri/` — the Tauri shell: commands, menu, notifications.
- `app/src/` — the frontend, vanilla TypeScript, one module per concern.
- `app/harness.html`, `app/compose-harness.html` — the frontend against a
  stubbed backend; `app/tests/` runs Playwright against them, with
  computed-style snapshots (`pnpm test -- --update-snapshots` after an
  intended change).
- `crates/orb-mobile/` — the UniFFI layer for iOS; `ios/` the app and
  `OrbKit`. A developer's team and bundle prefix go into the git-ignored
  `ios/Local.xcconfig`.
- `tests/` — Rust integration tests; `tests/common/` the container harness.
