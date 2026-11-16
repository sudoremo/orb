import { eventKey } from "./keys";
import { bindTabShortcuts } from "./tabs";
import { isPasswordPromptOpen } from "./password";
import { call, setStatus } from "./bridge";
import { COMMAND_DOCS } from "./commanddocs";
import { COMMANDS, activeBindings, dispatchCommand, vim } from "./commands";
import { escape } from "./format";
import { FuzzyMatch, fuzzyMatch } from "./fuzzy";
import { navigationDirection, spellKey } from "./keys";
import { typingInAField } from "./search";
import { showHeldPinKeys } from "./sidebar";
import { el, state } from "./state";
import { setCommandMode } from "./statusline";
import { keyFor } from "./taskview";

/** Text typed so far, or null when the command line is closed. */
let commandLine: string | null = null;
/** Previously run commands, newest last; browsable with Up/Down. */
const commandHistory: string[] = [];
let historyIndex = 0;
export function openCommandLine(): void {
  commandLine = "";
  browsingHistory = false;
  historyIndex = commandHistory.length;
  drawCommandLine();
}
/**
 * Open the command line with a command already typed.
 *
 * How the task commands that need a word — a tag name — ask for one. The
 * command line is already the place a word is typed in this application, it
 * already has history and editing, and a modal of its own for one field would
 * be a second answer to a question that has one.
 */
export function promptCommand(prefix: string): void {
  openCommandLine();
  commandLine = prefix;
  drawCommandLine();
}
function closeCommandLine(): void {
  commandLine = null;
  tabbing = null;
  setCommandMode(false);
  drawCompletions();
  setStatus(state.status);
}
function drawCommandLine(): void {
  setCommandMode(true);
  el.status.textContent = `:${commandLine ?? ""}`;
  drawCompletions();
}

/** At most this many completions show at once. */
const MAX_COMPLETIONS = 8;
/**
 * While Tab cycles: the text typed before the first Tab, whose matches it
 * walks, and the one it has filled in. Any other key ends the walk.
 */
let tabbing: { typed: string; at: number } | null = null;
/** Whether the arrows are walking the history, begun on an empty line. */
let browsingHistory = false;

/** The commands a typed name could be, best first, with the letters that matched. */
function matches(typed: string): { name: string; positions: number[] }[] {
  const word = typed.trim();
  if (!word) return [];
  return [...Object.keys(COMMANDS), ...Object.keys(state.userCommands)]
    .filter((name) => name !== "noop")
    .map((name) => ({ name, match: fuzzyMatch(word, name) }))
    .filter((m): m is { name: string; match: FuzzyMatch } => m.match !== null)
    .sort((a, b) => a.match.score - b.match.score || a.name.localeCompare(b.name))
    .map((m) => ({ name: m.name, positions: m.match.positions }));
}

/**
 * The completions under the command line: the commands the name typed so far
 * could be, each with what it takes, what it does and its key — so `:` also
 * teaches the bindings. Once the name is followed by a space, the one command
 * it names, as a reminder of its arguments.
 */
function drawCompletions(): void {
  let box = document.getElementById("completions");
  if (commandLine === null) {
    box?.remove();
    return;
  }
  const typed = tabbing?.typed ?? commandLine;
  const named = typed.includes(" ") ? typed.slice(0, typed.indexOf(" ")) : null;
  const found = named !== null
    ? (COMMANDS[named] || state.userCommands[named] ? [{ name: named, positions: [] as number[] }] : [])
    : matches(typed).slice(0, MAX_COMPLETIONS);
  const names = found.map((m) => m.name);
  if (names.length === 0) {
    box?.remove();
    return;
  }
  if (!box) {
    box = document.createElement("div");
    box.id = "completions";
    box.className = "cmpl";
    box.setAttribute("role", "listbox");
    box.setAttribute("aria-label", "Commands");
    box.addEventListener("mousedown", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-command]");
      if (!row || commandLine === null) return;
      e.preventDefault();
      commandLine = row.dataset.command! + (COMMAND_DOCS[row.dataset.command!]?.args ? " " : "");
      tabbing = null;
      drawCommandLine();
    });
    document.body.append(box);
  }
  box.innerHTML = found
    .map(({ name, positions }, i) => {
      const own = state.userCommands[name];
      const doc = COMMAND_DOCS[name] ?? (own && { title: own.title, does: own.title ?? own.run });
      const shown = [...name].map((letter, at) => (positions.includes(at) ? `<b>${escape(letter)}</b>` : escape(letter))).join("");
      const key = keyFor(name);
      const on = tabbing?.at === i;
      return `<div class="ci${on ? " on" : ""}" role="option" aria-selected="${on}" data-command="${escape(name)}">
          <span class="cn">${shown}${doc?.args ? ` <span class="arg">${escape(doc.args)}</span>` : ""}</span>
          <span class="cd">${escape(doc?.does ?? "")}</span>
          ${key ? `<kbd>${escape(spellKey(key))}</kbd>` : "<span></span>"}
        </div>`;
    })
    .join("");
}

