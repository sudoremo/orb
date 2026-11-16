/**
 * The template picker: every mail template the account may use, filtered as
 * typed, beside the one under the cursor as it would be inserted.
 *
 * Opened over a message already under way — a reply, a forward or a new
 * message — rather than before it, because what a template fills in is known
 * by then: who it is to, and so the first name it greets. Built on the date
 * picker's scrim and its rules about the keyboard, like the list picker: it
 * owns the keyboard while open and resolves `null` for every way out that
 * chooses nothing.
 */

import { invoke } from "@tauri-apps/api/core";
import { escape } from "./format";
import { svgIcon } from "./icons";
import { eventKey } from "./keys";
import { fuzzyScore } from "./fuzzy";
import { braced, Expanded } from "./placeholders";

/** A template as the core lists it for a draft. */
export interface TemplateInfo {
  name: string;
  subject: string | null;
  accounts: string[];
  body: string;
  path: string;
  placeholders: string[];
  expanded: Expanded;
  suggestions: Record<string, string>;
}

interface TemplateList {
  folder: string;
  templates: TemplateInfo[];
  problems: { path: string; message: string }[];
}

/** What was chosen: a template to put into the draft, or its file to edit. */
export interface TemplateChoice {
  path: string;
  action: "insert" | "edit";
}

export interface TemplatePickerOptions {
  account: string;
  to: string[];
  /** Whether the message is a reply or a forward: the template goes above the quote. */
  reply: boolean;
  /** The draft's subject as it stands; a template's only fills an empty one. */
  subject: string;
  /** Whether ⌘E can open the file: only Neovim can. */
  editable: boolean;
}

let current: TemplatePicker | null = null;

/** Whether the picker owns the keyboard. */
export function isTemplatePickerOpen(): boolean {
  return current !== null;
}

/** Open the picker; resolves with what was chosen, or `null`. */
export async function openTemplatePicker(options: TemplatePickerOptions): Promise<TemplateChoice | null> {
  current?.finish(null);
  const list = await invoke<TemplateList>("templates", { account: options.account, to: options.to });
  return new Promise((resolve) => {
    current = new TemplatePicker(options, list, (choice) => {
      current = null;
      resolve(choice);
    });
  });
}

/** A placeholder in the preview: filled when Orb knows its value. */
function chip(name: string, value: string | undefined): string {
  return value
    ? `<span class="ph ok" title="${escape(braced(name))}">${escape(value)}</span>`
    : `<span class="ph">${escape(braced(name))}</span>`;
}

/** `text`'s lines as HTML, each placeholder a chip. */
function previewLines(text: Expanded, values: Record<string, string>): string {
  return text.lines
    .map((line, index) => {
      const marks = text.marks.filter((m) => m.line === index).sort((a, b) => a.start16 - b.start16);
      let html = "";
      let at = 0;
      for (const m of marks) {
        html += escape(line.slice(at, m.start16)) + chip(m.name, values[m.name]);
        at = m.end16;
      }
      html += escape(line.slice(at));
      return `<div class="tpk-ln">${html || "&nbsp;"}</div>`;
    })
    .join("");
}

class TemplatePicker {
  private readonly scrim: HTMLDivElement;
  private readonly input: HTMLInputElement;
  private readonly listEl: HTMLDivElement;
  private readonly preview: HTMLDivElement;
  private readonly foot: HTMLDivElement;
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus: Element | null;
  private shown: TemplateInfo[] = [];
  private highlighted = 0;
  private closed = false;

