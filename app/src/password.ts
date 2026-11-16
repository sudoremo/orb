/**
 * The password prompt: for a login whose config has no password command and
 * whose password the Keychain does not hold.
 *
 * The core raises one through the `password-needed` event and waits for the
 * `answer_password` command; the sync, or the send, that needs the password
 * is held until then. A window that opens after a prompt was raised asks for
 * what is pending, so a prompt is never lost to a closed window.
 *
 * Built on the picker's markup and styles: one modal look, one scrim, one set
 * of rules about focus. It owns the keyboard while open; Enter answers, Escape
 * dismisses, and a dismissed prompt fails the login that asked rather than
 * asking again until the next sync.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface PasswordPrompt {
  id: number;
  account: string;
  kind: string;
  username: string;
  server: string;
  /** The server refused the last password given for this login. */
  retry: boolean;
}

const queue: PasswordPrompt[] = [];
let current: Prompt | null = null;

/** Whether a prompt owns the keyboard, for a caller deciding what a key means. */
export function isPasswordPromptOpen(): boolean {
  return current !== null;
}

/** Listen for prompts, and pick up any raised before this window was here. */
export function installPasswordPrompt(): void {
  void listen<PasswordPrompt>("password-needed", (event) => enqueue(event.payload));
  void invoke<PasswordPrompt[]>("pending_password_prompts")
    .then((pending) => pending.forEach(enqueue))
    .catch(() => {
      // Nothing pending is the ordinary case; a failure to ask is no worse.
    });
}

function enqueue(prompt: PasswordPrompt): void {
  if (queue.some((p) => p.id === prompt.id) || current?.prompt.id === prompt.id) return;
  queue.push(prompt);
  showNext();
}

function showNext(): void {
  if (current !== null) return;
  const next = queue.shift();
  if (!next) return;
  current = new Prompt(next, (password, store) => {
    current = null;
    void invoke("answer_password", { id: next.id, password, store }).catch(() => {
      // The login that asked has given up in the meantime; nothing to do.
    });
    showNext();
  });
}

class Prompt {
  readonly prompt: PasswordPrompt;
  private readonly scrim: HTMLDivElement;
  private readonly input: HTMLInputElement;
  private readonly keep: HTMLInputElement;
  private readonly done: (password: string | null, store: boolean) => void;
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus: Element | null;
  private closed = false;

  constructor(prompt: PasswordPrompt, done: (password: string | null, store: boolean) => void) {
    this.prompt = prompt;
    this.done = done;
    this.restoreFocus = document.activeElement;

    this.scrim = document.createElement("div");
    this.scrim.className = "dp-scrim";
    const panel = document.createElement("div");
    panel.className = "dp password";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");

    const header = document.createElement("div");
    header.className = "dp-head";
    const title = document.createElement("span");
    title.className = "dp-title";
    title.textContent = `${prompt.kind} password · ${prompt.account}`;
    header.append(title);

    const who = document.createElement("div");
    who.className = "pw-login";
    who.textContent = `${prompt.username} on ${prompt.server}`;
    if (prompt.retry) {
      const refused = document.createElement("div");
      refused.className = "pw-refused";
      refused.textContent = "The server refused the last password.";
      who.append(refused);
    }

    this.input = document.createElement("input");
    this.input.type = "password";
    this.input.className = "dp-input";
    this.input.autocomplete = "current-password";
    this.input.placeholder = "password";

    const keepRow = document.createElement("label");
    keepRow.className = "pw-keep";
    this.keep = document.createElement("input");
    this.keep.type = "checkbox";
    this.keep.checked = true;
    keepRow.append(this.keep, document.createTextNode(" Keep in the Keychain"));

    const foot = document.createElement("div");
    foot.className = "dp-foot";
    foot.textContent = "enter  continue   esc  skip this login";

    panel.append(header, who, this.input, keepRow, foot);
    this.scrim.append(panel);
    document.body.append(this.scrim);

    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.finish(null);
    });
    this.onKeyDown = (e) => this.handleKey(e);
    window.addEventListener("keydown", this.onKeyDown, true);
    this.input.focus();
  }

  private handleKey(e: KeyboardEvent): void {
    // The field's own editing keys pass through; only the decisions are
    // taken here, and nothing reaches the window behind.
    e.stopPropagation();
    if (e.key === "Enter") {
      e.preventDefault();
      if (this.input.value.length > 0) this.finish(this.input.value);
    } else if (e.key === "Escape") {
      e.preventDefault();
      this.finish(null);
    }
  }

  private finish(password: string | null): void {
    if (this.closed) return;
    this.closed = true;
    window.removeEventListener("keydown", this.onKeyDown, true);
    const store = this.keep.checked;
    this.input.value = "";
    this.scrim.remove();
    if (this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus();
    this.done(password, store);
  }
}