/** Fill in the next (or, backwards, the previous) completion of what was typed. */
function cycleCompletion(step: 1 | -1): void {
  if (commandLine === null) return;
  const typed = tabbing?.typed ?? commandLine;
  if (typed.includes(" ")) return;
  const names = matches(typed).slice(0, MAX_COMPLETIONS).map((m) => m.name);
  if (names.length === 0) return;
  const at = tabbing ? (tabbing.at + step + names.length) % names.length : step === 1 ? 0 : names.length - 1;
  tabbing = { typed, at };
  commandLine = names[at]!;
  drawCommandLine();
}
/**
 * Handle a key while the command line is open.
 *
 * Runs before the modal dispatcher, so typing `d` composes text rather than
 * staging a delete.
 */
function handleCommandLineKey(e: KeyboardEvent): boolean {
  if (commandLine === null) return false;
  const key = eventKey(e);

  if (key === "<esc>") {
    closeCommandLine();
    return true;
  }
  if (key === "<tab>" || key === "<s-tab>") {
    cycleCompletion(key === "<tab>" ? 1 : -1);
    return true;
  }
  // The arrows and ⌃J/⌃K walk the completions shown; on an empty line, and
  // once begun there, the arrows walk the commands run before instead.
  const back = key === "<up>" || key === "<c-k>";
  if (back || key === "<down>" || key === "<c-j>") {
    const arrow = key === "<up>" || key === "<down>";
    if (arrow && (browsingHistory || commandLine === "")) {
      if (commandHistory.length) {
        browsingHistory = true;
        historyIndex = Math.min(Math.max(historyIndex + (back ? -1 : 1), 0), commandHistory.length);
        commandLine = commandHistory[historyIndex] ?? "";
        drawCommandLine();
      }
    } else {
      cycleCompletion(back ? -1 : 1);
    }
    return true;
  }
  // Shift on its own, on the way to Shift-Tab, is not an edit.
  if (["Shift", "Control", "Alt", "Meta"].includes(e.key)) return true;
  tabbing = null;
  browsingHistory = false;
  if (key === "<enter>") {
    const spec = commandLine.trim();
    closeCommandLine();
    if (spec) {
      commandHistory.push(spec);
      void dispatchCommand(spec, 1);
    }
    return true;
  }
  if (key === "<backspace>") {
    // Backspacing past the start dismisses, as it does in vim.
    if (commandLine === "") closeCommandLine();
    else {
      commandLine = commandLine.slice(0, -1);
      drawCommandLine();
    }
    return true;
  }
  // `<space>` is a named key, not a one-character one, so typing a space was
  // silently dropped — which made every command with an argument impossible.
  const typed = key === "<space>" ? " " : key.length === 1 ? key : null;
  if (typed !== null) {
    commandLine += typed;
    drawCommandLine();
    return true;
  }
  return true; // swallow everything else while open
}
// Tab switching first: it is the one set of keys that has to work from every
// window, and the keymap below deliberately ignores anything with a modifier.
bindTabShortcuts();
window.addEventListener("keydown", (e) => {
  // Alt is left to the system. Command is not: ⌘A selects everything, and
  // `eventKey` spells it `<d-a>` so it cannot collide with the plain letter.
  // Anything unbound falls through untouched, so ⌘C and ⌘V still copy and
  // paste.
  if (e.altKey) return;
  if (isPasswordPromptOpen()) return;
  // A field has its own idea of what a keystroke means, and it wins — but for
  // Ctrl and a digit, the pins' keys, which type nothing in a field, and a
  // chord that pins what is open, which leaves the field to go on typing in.
  if (typingInAField(e.target)) {
    const key = eventKey(e);
    const command = /^<c-[0-9]>$/.test(key) ? activeBindings()[key] : undefined;
    if (command) {
      e.preventDefault();
      (e.target as HTMLElement).blur();
      void dispatchCommand(command, 1);
    } else if (/^<[cd]-/.test(key) && activeBindings()[key]?.trim() === "toggle-pin") {
      e.preventDefault();
      void dispatchCommand("toggle-pin", 1);
    }
    return;
  }
  // Ctrl on its own shows the pins' keys; the key it is held for hides them.
  if (!e.repeat) showHeldPinKeys(e.key === "Control" && commandLine === null, activeBindings());
  if (handleCommandLineKey(e)) {
    e.preventDefault();
    return;
  }
  if (vim.handle(e)) {
    e.preventDefault();
    return;
  }
  const direction = navigationDirection(eventKey(e));
  if (direction) {
    e.preventDefault();
    void call("escalate", { direction }).catch(() => {});
  }
});
window.addEventListener("keyup", (e) => {
  if (e.key === "Control") showHeldPinKeys(false, {});
});
window.addEventListener("blur", () => showHeldPinKeys(false, {}));
