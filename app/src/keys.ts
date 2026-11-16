/**
 * Key notation shared between `config.yml` and the browser.
 *
 * The config is the authority on spelling — it already contains entries like
 * `<C-d>`, `<enter>`, `<esc>`, `gg` and `ä`. Both sides are normalised to one
 * canonical form so a binding table can be looked up directly by the key of a
 * `KeyboardEvent`.
 *
 * Canonical form: bracketed names are lowercased (`<C-d>` → `<c-d>`) and
 * aliases collapse (`<CR>`/`<Return>` → `<enter>`). Bare characters keep their
 * case, because `j` and `J` are different bindings.
 */

/** Bracketed aliases that mean the same key. */
const ALIASES: Record<string, string> = {
  "<cr>": "<enter>",
  "<return>": "<enter>",
  "<escape>": "<esc>",
  "<bs>": "<backspace>",
  "<del>": "<delete>",
};

/** Normalise a notation string from either the config or `eventKey`. */
export function canonicalKey(notation: string): string {
  if (!notation.startsWith("<") || !notation.endsWith(">")) return notation;
  const lowered = notation.toLowerCase();
  // Shift with Command is written either way round: `<S-D-y>` is `<d-s-y>`.
  const shifted = lowered.match(/^<s-d-(.+)>$/);
  if (shifted) return `<d-s-${shifted[1]}>`;
  return ALIASES[lowered] ?? lowered;
}

/** Named keys whose `KeyboardEvent.key` is not a single character. */
/** Arrows with Shift held, which mean "extend" rather than "move". */
const SHIFTED_ARROWS: Record<string, string> = {
  ArrowUp: "<s-up>",
  ArrowDown: "<s-down>",
};

const COMMAND_ARROWS: Record<string, string> = {
  ArrowUp: "<d-up>",
  ArrowDown: "<d-down>",
};

const NAMED: Record<string, string> = {
  Enter: "<enter>",
  Escape: "<esc>",
  " ": "<space>",
  Backspace: "<backspace>",
  Delete: "<delete>",
  ArrowUp: "<up>",
  ArrowDown: "<down>",
  ArrowLeft: "<left>",
  ArrowRight: "<right>",
  Home: "<home>",
  End: "<end>",
  PageUp: "<pageup>",
  PageDown: "<pagedown>",
};

/** A way out of Orb, for `navigation.escalate_command`. */
export type Direction = "left" | "right" | "up" | "down";

const DIRECTIONS: Record<string, Direction> = {
  "<c-h>": "left",
  "<c-j>": "down",
  "<c-k>": "up",
  "<c-l>": "right",
};

/**
 * The direction a Ctrl+H/J/K/L points, or `null` for any other key. Orb
 * hands one it has no use for to the escalate command, so the chords that
 * move between windows everywhere else keep doing so from Orb.
 */
export function navigationDirection(key: string): Direction | null {
  return DIRECTIONS[key] ?? null;
}

/** Canonical notation for a key event, matching the config's vocabulary. */
export function eventKey(e: KeyboardEvent): string {
  if (e.key === "Tab") return e.shiftKey ? "<s-tab>" : "<tab>";

  // Command, spelled as vim spells it on macOS. A letter typed with Command
  // held arrives as the bare letter, so without this ⌘A would run whatever `a`
  // is bound to — in a config where that is "archive", silently.
  // Shift is kept with a letter, so ⇧⌘Y is not ⌘Y; a symbol already says it
  // (`⌘+` is Shift and `=`).
  if (e.metaKey && e.key.length === 1) {
    const shifted = e.shiftKey && /^[a-z]$/i.test(e.key) ? "s-" : "";
    return canonicalKey(`<d-${shifted}${e.key.toLowerCase()}>`);
  }
  if (e.metaKey && e.key === "Enter") return e.shiftKey ? "<d-s-enter>" : "<d-enter>";
  // ⌘↑ is not ↑: it moves a task through the order rather than the cursor.
  if (e.metaKey && !e.shiftKey && COMMAND_ARROWS[e.key]) return COMMAND_ARROWS[e.key]!;

  // Shift with an arrow extends a selection, and has to be told apart from the
  // arrow alone. Shift with a letter needs no marking: it produces the capital.
  if (e.shiftKey) {
    const arrow = SHIFTED_ARROWS[e.key];
    if (arrow) return arrow;
  }

  // Ctrl collapses to <c-x>; the config never distinguishes <C-d> from <C-D>.
  if (e.ctrlKey && e.key.length === 1) {
    return canonicalKey(`<c-${e.key.toLowerCase()}>`);
  }

  const named = NAMED[e.key];
  if (named) return named;

  // Single characters keep their case so `j` and `J` stay distinct, and so
  // non-ASCII bindings such as `ä`/`ö` work unchanged.
  return e.key;
}

/** Canonicalise every key of a binding table read from the config. */
export function canonicalTable(
  table: Record<string, string> | null | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, command] of Object.entries(table ?? {})) {
    out[canonicalKey(key)] = command;
  }
  return out;
}
/**
 * A binding as a hint line should show it.
 *
 * The config's notation is exact and unreadable at a glance — `<c-d>` is a
 * spelling, `⌃d` is a key. Everything unrecognised is shown as it was
 * configured, which is still better than not naming it.
 */
export function spellKey(key: string): string {
  const named: Record<string, string> = {
    "<enter>": "⏎",
    "<esc>": "esc",
    "<tab>": "⇥",
    "<s-tab>": "⇧⇥",
    "<up>": "↑",
    "<down>": "↓",
    "<left>": "←",
    "<right>": "→",
    "<space>": "space",
    "<home>": "home",
    "<end>": "end",
    "<pageup>": "⇞",
    "<pagedown>": "⇟",
  };
  if (named[key]) return named[key];
  const control = key.match(/^<c-(.+)>$/);
  if (control) return `⌃${control[1]}`;
  const shiftCommand = key.match(/^<d-s-(.)>$/);
  if (shiftCommand) return `⇧⌘${shiftCommand[1]!.toUpperCase()}`;
  const command = key.match(/^<d-(.+)>$/);
  if (command) return `⌘${named[`<${command[1]}>`] ?? command[1]}`;
  return key;
}
