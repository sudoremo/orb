/**
 * One draft, one window.
 *
 * Each draft gets a real window rather than a tab Orb draws itself, because
 * macOS already groups windows that share a tabbing identifier: the tab bar,
 * dragging a tab out into its own window, and the shortcuts for moving between
 * them all come from the system. A tab strip drawn here would be less capable
 * and would still need real windows the moment one had to be detached.
 *
 * The window owns the envelope — recipients, subject, attachments, sending —
 * and the body is written in Neovim, which is the arrangement the client was
 * built around.
 */

import { escape, parseFrom } from "./format";
import { svgIcon } from "./icons";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { pickFiles } from "./files";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Draft, draftProblems } from "./draft";
import { TokenField, TokenGroup } from "./tokens";
import { bindTabShortcuts } from "./tabs";
import { installPasswordPrompt, isPasswordPromptOpen } from "./password";
import { Editor, startEditor } from "./editor";
import { canonicalTable, Direction, eventKey, navigationDirection } from "./keys";
import "./styles.css";
import { followAppearance } from "./windowlook";
import { followTitleBar } from "./chrome";
import { isDatePickerOpen, openDatePicker } from "./datepicker";
import { isPickerOpen, openPicker } from "./picker";
import { localToday } from "./dates";
import { formatCivilDate } from "./tasklist";
import { isTemplatePickerOpen, openTemplatePicker } from "./templatepicker";
import { braced, Expanded, fillPlain, followEdit, insertPlain, render, Span } from "./placeholders";


const el = {
  identity: document.getElementById("identity") as HTMLButtonElement,
  fromAccount: document.getElementById("from-account")!,
  to: document.getElementById("to") as HTMLElement,
  cc: document.getElementById("cc") as HTMLElement,
  bcc: document.getElementById("bcc") as HTMLElement,
  bccRow: document.getElementById("bcc-row")!,
  showBcc: document.getElementById("show-bcc") as HTMLButtonElement,
  subject: document.getElementById("subject") as HTMLInputElement,
  editor: document.getElementById("editor")!,
  plain: document.getElementById("plain") as HTMLTextAreaElement,
  attachments: document.getElementById("attachments")!,
  filesRow: document.getElementById("files-row")!,
  attach: document.getElementById("attach") as HTMLButtonElement,
  discard: document.getElementById("discard") as HTMLButtonElement,
  saveClose: document.getElementById("save-close") as HTMLButtonElement,
  warningsRow: document.getElementById("warnings-row")!,
  warnings: document.getElementById("warnings")!,
  followUp: document.getElementById("follow-up") as HTMLButtonElement,
  template: document.getElementById("template") as HTMLButtonElement,
  fillRow: document.getElementById("fill-row")!,
  fillFields: document.getElementById("fill-fields")!,
  fillLeft: document.getElementById("fill-left")!,
  toggleEditor: document.getElementById("toggle-editor") as HTMLButtonElement,
  send: document.getElementById("send") as HTMLButtonElement,
  status: document.getElementById("compose-status")!,
  title: document.getElementById("compose-title")!,
};

/**
 * What this window is writing.
 *
 * `savedUid` is where the draft lives on the server. It is how a later save
 * replaces the previous one rather than leaving a trail of near-identical
 * drafts behind — IMAP cannot update a message, so a save is an append and a
 * delete.
 */
const draft: Draft & {
  account: string;
  /** The name of the identity it is written as; empty for the account's own. */
  identity: string;
  savedUid: number | null;
  /** The Outbox message this was opened from, until it is saved or sent. */
  outboxId: number | null;
  attachments: string[];
} = {
  account: "",
  identity: "",
  attachments: [],
  to: [],
  cc: [],
  bcc: [],
  subject: "",
  body: "",
  savedUid: null,
  outboxId: null,
};

/**
 * Take the message this window was opened from out of the Outbox: it is in
 * Drafts now, or sent. Not before, so a window closed or failing to save
 * leaves it where it was.
 */
async function releaseOutbox(): Promise<void> {
  if (draft.outboxId === null) return;
  const id = draft.outboxId;
  draft.outboxId = null;
  await invoke("discard_outbox", { id }).catch(() => {});
}

/**
 * The recipient fields.
 *
 * Tokens rather than text: a recipient is one thing to select and delete, not a
 * run of characters between two commas. Each change saves, so closing the
 * window never loses who the message was for.
 */
const fields = {
  to: recipientField(el.to),
  cc: recipientField(el.cc),
  bcc: recipientField(el.bcc),
};
new TokenGroup(
  [
    { field: fields.to },
    { field: fields.cc },
    {
      field: fields.bcc,
      hidden: () => el.bccRow.hidden,
      reveal: () => showBcc(false),
      conceal: () => {
        el.bccRow.hidden = true;
        el.showBcc.hidden = false;
      },
    },
  ],
  recipientsChanged,
);

function recipientsChanged(): void {
  void save("recipient");
  void refreshSuggestions();
}

function recipientField(host: HTMLElement): TokenField {
  const field = new TokenField(
    host,
    recipientsChanged,
    (reason) => setStatus(`no suggestions: ${reason}`),
  );
  // The row is what looks clickable — the label, the padding and the empty
  // space after the last recipient all read as "the To field". Only the input
  // itself actually took the keyboard, so clicking the wrong half of a row
  // left the typing to go wherever it had been going before.
  host.parentElement?.addEventListener("mousedown", (e) => {
    if ((e.target as HTMLElement).closest("button, .token, .completions")) return;
    e.preventDefault();
    field.focus();
  });
  return field;
}

/** An address Orb sends as, and the account it goes out of. */
interface Identity {
  account: string;
  name: string;
  from: string;
  /** Its signature from the `-- ` line on, or null for none. */
  signature: string | null;
}

/** Every account's identities, each account's default one first. */
let identities: Identity[] = [];

/** The identity the draft is written as. */
function currentIdentity(): Identity | undefined {
  const own = identities.filter((i) => i.account === draft.account);
  return own.find((i) => i.name === draft.identity) ?? own[0];
}