  constructor(
    private readonly options: TemplatePickerOptions,
    private readonly list: TemplateList,
    private readonly done: (choice: TemplateChoice | null) => void,
  ) {
    this.restoreFocus = document.activeElement;
    this.scrim = document.createElement("div");
    this.scrim.className = "dp-scrim tpk-scrim";
    const panel = document.createElement("div");
    panel.className = "tpk";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-label", "Templates");
    panel.innerHTML = `<div class="tpk-head">${svgIcon("tpl")}<input class="tpk-input" type="text" spellcheck="false" autocomplete="off" placeholder="Template"><span class="tpk-count"></span><kbd>esc</kbd></div>
      <div class="tpk-list" role="listbox"></div><div class="tpk-preview"></div><div class="tpk-foot"></div>`;
    this.scrim.append(panel);
    document.body.append(this.scrim);
    this.input = panel.querySelector(".tpk-input")!;
    this.listEl = panel.querySelector(".tpk-list")!;
    this.preview = panel.querySelector(".tpk-preview")!;
    this.foot = panel.querySelector(".tpk-foot")!;

    this.input.addEventListener("input", () => this.filter());
    this.listEl.addEventListener("mousedown", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-index]");
      if (!row) return;
      e.preventDefault();
      this.highlighted = Number(row.dataset.index);
      this.render();
    });
    this.listEl.addEventListener("dblclick", (e) => {
      if ((e.target as HTMLElement).closest("[data-index]")) this.accept("insert");
    });
    this.foot.addEventListener("click", (e) => {
      const action = (e.target as HTMLElement).closest<HTMLElement>("[data-action]")?.dataset.action;
      if (action === "insert" || action === "edit") this.accept(action);
    });
    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.finish(null);
    });
    this.onKeyDown = (e) => this.handleKey(e);
    window.addEventListener("keydown", this.onKeyDown, true);
    this.filter();
    this.input.focus();
  }

  private filter(): void {
    const query = this.input.value.trim();
    const scored = this.list.templates
      .map((t, order) => ({ t, order, score: fuzzyScore(query, `${t.name} ${t.subject ?? ""}`) }))
      .filter((s): s is { t: TemplateInfo; order: number; score: number } => s.score !== null);
    // Typing ranks by the match; until then, the account's own come first.
    if (query) scored.sort((a, b) => a.score - b.score || a.order - b.order);
    this.shown = scored.map((s) => s.t);
    this.highlighted = 0;
    this.render();
  }

  /** The heading a template is listed under. */
  private group(t: TemplateInfo): string {
    return t.accounts.length ? this.options.account : "Every account";
  }

  private render(): void {
    const total = this.list.templates.length;
    this.scrim.querySelector(".tpk-count")!.textContent = total
      ? this.input.value.trim()
        ? `${this.shown.length} of ${total}`
        : `${total} template${total === 1 ? "" : "s"}`
      : "";
    const query = this.input.value.trim();
    let heading = "";
    const rows = this.shown.map((t, i) => {
      const group = this.group(t);
      const head = !query && group !== heading ? `<div class="tpk-h">${escape(group)}</div>` : "";
      heading = group;
      const n = t.placeholders.length;
      return `${head}<div class="tpk-i${i === this.highlighted ? " on" : ""}" data-index="${i}" role="option" aria-selected="${i === this.highlighted}">
          ${svgIcon("tpl")}<span class="tpk-name">${escape(t.name)}</span>${n ? `<span class="tpk-n" title="${n} placeholder${n === 1 ? "" : "s"}">${n} {}</span>` : "<span></span>"}
          ${t.subject ? `<span class="tpk-s">${escape(t.subject)}</span>` : ""}</div>`;
    });
    const problems = this.list.problems.map(
      (p) => `<div class="tpk-i broken" title="${escape(p.message)}">${svgIcon("alert")}<span class="tpk-name">${escape(p.path.split("/").pop() ?? p.path)}</span><span></span><span class="tpk-s">${escape(p.message)}</span></div>`,
    );
    if (!total && !problems.length) {
      this.listEl.innerHTML = `<div class="tpk-empty">No templates yet.</div>`;
    } else if (!this.shown.length && !problems.length) {
      this.listEl.innerHTML = `<div class="tpk-empty">No template matches.</div>`;
    } else {
      this.listEl.innerHTML = rows.join("") + (problems.length ? `<div class="tpk-h">Could not be read</div>${problems.join("")}` : "");
    }
    this.listEl.querySelector(".tpk-i.on")?.scrollIntoView({ block: "nearest" });
    this.renderPreview();
  }

  private renderPreview(): void {
    const t = this.shown[this.highlighted];
    if (!t) {
      this.preview.innerHTML = this.list.templates.length
        ? ""
        : `<div class="tpk-about"><p>A template is a YAML file in <code>${escape(this.list.folder)}</code>:</p>
           <pre>name: Thanks for the introduction
subject: Great to meet you
accounts: [Work]      # optional; every account without it
body: |
  Hi {Vorname},

  thanks for …</pre><p>A word in braces is a placeholder, filled in after the template is inserted.</p></div>`;
      this.foot.innerHTML = `<span class="tpk-path">${escape(this.list.folder)}</span>`;
      return;
    }
    const known = t.placeholders.filter((n) => t.suggestions[n]);
    const open = t.placeholders.filter((n) => !t.suggestions[n]);
    const own = this.options.subject.trim();
    const subject = own
      ? `<span class="dim">stays</span> ${escape(own)}`
      : t.subject
        ? escape(t.subject)
        : `<span class="dim">none</span>`;
    const fills = t.placeholders.length
      ? `<div class="tpk-fill"><span class="k">Fill in</span>${known
          .map((n) => `${chip(n, t.suggestions[n])}<span class="dim">${escape(n)}, from To</span>`)
          .join(" ")} ${open.map((n) => chip(n, undefined)).join(" ")}</div>`
      : "";
    this.preview.innerHTML = `<div class="tpk-ph"><div class="tpk-t">${escape(t.name)}</div>
        <div class="tpk-kv"><span class="k">Subject</span><span>${subject}</span>
          <span class="k">Goes</span><span>${this.options.reply ? "above the quote, where the cursor is" : "where the cursor is"}</span></div>${fills}</div>
      <div class="tpk-body">${previewLines(t.expanded, t.suggestions)}</div>`;
    this.foot.innerHTML = `<span class="tpk-path" title="${escape(t.path)}">${escape(t.path)}</span>
      <button type="button" data-action="insert"><kbd>↩</kbd> Insert</button>
      ${this.options.editable ? `<button type="button" data-action="edit"><kbd>⌘E</kbd> Edit the file</button>` : ""}`;
  }

  private move(delta: number): void {
    if (!this.shown.length) return;
    this.highlighted = (this.highlighted + delta + this.shown.length) % this.shown.length;
    this.render();
  }

  private accept(action: "insert" | "edit"): void {
    const t = this.shown[this.highlighted];
    if (!t || (action === "edit" && !this.options.editable)) return;
    this.finish({ path: t.path, action });
  }

  private handleKey(e: KeyboardEvent): void {
    const key = eventKey(e);
    const take = () => {
      e.preventDefault();
      e.stopPropagation();
    };
    switch (key) {
      case "<esc>":
        take();
        this.finish(null);
        return;
      case "<enter>":
        take();
        this.accept("insert");
        return;
      case "<d-e>":
        take();
        this.accept("edit");
        return;
      case "<down>":
      case "<c-n>":
      case "<tab>":
        take();
        this.move(1);
        return;
      case "<up>":
      case "<c-p>":
      case "<s-tab>":
        take();
        this.move(-1);
        return;
      default:
        // Typing, which belongs to the field; kept from the window's keys.
        e.stopPropagation();
    }
  }

  finish(choice: TemplateChoice | null): void {
    if (this.closed) return;
    this.closed = true;
    window.removeEventListener("keydown", this.onKeyDown, true);
    this.scrim.remove();
    if (!choice && this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus();
    this.done(choice);
  }
}
