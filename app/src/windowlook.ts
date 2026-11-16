/**
 * The appearance of a window other than the mailbox: the theme, the look and
 * the text size the config asks for, and the size ⌘+ and ⌘- and the look
 * `toggle-look` chose in the mailbox window, which reach every window of the
 * app through the shared storage.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const MIN = 9;
const MAX = 24;

/** Follow the configured appearance from now on. */
export async function followAppearance(): Promise<void> {
  const root = document.documentElement;
  let configured = 13;
  let configuredEditor: number | null = null;
  let configuredLook = "hybrid";
  const read = async () => {
    try {
      const settings = await invoke<{
        theme?: string;
        font_size?: number;
        editor_font_size?: number | null;
        look?: string;
      }>("ui_settings");
      if (settings.theme === "dark" || settings.theme === "light") root.dataset.theme = settings.theme;
      else delete root.dataset.theme;
      configuredLook = settings.look === "terminal" ? "terminal" : "hybrid";
      if (settings.font_size) configured = settings.font_size;
      configuredEditor = settings.editor_font_size ?? null;
    } catch {
      // The defaults, then.
    }
  };
  const apply = () => {
    let size = configured;
    try {
      const saved = Number(localStorage.getItem("orb.fontSize"));
      if (saved >= MIN && saved <= MAX) size = saved;
    } catch {
      // No storage: the configured size.
    }
    root.style.setProperty("--font-size", `${size}px`);
    // ⌘+ and ⌘- scale Neovim's size by as much as they scale the window's.
    if (configuredEditor) {
      const editor = Math.round((configuredEditor * size) / configured);
      root.style.setProperty("--editor-font-size", `${editor}px`);
    } else {
      root.style.removeProperty("--editor-font-size");
    }
    let look = configuredLook;
    try {
      const saved = localStorage.getItem("orb.look");
      if (saved === "hybrid" || saved === "terminal") look = saved;
    } catch {
      // No storage: the configured look.
    }
    root.dataset.look = look;
  };
  await read();
  apply();
  // Fired here when another window of the app changes the size or the look.
  window.addEventListener("storage", (e) => {
    if (e.key === "orb.fontSize" || e.key === "orb.look" || e.key === null) apply();
  });
  void listen("config-reloaded", async () => {
    await read();
    apply();
  }).catch(() => {
    // No events outside the app: the appearance stays as read.
  });
}