/** Where an identity goes out of, as the From line names it. */
function senderLabel(identity: Identity): string {
  return identity.name === identity.account ? identity.account : `${identity.account} / ${identity.name}`;
}

/**
 * Show who the message is from, with the account it goes out of at the end
 * of the line; the address opens the list of senders when there is another.
 */
function renderIdentity(): void {
  const current = currentIdentity();
  if (!current) {
    el.identity.textContent = "";
    el.fromAccount.textContent = draft.account;
    return;
  }
  const { name, email } = parseFrom(current.from);
  const several = identities.length > 1;
  el.identity.innerHTML = `${escape(name)}${email ? ` <span class="addr">${escape(email)}</span>` : ""}${
    several ? svgIcon("chev", "chev") : ""
  }`;
  el.identity.disabled = !several;
  el.identity.title = several ? `${current.from} · choose the sender (⌘I)` : current.from;
  el.fromAccount.textContent = senderLabel(current);
}

/** Choose who the message is from, among every account's identities. */
async function pickSender(): Promise<void> {
  if (finished) return;
  if (identities.length < 2) {
    setStatus("no other identity is configured");
    return;
  }
  const current = currentIdentity();
  const key = (i: Identity) => `${i.account}\n${i.name}`;
  // The current one first, so Enter alone changes nothing.
  const ordered = [...identities].sort((a, b) => Number(b === current) - Number(a === current));
  const chosen = await openPicker({
    title: "From",
    placeholder: "Name, address or account",
    items: ordered.map((i) => ({
      value: key(i),
      label: i.from,
      detail: senderLabel(i),
      keywords: senderLabel(i),
    })),
  });
  const next = identities.find((i) => key(i) === chosen);
  if (next && next !== current) await writeAs(next);
}

/** Write as the next identity, through every account's. */
async function cycleIdentity(): Promise<void> {
  if (identities.length < 2) {
    setStatus("no other identity is configured");
    return;
  }
  const current = currentIdentity();
  const index = current ? identities.indexOf(current) : -1;
  await writeAs(identities[(index + 1) % identities.length]);
}

/**
 * Write as `next` from here on.
 *
 * The signature changes with it, in whichever editor has the body: the core
 * finds the old identity's signature and puts the new one in its place, so
 * switching back and forth leaves what was written alone.
 *
 * Another account's identity moves the draft to that account's Drafts, where
 * it will be sent from. The old copy is removed only once the new one is
 * saved, so a save that fails leaves two copies rather than none.
 */
async function writeAs(next: Identity): Promise<void> {
  const previous = currentIdentity();
  if (finished || !previous) return;
  try {
    const body = editor ? await editor.buffer() : el.plain.value;
    const swapped = await invoke<string>("switch_identity", {
      account: previous.account,
      from: previous.name,
      toAccount: next.account,
      to: next.name,
      body,
    });
    if (swapped !== body) {
      if (editor) await editor.setBuffer(swapped, next.signature);
      else el.plain.value = swapped;
      endFilling();
    }
    // A save in flight still goes to the account it started for.
    await saveChain;
    const left = next.account === draft.account ? null : { account: draft.account, uid: draft.savedUid };
    draft.account = next.account;
    draft.identity = next.name;
    if (left) {
      draft.savedUid = null;
      lastSaved = "";
    }
    renderIdentity();
    await save(left ? `moved to ${next.account}` : "identity changed");
    if (left?.uid != null && draft.savedUid !== null) {
      await invoke("discard_draft", { account: left.account, uid: left.uid }).catch((e) =>
        setStatus(`the copy in ${left.account}'s Drafts is still there: ${describe(e)}`),
      );
    }
    await refreshWarnings();
  } catch (e) {
    setStatus(`could not switch identity: ${describe(e)}`);
  }
}

/** What the core warns about in the draft, as it stood when last asked. */
interface Warning {
  kind: string;
  message: string;
  /** For a `placeholder` warning: which, and the body's line it is on (absent for the subject). */
  placeholder?: string;
  line?: number;
}

let warnings: Warning[] = [];

/** Ask the core what is worth a second look about the draft as it stands. */
async function refreshWarnings(): Promise<Warning[]> {
  try {
    warnings = await invoke<Warning[]>("draft_warnings", {
      input: {
        account_name: draft.account,
        identity: draft.identity || null,
        to: draft.to,
        cc: draft.cc,
        bcc: draft.bcc,
        subject: draft.subject,
        body: draft.body,
        attachment_paths: draft.attachments,
        placeholders: usedPlaceholders,
        addressed_to: draft.addressedTo ?? [],
        delivered_to: draft.deliveredTo ?? [],
      },
    });
  } catch {
    // Warnings help; a draft that cannot be checked is still sent.
    warnings = [];
  }
  renderWarnings();
  return warnings;
}

function renderWarnings(): void {
  el.warningsRow.hidden = warnings.length === 0;
  // The placeholders on one line, however many there are.
  const open = warnings.filter((w) => w.kind === "placeholder");
  const rest = warnings.filter((w) => w.kind !== "placeholder");
  const line = (kind: string, text: string) =>
    `<li data-kind="${escape(kind)}">${svgIcon("alert")}<span>${escape(text)}</span></li>`;
  el.warnings.innerHTML =
    (open.length
      ? line(
          "placeholder",
          `Not filled in: ${open
            .map((w) => `${braced(w.placeholder ?? "")} ${w.line ? `(line ${w.line})` : "(subject)"}`)
            .join(", ")}`,
        )
      : "") + rest.map((w) => line(w.kind, w.message)).join("");
}

/**
 * The window's own keys, from `bindings.compose_normal` over the built-in
 * ones. Only the commands the window knows are taken from the keyboard; any
 * other entry in the table is left to whatever has focus.
 */
let bindings: Record<string, string> = {
  "<d-enter>": "send",
  "<d-s>": "save",
  "<d-i>": "pick-sender",
  "<d-s-s>": "save-and-close",
  "<c-t>": "follow-up",
  "<d-t>": "template",
  "<c-j>": "next-field",
  "<c-k>": "prev-field",
};

