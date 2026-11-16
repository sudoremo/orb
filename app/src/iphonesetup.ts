/**
 * Set Up iPhone: the code Orb on the iPhone scans to set itself up — the tasks
 * account, its folder and settings, and the password (IOS_PLAN.md 2.1).
 *
 * The code is as sensitive as the password, so it is shown only while this is
 * open and says so; each code works for five minutes, and a new one replaces
 * it while the dialog stays open. Built on the picker's frame, like the
 * password prompt. Escape or a click outside closes it.
 */

import { invoke } from "@tauri-apps/api/core";

interface SetupCode {
  svg: string;
  /** Seconds since the epoch. */
  expires: number;
  login: string;
  folder: string;
}

let open: SetupDialog | null = null;

/** Whether the dialog owns the keyboard, for a caller deciding what a key means. */
export function isIphoneSetupOpen(): boolean {
  return open !== null;
}

/** Show the code; resolves once the dialog is closed. */
export function showIphoneSetup(): Promise<void> {
  if (open) return Promise.resolve();
  return new Promise((resolve) => {
    open = new SetupDialog(() => {
      open = null;
      resolve();
    });
  });
}

class SetupDialog {
  private readonly scrim: HTMLDivElement;
  private readonly code: HTMLDivElement;
  private readonly about: HTMLDivElement;
  private readonly countdown: HTMLDivElement;
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus: Element | null;
  private readonly done: () => void;
  private expires = 0;
  private timer: number | undefined;
  private closed = false;

  constructor(done: () => void) {
    this.done = done;
    this.restoreFocus = document.activeElement;

    this.scrim = document.createElement("div");
    this.scrim.className = "dp-scrim";
    const panel = document.createElement("div");
    panel.className = "dp iphone-setup";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-label", "Set Up iPhone");

    const header = document.createElement("div");
    header.className = "dp-head";
    const title = document.createElement("span");
    title.className = "dp-title";
    title.textContent = "Set up iPhone";
    header.append(title);

    this.about = document.createElement("div");
    this.about.className = "is-about";
    this.about.textContent = "Making the code…";

    this.code = document.createElement("div");
    this.code.className = "is-code";

    const warning = document.createElement("div");
    warning.className = "is-warning";
    warning.textContent =
      "The code holds the password. Anyone who sees or photographs it can read this account's mail. Close this once the iPhone is set up.";

    this.countdown = document.createElement("div");
    this.countdown.className = "is-countdown";

    const foot = document.createElement("div");
    foot.className = "dp-foot";
    foot.textContent = "esc  close";

    panel.append(header, this.about, this.code, warning, this.countdown, foot);
    this.scrim.append(panel);
    document.body.append(this.scrim);

    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.close();
    });
    this.onKeyDown = (e) => {
      e.stopPropagation();
      if (e.key === "Escape") {
        e.preventDefault();
        this.close();
      }
    };
    window.addEventListener("keydown", this.onKeyDown, true);
    void this.load();
  }

  /** Fetch a code, and a new one each time the last expires. */
  private async load(): Promise<void> {
    try {
      const code = await invoke<SetupCode>("iphone_setup_code");
      if (this.closed) return;
      this.expires = code.expires;
      this.code.innerHTML = code.svg;
      this.about.textContent = `Scan this with Orb on the iPhone: “Scan the Code on Your Mac”. It sets up ${code.login}, folder “${code.folder}”.`;
      this.tick();
    } catch (error) {
      if (this.closed) return;
      this.code.replaceChildren();
      this.about.textContent = `No code: ${String(error)}`;
      this.countdown.textContent = "";
    }
  }

  private tick(): void {
    window.clearTimeout(this.timer);
    if (this.closed) return;
    const left = this.expires - Math.floor(Date.now() / 1000);
    if (left <= 0) {
      this.code.replaceChildren();
      this.countdown.textContent = "Making a new code…";
      void this.load();
      return;
    }
    const minutes = Math.floor(left / 60);
    const seconds = String(left % 60).padStart(2, "0");
    this.countdown.textContent = `This code works for ${minutes}:${seconds} more`;
    this.timer = window.setTimeout(() => this.tick(), 1000);
  }

  private close(): void {
    if (this.closed) return;
    this.closed = true;
    window.clearTimeout(this.timer);
    window.removeEventListener("keydown", this.onKeyDown, true);
    this.code.replaceChildren();
    this.scrim.remove();
    if (this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus();
    this.done();
  }
}
