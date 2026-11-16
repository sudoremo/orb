/**
 * Moving between the window's tabs.
 *
 * Shared by every window in the group, because a shortcut that only works while
 * the mailbox has focus is not a shortcut for switching away from a draft.
 *
 * Matched on `KeyboardEvent.code` — the physical key — rather than on `key`,
 * which is the character the layout produces. On a Swiss German keyboard the
 * key carrying ö sits where a US layout has `;`, and pressing it with Alt gives
 * a third character again, so matching what was typed would work on one layout
 * and silently not on another. The code says which key was pressed regardless.
 */

import { invoke } from "@tauri-apps/api/core";

/** Physical keys for previous and next, whatever they are engraved with. */
const PREVIOUS = "Semicolon"; // ö on Swiss German, ; on US
const NEXT = "Quote"; // ä on Swiss German, ' on US

function select(which: string): void {
  void invoke("select_tab", { which }).catch(() => {
    // Nothing to switch to, or no tab group. Not worth reporting: the
    // shortcut is a convenience and its failure is silence, not an error.
  });
}

/**
 * Listen for the tab shortcuts.
 *
 * Call once per window.
 */
export function bindTabShortcuts(): void {
  window.addEventListener("keydown", (e) => {
    // Cmd-1 to Cmd-9 pick a tab by position. Digits are in the same place on
    // every layout, so these can be matched on the character.
    if (e.metaKey && !e.altKey && !e.ctrlKey && /^[1-9]$/.test(e.key)) {
      e.preventDefault();
      select(e.key);
      return;
    }

    if (e.altKey && !e.metaKey && !e.ctrlKey) {
      if (e.code === PREVIOUS) {
        e.preventDefault();
        select("previous");
      } else if (e.code === NEXT) {
        e.preventDefault();
        select("next");
      }
    }
  });
}