const COMPOSE_COMMANDS: Record<string, () => Promise<void>> = {
  send,
  save: () => save("by hand"),
  "pick-sender": pickSender,
  "cycle-identity": cycleIdentity,
  "save-and-close": saveAndClose,
  discard,
  "follow-up": pickFollowUp,
  template: pickTemplate,
  "next-field": async () => moveFocus(1),
  "prev-field": async () => moveFocus(-1),
};

/**
 * The header's lines and the body, top to bottom, as the keyboard visits
 * them: each with the element that holds its focus and a way to give it.
 * From only while there is another identity to choose, Bcc only while it
 * is shown.
 */
function focusStops(): { host: HTMLElement; focus: () => void }[] {
  const stops: { host: HTMLElement; focus: () => void }[] = [];
  if (!el.identity.disabled) stops.push({ host: el.identity, focus: () => el.identity.focus() });
  stops.push({ host: el.to, focus: () => fields.to.focus() });
  stops.push({ host: el.cc, focus: () => fields.cc.focus() });
  if (!el.bccRow.hidden) stops.push({ host: el.bcc, focus: () => fields.bcc.focus() });
  stops.push({ host: el.subject, focus: () => el.subject.focus() });
  if (!el.fillRow.hidden) {
    stops.push({ host: el.fillRow, focus: () => el.fillFields.querySelector<HTMLInputElement>("input")?.focus() });
  }
  if (editor) stops.push({ host: el.editor, focus: () => editor?.focus() });
  else stops.push({ host: el.plain, focus: () => el.plain.focus() });
  return stops;
}

/** Give the keyboard to the line above or below the one that has it. */
function moveFocus(step: 1 | -1): void {
  const stops = focusStops();
  const active = document.activeElement;
  const current = stops.findIndex((s) => active && s.host.contains(active));
  // Nothing in the header or the body has the keyboard: start from the end the
  // step leads away from.
  const next = current < 0 ? (step > 0 ? 0 : stops.length - 1) : current + step;
  const stop = stops[next];
  if (stop) stop.focus();
  else escalate(step > 0 ? "down" : "up");
}

/** Leave the window for whatever lies that way, by the escalate command. */
function escalate(direction: Direction): void {
  invoke("escalate", { direction }).catch((e) => setStatus(describe(e)));
}

function describe(e: unknown): string {
  return typeof e === "string" ? e : ((e as Error)?.message ?? JSON.stringify(e));
}

/**
 * Say how the draft stands, beside Send. A problem is said in the warning
 * colour, and in full on hover when the title bar cuts it short.
 */
function setStatus(text: string, problem = true): void {
  el.status.textContent = text;
  el.status.title = text;
  el.status.classList.toggle("warn", problem);
}

/**
 * The day to follow the message up on, or null for none: once it is sent, a
 * task linked to it is made, postponed to that day. Kept by the window, not
 * by the draft.
 */
let followUp: string | null = null;

async function pickFollowUp(): Promise<void> {
  if (finished) return;
  if (!(await invoke<boolean>("tasks_configured").catch(() => false))) {
    setStatus("a follow-up needs a tasks section in config.yml: an account and a folder");
    return;
  }
  const chosen = await openDatePicker({
    title: "Follow up on",
    initial: followUp ?? localToday(),
    onProblem: (reason) => setStatus(reason),
  });
  if (!chosen) return;
  followUp = chosen.kind === "clear" ? null : chosen.date;
  renderFollowUp();
}

function renderFollowUp(): void {
  const label = followUp ? `Follow up ${formatCivilDate(followUp, localToday())}` : "Follow up";
  el.followUp.innerHTML = `${svgIcon("task")}<span>${escape(label)}</span>`;
  el.followUp.classList.toggle("on", followUp !== null);
  el.followUp.title = followUp
    ? `A task for this message, postponed to ${followUp}, once it is sent`
    : "Make a task for this message once it is sent";
}

/** A template as the core hands it over to be put into the draft. */
interface TemplateUse {
  name: string;
  subject: Expanded | null;
  body: Expanded;
  placeholders: string[];
  suggestions: Record<string, string>;
}

/**
 * The template last put into the draft, while its placeholders are filled
 * in from the Fill in line. Gone once they all are and the keyboard has left
 * the line, or once the body is replaced as a whole.
 */
interface Filling {
  name: string;
  placeholders: string[];
  values: Record<string, string>;
  /** The values Orb filled in rather than the writer, which follow To. */
  suggested: Set<string>;
  /** The template's subject, when it filled an empty one, and what was last written there. */
  subject: { text: Expanded; written: string } | null;
}

let filling: Filling | null = null;

/**
 * Every placeholder of every template put into the draft: what Send checks
 * for still being in braces. Only these, so a brace typed by hand, or one in
 * a quoted message, is never taken for a placeholder.
 */
let usedPlaceholders: string[] = [];

/** The placeholders' spans in the plain field, while it holds the body. */
let spans: Span[] = [];

/** The plain field's text as the spans last saw it. */
let plainBefore = "";

/** Fills for Neovim, one after another, so a fast typist's keys land in order. */
let fillChain: Promise<void> = Promise.resolve();

async function pickTemplate(): Promise<void> {
  if (finished) return;
  readFields();
  const choice = await openTemplatePicker({
    account: draft.account,
    to: draft.to,
    reply: Boolean(draft.inReplyTo || draft.forwards),
    subject: draft.subject,
    editable: editor !== null,
  }).catch((e) => {
    setStatus(`could not read the templates: ${describe(e)}`);
    return null;
  });
  if (!choice) {
    // Opened from the button, which has the keyboard now: back to the text.
    if (document.activeElement === el.template || document.activeElement === document.body) focusBody();
    return;
  }
  if (choice.action === "edit") {
    await editor?.editFile(choice.path).catch((e) => setStatus(`could not open the template: ${describe(e)}`));
    editor?.focus();
    return;
  }
  let used: TemplateUse;
  try {
    used = await invoke<TemplateUse>("use_template", { path: choice.path, to: draft.to });
  } catch (e) {
    setStatus(`could not use the template: ${describe(e)}`);
    focusBody();
    return;
  }
  await insertTemplate(used);
}

