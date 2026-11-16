/**
 * Keyboard events in Neovim's key notation (`:help key-notation`).
 *
 * Only what is not text: printable characters, with or without Alt — which
 * types `@`, `|` and `[` on a Swiss keyboard — and dead keys and input
 * methods arrive through the hidden text field's `input` and composition
 * events instead, so they come out as the system composed them. ⌘ chords
 * belong to the window and the menu bar, and ⌘V arrives as a paste.
 */

const SPECIAL: Record<string, string> = {
  Enter: "CR",
  Escape: "Esc",
  Backspace: "BS",
  Tab: "Tab",
  Delete: "Del",
  ArrowUp: "Up",
  ArrowDown: "Down",
  ArrowLeft: "Left",
  ArrowRight: "Right",
  Home: "Home",
  End: "End",
  PageUp: "PageUp",
  PageDown: "PageDown",
  Insert: "Insert",
  F1: "F1", F2: "F2", F3: "F3", F4: "F4", F5: "F5", F6: "F6",
  F7: "F7", F8: "F8", F9: "F9", F10: "F10", F11: "F11", F12: "F12",
};

/** What a typed character is called inside `<…>`. */
function keyName(key: string): string {
  if (key === "<") return "lt";
  if (key === " ") return "Space";
  if (key === "\\") return "Bslash";
  if (key === "|") return "Bar";
  return key;
}

/** Text as Neovim input: `<` is the one character that means something. */
export function textToInput(text: string): string {
  return text.replace(/</g, "<lt>");
}

export interface KeyLike {
  key: string;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  isComposing?: boolean;
  /** 229 while an input method has the key, in every engine. */
  keyCode?: number;
}

/**
 * The notation for a key, or `null` when it is text, a modifier on its own,
 * part of a composition, or a ⌘ chord — none of which are sent from here.
 */
export function keyToInput(e: KeyLike): string | null {
  if (
    e.isComposing ||
    e.keyCode === 229 ||
    e.key === "Dead" ||
    e.key === "Process" ||
    e.key === "Unidentified"
  ) {
    return null;
  }
  if (e.metaKey) return null;
  const special = SPECIAL[e.key];
  if (special) {
    const mods = (e.ctrlKey ? "C-" : "") + (e.altKey ? "A-" : "") + (e.shiftKey ? "S-" : "");
    return `<${mods}${special}>`;
  }
  // Text: a single character, typed plainly or with Alt, which on a Mac
  // types a character rather than being a modifier.
  if ([...e.key].length !== 1) return null;
  if (!e.ctrlKey) return null;
  return `<C-${e.altKey ? "A-" : ""}${keyName(e.key)}>`;
}

/** Modifiers for `nvim_input_mouse`. */
export function mouseModifiers(e: { ctrlKey: boolean; altKey: boolean; shiftKey: boolean; metaKey: boolean }): string {
  return (e.ctrlKey ? "C-" : "") + (e.altKey ? "A-" : "") + (e.shiftKey ? "S-" : "") + (e.metaKey ? "D-" : "");
}
