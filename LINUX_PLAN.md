# Orb on Linux

What it would take to run Orb on Linux (e.g. Fedora) as well as on macOS.
Assessed against the code on 2026-10-28; file references are a starting point,
not a specification.

In short: a few days to build and run, two to three weeks to feel finished.
The ground is prepared — nearly every macOS-only call already has a
`#[cfg(not(target_os = "macos"))]` stub beside it, kept so the crates compile
elsewhere.

## Progress

- [ ] Phase 1: Builds and runs (2–4 days)
  - [ ] 1.1 Passwords in the Secret Service
  - [ ] 1.2 Opening files and links
  - [ ] 1.3 Cmd bindings on Ctrl
  - [ ] 1.4 A window that renders on WebKitGTK
  - [ ] 1.5 CI for the app on Linux
- [ ] Phase 2: Comfortable to use (1–2 weeks)
  - [ ] 2.1 Reminders
  - [ ] 2.2 Window tabs
  - [ ] 2.3 Quick Look and reveal
  - [ ] 2.4 WebKitGTK performance and rendering
  - [ ] 2.5 Packaging
  - [ ] 2.6 Documentation
- [ ] Phase 3: Optional extras
  - [ ] 3.1 Contacts
  - [ ] 3.2 Badges

## Already in place

- **`orb-core`** builds and tests on `ubuntu-latest` in CI today: sync, store,
  SMTP, tasks and chat.
- **Tauri 2** supports Linux through WebKitGTK and bundles `.rpm` and AppImage.
  The `macos-private-api` feature is inert there.
- **The app shell** very likely compiles as is, but is unverified: the CI `app`
  job runs on `macos-latest` only.

## Phase 1: Builds and runs

### 1.1 Passwords in the Secret Service

`src/keychain.rs` stores passwords in the Keychain via `security-framework`.
The non-macOS `PasswordStore` returns `None` from `get` and fails `set`, so no
account can authenticate. Implement it against the Secret Service (GNOME
Keyring, KWallet) with the `keyring` or `secret-service` crate. Blocks
everything else.

### 1.2 Opening files and links

`open` is spawned in `app/src-tauri/src/main.rs` (`open_on_disk` and three more
places), `chat.rs` and `notifications.rs`. Replace it with `xdg-open`, or with
`tauri-plugin-opener` for both platforms at once.

### 1.3 Cmd bindings on Ctrl

There are 28 `<d-…>` bindings (`bindings.ts`, `commandline.ts`, `keys.ts`,
`compose.ts`, `picker.ts`) and `metaKey` checks in about ten frontend modules
(`keys.ts`, `nvimkeys.ts`, `list.ts`, `reader.ts`, `tabs.ts`, `taskpointer.ts`,
`stepsui.ts`, `chat.ts`, `compose.ts`). On Linux the window manager grabs
Super, so these move to Ctrl — which collides with the existing Ctrl bindings
and needs a mapping decided case by case. Menu labels (`menus.ts`, `tabs.ts`)
and the README follow.

### 1.4 A window that renders on WebKitGTK

`styles.css` relies on vibrancy, `backdrop-filter`, SF Pro / `-apple-system`
and an inset for the macOS title bar. Window transparency on WebKitGTK is
unreliable: give Linux a solid background, a normal title bar and a font
fallback. `give_toolbar` and `title_bar` already fall back to defaults.

### 1.5 CI for the app on Linux

Add `ubuntu-latest` to the `app` job in `.github/workflows/ci.yml`, with
`webkit2gtk-4.1`, `libsoup3` and the other Tauri build dependencies installed.

## Phase 2: Comfortable to use

### 2.1 Reminders

`notifications.rs` schedules reminders with `UNUserNotificationCenter`, so they
fire while Orb is closed (todo-spec §5.1). Linux has no OS-scheduled
notifications; the stub reports `Permission::Unavailable`. Options are systemd
user timers that post through `notify-send` / the freedesktop notification
interface, or an in-app timer — which §5.1 rules out. Needs a decision before
code.

### 2.2 Window tabs

`add_as_tab` and `switch_tab` in `main.rs` use AppKit window tab groups, which
have no Linux equivalent. Either accept separate windows (the stub already
does) or build tabs inside the app.

### 2.3 Quick Look and reveal

`qlmanage -p` has no portable equivalent: use GNOME's previewer (`sushi`, over
D-Bus) where present, or drop the command on Linux. `open -R` becomes
`org.freedesktop.FileManager1.ShowItems` over D-Bus.

### 2.4 WebKitGTK performance and rendering

WebKitGTK is not Safari's WebKit: check the virtual list, large message
bodies and the compose editor. The Playwright suite should run against WebKit
on Linux, with snapshots for the Linux look.

### 2.5 Packaging

Configure Tauri's `.rpm` and AppImage bundles in `tauri.conf.json`; the PNG
icons exist, `icon.icns` and `Assets.car` are macOS-only.

### 2.6 Documentation

A README section on what differs on Linux, kept to what works today.

## Phase 3: Optional extras

### 3.1 Contacts

`contacts.rs` reads Contacts.framework; the stub returns nothing. Could read
Evolution Data Server or a vCard/CardDAV source, or stay without.

### 3.2 Badges

`dock.rs` draws Dock badges. The Unity `LauncherEntry` D-Bus API covers KDE and
some docks, not stock GNOME. Can stay a stub.

## Stays macOS-only

- **Things import** (`src/tasks/things.rs`): Things is a Mac app.
- **Accessibility permission** (`accessibility.rs`): Linux has no such grant;
  the no-op stub is correct.

## Decisions needed

- How reminders fire on Linux (2.1).
- Which Ctrl binding each Cmd binding becomes (1.3).