/** Put `used` into the draft at the cursor, and open the Fill in line for it. */
async function insertTemplate(used: TemplateUse): Promise<void> {
  if (editor) {
    const done = await editor.insertTemplate(used.body).catch(() => false);
    if (!done) {
      setStatus("could not insert the template: Neovim has no draft open");
      return;
    }
  } else {
    const before = el.plain.value;
    const inserted = insertPlain(before, el.plain.selectionStart, used.body);
    spans = [...followEdit(spans, before, inserted.value), ...inserted.spans];
    el.plain.value = inserted.value;
    plainBefore = inserted.value;
    el.plain.setSelectionRange(inserted.caret, inserted.caret);
  }

  // A template's subject only fills an empty one: a reply keeps its own.
  let subject: Filling["subject"] = null;
  if (used.subject && !el.subject.value.trim()) {
    const written = render(used.subject, {}).join(" ");
    el.subject.value = written;
    draft.subject = written;
    retitle();
    subject = { text: used.subject, written };
  }

  usedPlaceholders = [...new Set([...usedPlaceholders, ...used.placeholders])];
  filling = { name: used.name, placeholders: used.placeholders, values: {}, suggested: new Set(), subject };
  for (const [name, value] of Object.entries(used.suggestions)) {
    filling.suggested.add(name);
    applyValue(name, value);
  }
  renderTemplateButton();
  renderFill();
  setStatus(`Inserted “${used.name}”`, false);
  const first = el.fillFields.querySelector<HTMLInputElement>(".pf:not(.ok) input");
  if (first) first.focus();
  else await toBody();
  scheduleWarnings();
}

/** Write `value` wherever the placeholder stands, in the body and the subject. */
function applyValue(name: string, value: string): void {
  if (!filling) return;
  filling.values[name] = value;
  if (editor) {
    const target = editor;
    fillChain = fillChain.then(() => target.fillPlaceholder(name, value)).catch(() => {});
  } else {
    const { selectionStart, selectionEnd } = el.plain;
    const filled = fillPlain(el.plain.value, spans, name, value);
    el.plain.value = filled.text;
    spans = filled.spans;
    plainBefore = filled.text;
    el.plain.setSelectionRange(selectionStart, selectionEnd);
  }
  // The subject follows as long as nobody has written in it since.
  if (filling.subject && el.subject.value === filling.subject.written) {
    const written = render(filling.subject.text, filling.values).join(" ");
    el.subject.value = written;
    filling.subject.written = written;
    draft.subject = written;
    retitle();
  }
}

/** The Fill in line: one field per placeholder of the template, in order. */
function renderFill(): void {
  const names = filling?.placeholders ?? [];
  el.fillRow.hidden = names.length === 0;
  if (!filling || !names.length) {
    el.fillFields.innerHTML = "";
    el.fillLeft.textContent = "";
    return;
  }
  const values = filling.values;
  el.fillFields.innerHTML = names
    .map(
      (name) => `<label class="pf">
        <span class="lb">${svgIcon("check")}<span>${escape(name)}</span><span class="from">· from To</span></span>
        <input type="text" data-name="${escape(name)}" value="${escape(values[name] ?? "")}" placeholder="Not filled in" spellcheck="false" autocomplete="off">
      </label>`,
    )
    .join("");
  updateFill();
}

/** Mark each field filled or not, and count what is left. */
function updateFill(): void {
  if (!filling) return;
  const names = filling.placeholders;
  for (const label of el.fillFields.querySelectorAll<HTMLElement>(".pf")) {
    const name = label.querySelector("input")!.dataset.name!;
    const value = filling.values[name] ?? "";
    label.classList.toggle("ok", value !== "");
    label.classList.toggle("todo", value === "");
    label.classList.toggle("auto", filling.suggested.has(name) && value !== "");
  }
  // Each field as wide as what is typed into it, so a date reads whole.
  for (const input of el.fillFields.querySelectorAll("input")) {
    input.size = Math.max(16, Math.min(48, input.value.length + 2));
  }
  const left = names.filter((n) => !filling!.values[n]).length;
  el.fillLeft.innerHTML = left
    ? `<span class="left">${left} of ${names.length} left</span><span class="keys"><kbd>⇥</kbd> next <kbd>↩</kbd> to the text</span>`
    : `<span class="keys">All filled in · <kbd>↩</kbd> to the text</span>`;
}

/**
 * Hand the keyboard to the body, at the first placeholder still open. With
 * none left, the Fill in line has done its work and folds away.
 */
async function toBody(): Promise<void> {
  const open = filling?.placeholders.find((n) => !filling!.values[n]);
  if (!open && filling) {
    filling = null;
    renderFill();
  }
  if (open) await goToPlaceholder(open);
  else focusBody();
}

/** Put the cursor where `name` first stands in the body, and the keyboard there. */
async function goToPlaceholder(name: string): Promise<void> {
  if (editor) {
    await editor.gotoPlaceholder(name).catch(() => false);
    editor.focus();
    return;
  }
  const span = spans.find((s) => s.name === name && el.plain.value.slice(s.start, s.end) === braced(name));
  const at = span?.start ?? el.plain.value.indexOf(braced(name));
  el.plain.focus();
  if (at >= 0) el.plain.setSelectionRange(at, at + braced(name).length);
}

/** Close the Fill in line, for when the body was replaced and the placeholders' places are lost. */
function endFilling(): void {
  spans = [];
  if (!filling) return;
  filling = null;
  renderFill();
  setStatus("the Fill in line closed with the body; placeholders still in braces are checked before sending", false);
}

/** The names Orb fills in follow the first recipient, until the writer types over them. */
async function refreshSuggestions(): Promise<void> {
  if (!filling) return;
  const names = filling.placeholders.filter((n) => filling!.suggested.has(n) || !filling!.values[n]);
  if (!names.length) return;
  const suggested = await invoke<Record<string, string>>("template_suggestions", {
    names,
    to: fields.to.value,
  }).catch(() => ({}) as Record<string, string>);
  if (!filling) return;
  for (const name of names) {
    const value = suggested[name] ?? (filling.suggested.has(name) ? "" : undefined);
    if (value === undefined || value === (filling.values[name] ?? "")) continue;
    if (value) filling.suggested.add(name);
    else filling.suggested.delete(name);
    applyValue(name, value);
    const input = el.fillFields.querySelector<HTMLInputElement>(`input[data-name="${CSS.escape(name)}"]`);
    if (input) input.value = value;
  }
  updateFill();
}

let warningsTimer: number | undefined;

/** Look at the draft again a moment after filling stops, so the Check line keeps up. */
function scheduleWarnings(): void {
  window.clearTimeout(warningsTimer);
  warningsTimer = window.setTimeout(() => {
    void (async () => {
      readFields();
      await fillChain;
      await collectBody().catch(() => {});
      await refreshWarnings();
    })();
  }, 400);
}

function renderTemplateButton(): void {
  const name = filling?.name;
  el.template.innerHTML = `${svgIcon("tpl")}<span>${escape(name ?? "Template")}</span>`;
  el.template.classList.toggle("on", Boolean(name));
  el.template.title = name ? `Inserted “${name}” · insert another (⌘T)` : "Insert a template (⌘T)";
}

el.fillFields.addEventListener("input", (e) => {
  const input = e.target as HTMLInputElement;
  const name = input.dataset.name;
  if (!filling || !name) return;
  filling.suggested.delete(name);
  applyValue(name, input.value);
  updateFill();
  scheduleWarnings();
});

el.fillFields.addEventListener("keydown", (e) => {
  const input = e.target as HTMLInputElement;
  if (!(input instanceof HTMLInputElement)) return;
  const inputs = [...el.fillFields.querySelectorAll("input")];
  const last = inputs.indexOf(input) === inputs.length - 1;
  if (e.key === "Enter" || e.key === "Escape" || (e.key === "Tab" && !e.shiftKey && last)) {
    e.preventDefault();
    e.stopPropagation();
    void toBody();
  }
});

/** Whether the sheet asking about open placeholders is up. */
let sheetOpen = false;

/**
 * Ask before sending past placeholders still in braces. Unlike the other
 * checks the safe answer is the default: Enter goes back to fill them in,
 * and sending anyway takes ⌥⌘Enter or a click. Resolves with what to do and,
 * for `fill`, the placeholder to go to.
 */
function askAboutPlaceholders(found: Warning[]): Promise<{ answer: "send" | "fill" | "cancel"; name?: string }> {
  const open = found.filter((w) => w.kind === "placeholder");
  const others = found.filter((w) => w.kind !== "placeholder");
  const lines = draft.body.split("\n");
  const who = draft.to.length + draft.cc.length + draft.bcc.length === 1
    ? (parseFrom([...draft.to, ...draft.cc, ...draft.bcc][0]).name || "The recipient")
    : "The recipients";
  const scrim = document.createElement("div");
  scrim.className = "sheet-scrim";
  scrim.innerHTML = `<div class="sheet" role="alertdialog" aria-modal="true" aria-labelledby="sheet-title">
      <div class="sheet-h"><span class="badge">${svgIcon("braces")}</span><div>
        <h3 id="sheet-title">${open.length === 1 ? "1 placeholder is" : `${open.length} placeholders are`} not filled in</h3>
        <p>${escape(who)} would get ${open.length === 1 ? "it" : "them"} as ${open.length === 1 ? "it stands" : "they stand"}, braces and all.</p></div></div>
      <div class="sheet-l">${open
        .map((w, i) => {
          const context = w.line ? lines[w.line - 1] ?? "" : draft.subject;
          return `<div class="it${i === 0 ? " on" : ""}" data-index="${i}"><span class="ph">${escape(braced(w.placeholder ?? ""))}</span>
            <span class="ctx">${escape(context.trim())}</span><span class="ln">${w.line ? `line ${w.line}` : "subject"}</span></div>`;
        })
        .join("")}</div>
      ${others.length ? `<ul class="sheet-o">${others.map((w) => `<li>${svgIcon("alert")}<span>${escape(w.message)}</span></li>`).join("")}</ul>` : ""}
      <div class="sheet-f"><button type="button" class="btn quiet" data-answer="send">Send anyway <kbd>⌥⌘↩</kbd></button>
        <button type="button" class="btn" data-answer="cancel">Cancel <kbd>esc</kbd></button>
        <button type="button" class="btn pri" data-answer="fill">Fill in <kbd>↩</kbd></button></div>
    </div>`;
  document.body.append(scrim);
  sheetOpen = true;
  let chosen = 0;
  const rows = [...scrim.querySelectorAll<HTMLElement>(".it")];
  const choose = (index: number) => {
    chosen = (index + rows.length) % rows.length;
    rows.forEach((r, i) => r.classList.toggle("on", i === chosen));
  };
  const restore = document.activeElement;
  (scrim.querySelector(".btn.pri") as HTMLButtonElement).focus();
  return new Promise((resolve) => {
    const finish = (answer: "send" | "fill" | "cancel") => {
      window.removeEventListener("keydown", onKey, true);
      scrim.remove();
      sheetOpen = false;
      if (answer === "cancel" && restore instanceof HTMLElement) restore.focus();
      resolve({ answer, name: open[chosen]?.placeholder });
    };
    const onKey = (e: KeyboardEvent) => {
      const take = () => {
        e.preventDefault();
        e.stopPropagation();
      };
      if (e.key === "Enter" && e.metaKey && e.altKey) {
        take();
        finish("send");
      } else if (e.key === "Enter") {
        take();
        finish("fill");
      } else if (e.key === "Escape") {
        take();
        finish("cancel");
      } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        take();
        choose(chosen + (e.key === "ArrowDown" ? 1 : -1));
      } else if (e.key !== "Tab") {
        take();
      }
    };
    window.addEventListener("keydown", onKey, true);
    scrim.addEventListener("click", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>(".it");
      if (row) choose(Number(row.dataset.index));
      const answer = (e.target as HTMLElement).closest<HTMLElement>("[data-answer]")?.dataset.answer;
      if (answer === "send" || answer === "fill" || answer === "cancel") finish(answer);
    });
  });
}

/** The draft's state once a save is through: `Saved 14:02`. */
function savedAt(): string {
  const now = new Date();
  return `Saved ${String(now.getHours()).padStart(2, "0")}:${String(now.getMinutes()).padStart(2, "0")}`;
}

/** The window's title, so a tab can be told from its neighbours. */
function retitle(): void {
  const name = draft.subject.trim() || "New message";
  void getCurrentWindow().setTitle(name);
  document.title = name;
  // The system draws no title in a title bar the content runs under.
  el.title.textContent = name;
}

/** Bcc is shown when it holds an address, or when asked for. */
function showBcc(focus: boolean): void {
  el.bccRow.hidden = false;
  el.showBcc.hidden = true;
  if (focus) fields.bcc.focus();
}

function readFields(): void {
  draft.to = fields.to.value;
  draft.cc = fields.cc.value;
  draft.bcc = fields.bcc.value;
  draft.subject = el.subject.value;
}

/** Show what is going out with the message, and let it be taken back off. */
function renderAttachments(): void {
  el.filesRow.hidden = draft.attachments.length === 0;
  el.attachments.innerHTML = draft.attachments
    .map((path, index) => {
      const name = path.split("/").pop() ?? path;
      return `<span class="file" title="${escape(path)}">
                ${svgIcon("clip")}<span>${escape(name)}</span>
                <button type="button" data-remove="${index}" title="Remove">✕</button>
              </span>`;
    })
    .join("");
}


async function pickAttachments(): Promise<void> {
  const picked = await pickFiles({ kind: "files", title: "Attach files" });
  if (picked.length === 0) return;
  // The files are read at send time rather than now: a draft saved today and
  // sent tomorrow should carry what the file says tomorrow, and holding the
  // contents in the window would mean holding them in memory until then.
  for (const path of picked) {
    if (!draft.attachments.includes(path)) draft.attachments.push(path);
  }
  renderAttachments();
  await save("attachment added");
}

/** Whether there is anything to keep: what saving skips. */
function isBlank(): boolean {
  return !draft.to.length && !draft.cc.length && !draft.subject.trim() && !draft.body.trim();
}

/** Keep the draft in Drafts and close the window, unless the save failed. */
async function saveAndClose(): Promise<void> {
  if (finished) return;
  await save("closing");
  if (!isBlank() && snapshot() !== lastSaved) return;
  await getCurrentWindow().close();
}

/** Give up on the draft: delete the saved copy, and close the window. */
async function discard(): Promise<void> {
  if (finished) return;
  readFields();
  await collectBody();
  if (draft.savedUid !== null || !isBlank()) {
    const go = await ask("It is deleted from Drafts as well, and cannot be brought back.", {
      title: "Discard this draft?",
      kind: "warning",
      okLabel: "Discard",
      cancelLabel: "Keep writing",
    });
    if (!go) {
      focusBody();
      return;
    }
  }
  finished = true;
  await saveChain;
  if (draft.savedUid !== null) {
    try {
      await invoke("discard_draft", { account: draft.account, uid: draft.savedUid });
    } catch (e) {
      finished = false;
      setStatus(`could not discard: ${describe(e)}`);
      return;
    }
  }
  await releaseOutbox();
  await getCurrentWindow().close();
}

function focusBody(): void {
  if (editor) editor.focus();
  else el.plain.focus();
}

/** Neovim, while it is running. */
let editor: Editor | null = null;

/**
 * Whether the window has finished opening.
 *
 * Until it has, starting the editor is something the window did on its own
 * initiative and must not hand the keyboard to. Afterwards it is something the
 * user asked for, and the editor is where they want to be.
 */
let started = false;

/**
 * Take the body from whichever editor has it.
 *
 * From Neovim that is the buffer as it stands, written or not: a draft saved
 * by the timer or on closing should have what is on screen.
 */
async function collectBody(): Promise<void> {
  draft.body = editor ? await editor.body() : el.plain.value;
}

/**
 * Write in the window rather than in Neovim.
 *
 * Leaving Neovim with `:x` or `:q` lands here, and so does pressing the button.
 * Without it, quitting the editor left the terminal showing the last frame it
 * drew: a black rectangle that no longer answered the keyboard, with the draft
 * apparently gone.
 */
async function usePlain(body: string): Promise<void> {
  await editor?.close();
  editor = null;
  draft.body = body;
  el.plain.value = body;
  plainBefore = body;
  endFilling();
  el.plain.hidden = false;
  el.editor.hidden = true;
  el.toggleEditor.textContent = "Edit in Neovim";
  el.plain.focus();
}

/** Hand the body back to Neovim, carrying whatever was typed here. */
async function useNvim(): Promise<void> {
  const body = editor ? await editor.body() : el.plain.value;
  endFilling();
  el.plain.hidden = true;
  el.editor.hidden = false;
  el.toggleEditor.textContent = "Plain text";
  try {
    editor = await startEditor(
      el.editor,
      crypto.randomUUID(),
      body,
      currentIdentity()?.signature ?? null,
      (text) => void usePlain(text),
      // Only when the editor is what was asked for. At startup the window
      // decides, once the layout has settled.
      started,
      () => void save(":w"),
    );
  } catch (e) {
    setStatus(`could not start Neovim: ${typeof e === "string" ? e : JSON.stringify(e)}`);
    await usePlain(body);
  }
}

/** The draft as the server last had it, so a save with nothing new is skipped. */
let lastSaved = "";

/** The draft, as text, for telling one state from another. */
function snapshot(): string {
  return JSON.stringify([
    draft.to,
    draft.cc,
    draft.bcc,
    draft.subject,
    draft.body,
    draft.attachments,
    draft.identity,
  ]);
}

/**
 * Seconds between looks at the draft for the autosave. Short enough that a
 * crash costs a sentence, long enough not to flood Drafts with appends: a
 * save is an append and a delete on the server.
 */
const AUTOSAVE_SECONDS = 30;

/**
 * Saves, one after another. Each runs once the one before it is done, so a
 * close or a send can wait for whatever is in flight rather than overtake
 * it.
 */
let saveChain: Promise<void> = Promise.resolve();

/**
 * Set while sending, once sent and once discarded: there is no draft left
 * to save.
 */
let finished = false;

/**
 * Write the draft to the server's Drafts folder.
 *
 * Saving is what makes a window closable without thinking about it: the work is
 * on the server, reachable from here or from anything else that reads the
 * account.
 */
function save(reason: string): Promise<void> {
  saveChain = saveChain
    .then(() => saveNow(reason))
    .catch((e) => setStatus(`could not save: ${describe(e)}`));
  return saveChain;
}

async function saveNow(reason: string): Promise<void> {
  if (finished) return;
  readFields();
  await collectBody();
  // Nothing worth keeping. An empty draft saved on every keystroke would fill
  // the folder with blanks.
  if (isBlank()) return;
  const state = snapshot();
  if (state === lastSaved) return;

  setStatus(reason === "autosave" ? "Saving…" : `Saving (${reason})…`, false);
  draft.savedUid = await invoke<number | null>("save_draft", {
    input: {
      account_name: draft.account,
      to: draft.to,
      cc: draft.cc,
      bcc: draft.bcc,
      subject: draft.subject,
      body: draft.body,
      in_reply_to: draft.inReplyTo ?? null,
      references: draft.references ?? null,
      forwards: draft.forwards ?? null,
      original_message_id: null,
      attachment_paths: draft.attachments,
      identity: draft.identity || null,
    },
    replacing: draft.savedUid,
  });
  await releaseOutbox();
  lastSaved = state;
  setStatus(savedAt(), false);
  void refreshWarnings();
}

async function send(): Promise<void> {
  if (finished) return;
  readFields();
  await collectBody();
  const problems = draftProblems(draft);
  if (problems.length) {
    setStatus(`not sent: ${problems.join(", ")}`);
    return;
  }
  await fillChain;
  const open = await refreshWarnings();
  if (open.some((w) => w.kind === "placeholder")) {
    const { answer, name } = await askAboutPlaceholders(open);
    if (answer === "fill" && name) {
      await goToPlaceholder(name);
      return;
    }
    if (answer !== "send") return;
  } else if (open.length) {
    const go = await ask(open.map((w) => `· ${w.message}`).join("\n"), {
      title: "Send anyway?",
      kind: "warning",
      okLabel: "Send",
      cancelLabel: "Back to the draft",
    });
    if (!go) {
      focusBody();
      return;
    }
  }

  // No save from here on, and none still in flight: a save landing after the
  // draft is discarded would leave a copy of the sent message in Drafts.
  finished = true;
  await saveChain;
  el.send.disabled = true;
  setStatus("Sending…", false);
  try {
    const result = await invoke<{
      queued: boolean;
      sent_folder: string;
      warning: string | null;
      follow_up: unknown | null;
    }>("send_mail", {
      input: {
        account_name: draft.account,
        to: draft.to,
        cc: draft.cc,
        bcc: draft.bcc,
        subject: draft.subject,
        body: draft.body,
        in_reply_to: draft.inReplyTo ?? null,
        references: draft.references ?? null,
        forwards: draft.forwards ?? null,
        original_message_id: null,
        attachment_paths: draft.attachments,
        identity: draft.identity || null,
        follow_up: followUp,
      },
    });

    await releaseOutbox();
    // The draft has become a sent message; leaving the saved copy behind would
    // show it as still being written.
    if (draft.savedUid !== null) {
      await invoke("discard_draft", { account: draft.account, uid: draft.savedUid }).catch(
        () => {},
      );
    }
    // A queued message is on its way, just not yet: the window closes as it
    // would for a sent one, and the warning says where the message is.
    setStatus(
      result.warning ??
        (result.queued ? "Queued · will be sent when the server can be reached" : `Sent · copied to ${result.sent_folder}`),
      result.warning !== null,
    );
    // The message is gone, but the task that was to follow it up is not
    // there: the window stays, so the warning that says so can be read.
    if (followUp && !result.follow_up) return;
    await getCurrentWindow().close();
  } catch (e) {
    finished = false;
    el.send.disabled = false;
    setStatus(`not sent: ${typeof e === "string" ? e : JSON.stringify(e)}`);
  }
}

el.subject.addEventListener("input", () => {
  draft.subject = el.subject.value;
  retitle();
});

el.send.addEventListener("click", () => void send());
el.attach.addEventListener("click", () => {
  void pickAttachments().catch((e) => setStatus(`could not attach: ${describe(e)}`));
});
el.attachments.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLElement>("[data-remove]");
  if (!button) return;
  draft.attachments.splice(Number(button.dataset.remove), 1);
  renderAttachments();
  void save("attachment removed");
});

/** Files dropped on the window go out with the message. */
function addAttachments(paths: string[]): void {
  let added = false;
  for (const path of paths) {
    if (draft.attachments.includes(path)) continue;
    draft.attachments.push(path);
    added = true;
  }
  if (!added) return;
  renderAttachments();
  void save("attachment added");
}

// Files dragged in come from the window, not the DOM: Tauri intercepts the
// drag and reports the paths, which the browser's drop event never has.
void getCurrentWebview().onDragDropEvent((event) => {
  if (event.payload.type === "enter" || event.payload.type === "over") {
    document.body.classList.add("dropping");
  } else {
    document.body.classList.remove("dropping");
  }
  if (event.payload.type === "drop") addAttachments(event.payload.paths);
});

// Every half minute, whatever has changed goes to the server, so a window
// forgotten overnight or a crash costs at most that much.
window.setInterval(() => {
  if (!started) return;
  void save("autosave");
}, AUTOSAVE_SECONDS * 1000);
el.toggleEditor.addEventListener("click", () => {
  if (editor) {
    // Take the body out of Neovim before closing it, or what was written is
    // lost on the way to the other editor.
    void editor.body().then((body) => usePlain(body));
  } else {
    void useNvim();
  }
});

el.identity.addEventListener("click", () => void pickSender());
el.template.addEventListener("click", () => void pickTemplate());
// The placeholders' spans move with whatever is typed around them.
el.plain.addEventListener("input", () => {
  spans = followEdit(spans, plainBefore, el.plain.value);
  plainBefore = el.plain.value;
});
renderTemplateButton();
el.showBcc.addEventListener("click", () => showBcc(true));
el.attach.innerHTML = `${svgIcon("clip")}<span>Attach</span>`;
el.discard.innerHTML = `${svgIcon("trash")}<span>Discard</span>`;
el.discard.addEventListener("click", () => void discard());
el.saveClose.addEventListener("click", () => void saveAndClose());
el.followUp.addEventListener("click", () => void pickFollowUp());
renderFollowUp();

bindTabShortcuts();

// In the capture phase, so the window's keys reach it before Neovim's terminal
// can take them.
window.addEventListener(
  "keydown",
  (e) => {
    if (isPasswordPromptOpen() || isDatePickerOpen() || isPickerOpen() || isTemplatePickerOpen() || sheetOpen) return;
    // A key without a modifier is a character in a field or in Neovim's
    // insert mode; only a chord is taken from whatever has focus.
    if (!e.metaKey && !e.ctrlKey && !e.altKey) return;
    const key = eventKey(e);
    const run = COMPOSE_COMMANDS[bindings[key] ?? ""];
    if (run) {
      e.preventDefault();
      e.stopPropagation();
      void run();
      return;
    }
    // A Ctrl+H/J/K/L the window has no use for leaves it — except from
    // Neovim, whose own mappings move between its splits first.
    const direction = navigationDirection(key);
    if (!direction || el.editor.contains(document.activeElement)) return;
    e.preventDefault();
    e.stopPropagation();
    escalate(direction);
  },
  true,
);

// Closing keeps the work rather than asking about it. A draft on the server can
// be reopened; a dialogue on every close cannot be answered by a script, by a
// crash, or by someone closing ten windows at once.
void getCurrentWindow().onCloseRequested(async () => {
  // Whatever happens to the save, the window closes: a handler that throws
  // keeps it open for good. A sent message has nothing left to save.
  await save("closing").catch(() => {});
  await editor?.close().catch(() => {});
});

/** The draft as the mailbox window hands it over; every field optional. */
interface ComposeHandover {
  account?: string;
  identity?: string;
  to?: string[];
  cc?: string[];
  bcc?: string[];
  subject?: string;
  body?: string;
  inReplyTo?: string;
  references?: string;
  forwards?: string;
  savedUid?: number | null;
  outboxId?: number | null;
  attachments?: string[];
  addressedTo?: string[];
  deliveredTo?: string[];
}

/** The draft this window was opened with, handed over by the mailbox window. */
async function start(): Promise<void> {
  // Before the editor measures its cells in the window's text size.
  await followAppearance();
  followTitleBar();
  // A send may need the SMTP password, and this is the window it is asked in.
  installPasswordPrompt();
  // The mailbox window left the draft with the shell for this window to
  // collect: a window exists before it can be spoken to, and a quoted body
  // is too long for a URL.
  const handed = (await invoke<ComposeHandover | null>("take_compose_draft")) ?? {};
  draft.account = handed.account ?? "";
  draft.identity = handed.identity ?? "";
  draft.to = handed.to ?? [];
  draft.cc = handed.cc ?? [];
  draft.bcc = handed.bcc ?? [];
  draft.subject = handed.subject ?? "";
  draft.body = handed.body ?? "";
  draft.inReplyTo = handed.inReplyTo ?? undefined;
  draft.references = handed.references ?? undefined;
  draft.forwards = handed.forwards ?? undefined;
  // A draft being continued already has a copy on the server; saving replaces
  // it rather than leaving the half-written version beside the new one.
  draft.savedUid = handed.savedUid ?? null;
  draft.outboxId = handed.outboxId ?? null;
  draft.attachments = handed.attachments ?? [];
  draft.addressedTo = handed.addressedTo ?? [];
  draft.deliveredTo = handed.deliveredTo ?? [];
  // A reopened draft is what the server has until something changes.
  renderAttachments();
  // What Neovim starts on; the plain field is the fallback without it.
  el.plain.value = draft.body;
  plainBefore = draft.body;

  fields.to.value = draft.to;
  fields.cc.value = draft.cc;
  fields.bcc.value = draft.bcc;
  if (draft.bcc.length) showBcc(false);
  el.subject.value = draft.subject;
  retitle();
  renderIdentity();
  // Before Neovim, which shows the identity's signature apart from the text.
  await loadIdentities();

  await useNvim();
  // A reopened draft is what the server has until something changes. Taken
  // from the editor, which is what a save will read, so a body Neovim gives
  // back with a different line ending does not count as a change.
  if (draft.savedUid !== null) {
    readFields();
    await collectBody().catch(() => {});
    lastSaved = snapshot();
  }
  readFields();
  await collectBody().catch(() => {});
  void refreshWarnings();

  // A new message needs an address before anything else; a reply already has
  // one, so the editor is where the work is.
  //
  // On the next frame rather than now: the terminal is still laying itself out
  // when this runs, and whatever it does to the keyboard on the way up should
  // happen before the window has its say, not after.
  requestAnimationFrame(() => {
    if (draft.to.length) editor?.focus();
    else fields.to.focus();
    started = true;
  });
}

/** Every account's identities and the keys that choose between them. */
async function loadIdentities(): Promise<void> {
  try {
    const [list, config] = await Promise.all([
      invoke<Identity[]>("senders"),
      invoke<{ compose_normal: Record<string, string> | null }>("bindings"),
    ]);
    identities = list;
    bindings = { ...bindings, ...canonicalTable(config.compose_normal) };
    renderIdentity();
  } catch (e) {
    setStatus(`could not read the identities: ${describe(e)}`);
  }
}

void start();
