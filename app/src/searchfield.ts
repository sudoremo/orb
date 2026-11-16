/**
 * What the search field offers while a query is typed (§4.5): the terms
 * already given as chips, where to search, what the word being typed could
 * mean, and the operators there are.
 *
 * The query language does not change; this only writes it for someone who
 * has not learnt it. A chip is a term of the language, the suggestions put
 * one in, and the line at the bottom names them all.
 */
import { call } from "./bridge";
import { currentFolder } from "./actions";
import { escape } from "./format";
import { IconName, svgIcon } from "./icons";
import { inTaskView } from "./sidebar";
import { removeSetting, readSetting, writeSetting } from "./splitters";
import { SearchKind, el, state, todayHasMail } from "./state";
import { TaskTagInfo } from "./tasklist";

/** Where a search looks: everywhere, one account's mail, or chosen folders. */
export type Scope =
  | { kind: "all" }
  | { kind: "account"; account: string }
  | { kind: "folders"; folders: [string, string][] };

/** The scope as the core's `SearchScope` is spelled on the wire. */
export function scopeArg(scope: Scope): unknown {
  if (scope.kind === "account") return { Account: { account_name: scope.account } };
  if (scope.kind === "folders") return { Folders: { folders: scope.folders } };
  return "All";
}

/** What the toolbar calls a scope. */
export function scopeLabel(scope: Scope): string {
  if (scope.kind === "account") return scope.account;
  if (scope.kind === "folders") {
    if (scope.folders.length === 1) return folderName(scope.folders[0]!);
    return `${scope.folders.length} folders`;
  }
  return "Everywhere";
}

function folderName([account, folder]: [string, string]): string {
  const info = state.folders.find((f) => f.account_name === account && f.folder_name === folder && !f.is_pinned);
  return info ? (info.label ?? info.display_name) : folder;
}

// ---------------------------------------------------------------------------
// The field's state, for as long as it is open
// ---------------------------------------------------------------------------

const SEARCH_KIND = ".searchKind";
const KINDS: { kind: SearchKind; label: string }[] = [
  { kind: "mail", label: "Mail" },
  { kind: "tasks", label: "Tasks" },
  { kind: "all", label: "All" },
];
/**
 * What a search is for: mail, tasks, or both. Remembered on this machine, the
 * results' own switch included, since most searches are for one kind; in the
 * tasks window, tasks.
 */
export function fieldKind(): SearchKind {
  if (!todayHasMail()) return "tasks";
  const kind = readSetting(SEARCH_KIND);
  return kind === "mail" || kind === "tasks" ? kind : "all";
}
export function setFieldKind(kind: SearchKind): void {
  if (kind === "all") removeSetting(SEARCH_KIND);
  else writeSetting(SEARCH_KIND, kind);
  if (kind === "tasks") picking = false;
}
function cycleKind(): void {
  const at = KINDS.findIndex((k) => k.kind === fieldKind());
  setFieldKind(KINDS[(at + 1) % KINDS.length]!.kind);
  renderField();
  void suggest();
}

/** Terms of the language given whole, as the field draws them. */
let chips: string[] = [];
let scope: Scope = { kind: "all" };
/** The folder the field was opened over, for "This folder". */
let here: [string, string] | null = null;
let picking = false;
let pickCursor = 0;
let items: Item[] = [];
let itemCursor = 0;
let tags: TaskTagInfo[] = [];
let asked = "";

interface Item {
  icon: IconName;
  html: string;
  hint: string;
  /** What choosing it does; none for the plain search. */
  apply?: () => void;
}

/** A term of the language that becomes a chip once it is typed whole. */
const OPERATOR =
  /^-?(f|from|t|to|cc|s|subject|b|body|in|folder|acct|account|is|has|after|since|before|until|tag|header):("[^"]+"|'[^']+'|[^\s"']+)$/i;

interface Token {
  text: string;
  start: number;
  end: number;
}

/**
 * Split text into the terms of the language: at whitespace, except inside a
 * quoted run, so that `s:"devel passwort"` is one term as the core reads it.
 * A quote left open runs to the end.
 */
function tokens(text: string): Token[] {
  const found: Token[] = [];
  let start = -1;
  let quote = "";
  for (let i = 0; i <= text.length; i++) {
    const c = text[i];
    if (c === undefined || (!quote && /\s/.test(c))) {
      if (start >= 0) found.push({ text: text.slice(start, i), start, end: i });
      start = -1;
      continue;
    }
    if (start < 0) start = i;
    if (quote ? c === quote : c === '"' || c === "'") quote = quote ? "" : c;
  }
  return found;
}

/** A value without the quotes around it. */
const unquoted = (value: string) => value.replace(/^["']|["']$/g, "");
const KEYS: Record<string, string> = { from: "f", to: "t", subject: "s", body: "b", folder: "in", account: "acct", since: "after", until: "before" };

/** The query the field holds: the chips and the text after them. */
export function fieldQuery(): string {
  return [...chips, el.searchInput.value.trim()].filter(Boolean).join(" ");
}

export function fieldScope(): Scope {
  return scope;
}

/** The query the field opened with: the search on screen, or nothing. */
let openedWith = "";
/** Whether the field still holds the search on screen, as it was opened. */
export function fieldUnchanged(): boolean {
  return fieldQuery() === openedWith;
}

/**
 * Set the field up as it opens: empty, or holding the search on screen so
 * that it can be changed rather than typed again.
 */
export function openField(): void {
  const folder = !inTaskView() && !state.search ? currentFolder() : undefined;
  here = folder?.folder_name ? [folder.account_name, folder.folder_name] : null;
  const search = state.search;
  if (search && search.view === undefined && search.conversation === undefined) {
    const words = tokens(search.query).map((t) => t.text);
    chips = words.filter((w) => OPERATOR.test(w));
    el.searchInput.value = words.filter((w) => !OPERATOR.test(w)).join(" ");
    scope = search.scope;
  } else {
    chips = [];
    scope = { kind: "all" };
  }
  picking = false;
  items = [];
  openedWith = fieldQuery();
  if (state.tasks.available) {
    void call<TaskTagInfo[]>("list_task_tags").then((found) => (tags = found ?? []), () => {});
  }
  renderField();
}

/** Turn every operator typed whole — followed by a space — into a chip. */
function takeChips(): void {
  const value = el.searchInput.value;
  const kept: string[] = [];
  let taken = false;
  for (const token of tokens(value)) {
    // Whole once something follows it: a space, and its quotes closed.
    if (token.end < value.length && OPERATOR.test(token.text)) {
      chips.push(token.text);
      taken = true;
    } else kept.push(token.text);
  }
  if (!taken) return;
  el.searchInput.value = kept.join(" ") + (/\s$/.test(value) && kept.length ? " " : "");
}

/** The word the caret is in, for the suggestions to answer. */
function currentWord(): string {
  const before = el.searchInput.value.slice(0, el.searchInput.selectionStart ?? undefined);
  if (/\s$/.test(before) && !tokens(before).some((t) => t.end === before.length)) return "";
  return tokens(before).pop()?.text ?? "";
}

/** Put `term` where the word being typed was, as a chip; none, and it goes. */
function replaceWord(term?: string): void {
  const input = el.searchInput;
  const caret = input.selectionStart ?? input.value.length;
  const word = tokens(input.value.slice(0, caret)).pop();
  const before = word && word.end === caret ? input.value.slice(0, word.start) : input.value.slice(0, caret);
  input.value = (before + input.value.slice(caret)).replace(/\s+/g, " ").trimStart();
  if (term) chips.push(term);
}

const quote = (value: string) => (/\s/.test(value) ? `"${value}"` : value);

/** Work out what the word being typed could mean. */
async function suggest(): Promise<void> {
  const word = currentWord();
  asked = word;
  items = [];
  itemCursor = 0;
  if (!word) return renderField();

  const prefixed = /^(-?)(\w+):(.*)$/.exec(word);
  if (prefixed) {
    const [, not, raw, quoted] = prefixed;
    const value = unquoted(quoted!);
    const key = KEYS[raw!.toLowerCase()] ?? raw!.toLowerCase();
    // What was typed comes first, so Enter searches for it as written: the
    // people and tags under it are offers, not the only answers.
    const asTyped = (what: string) =>
      value && items.push({ ...chipItem("search", `${not ? "Not " : ""}${what} “${escape(value)}”`, `${not}${key}:${quote(value)}`), hint: "↩" });
    if (["f", "t", "cc"].includes(key)) {
      if (fieldKind() !== "tasks") {
        asTyped(`${({ f: "From", t: "To", cc: "Cc" } as Record<string, string>)[key]} contains`);
        await people(value!, [key], not!);
      }
    } else if (key === "tag") {
      asTyped("Tasks with a tag beginning");
      tagItems(value!, not!);
    }
    else if (key === "in") folderItems(value!);
    else if (key === "acct") accountItems(value!);
    else if (key === "is") {
      for (const flag of ["unread", "read", "flagged", "answered"].filter((f) => f.startsWith(value!.toLowerCase()))) {
        items.push(chipItem("search", `${not ? "Not " : ""}${flag}`, `${not}is:${flag}`));
      }
    } else if (key === "has") items.push(chipItem("clip", "With a file attached", `${not}has:attachment`));
    if (asked === word) renderField();
    return;
  }

  const plain = unquoted(word);
  if (!plain) return renderField();
  const shown = escape(plain);
  const kind = fieldKind();
  const where =
    kind === "tasks" ? "in every task" : kind === "mail" || scope.kind !== "all" ? "in mail" : "in mail in every folder, and in tasks";
  items.push({ icon: "search", html: `“${shown}” <span class="d">— anywhere ${where}</span>`, hint: "↩" });
  items.push(chipItem("text", `${kind === "tasks" ? "Title" : "Subject"} contains “${shown}”`, `s:${quote(plain)}`));
  items.push(chipItem("text", `${kind === "tasks" ? "Notes contain" : "Body contains"} “${shown}”`, `b:${quote(plain)}`));
  if (kind !== "mail") tagItems(plain, "", 2);
  renderField();
  if (plain.length >= 2 && kind !== "tasks") await people(plain, ["f", "t"], "");
  if (asked === word) renderField();
}

/** A suggestion that puts `term` in as a chip. */
function chipItem(icon: IconName, html: string, term: string): Item {
  return { icon, html, hint: `<code>${escape(term)}</code>`, apply: () => replaceWord(term) };
}

/** People from the address book, as sender or as recipient. */
async function people(value: string, keys: string[], not: string): Promise<void> {
  if (!todayHasMail() || !value) return;
  const found = await call<{ name: string | null; recipients: { name: string | null; address: string }[] }[]>(
    "suggest_recipients",
    { query: value, limit: 3 },
  ).catch(() => []);
  if (asked !== currentWord()) return;
  const role: Record<string, string> = { f: "From", t: "To", cc: "Cc" };
  for (const key of keys) {
    for (const [i, person] of found.entries()) {
      // The sender is what a search is usually after; the first person
      // alone is offered as a recipient as well.
      if (key !== keys[0] && i > 0) break;
      const r = person.recipients[0];
      if (!r) continue;
      const name = r.name ?? person.name;
      items.push(
        chipItem(
          "user",
          `${not ? "Not " : ""}${role[key]} ${name ? `<b>${escape(name)}</b> <span class="d">${escape(r.address)}</span>` : `<b>${escape(r.address)}</b>`}`,
          `${not}${key}:${r.address}`,
        ),
      );
    }
  }
}

function tagItems(value: string, not: string, limit = 6): void {
  const wanted = value.toLowerCase();
  for (const tag of tags.filter((t) => t.name.toLowerCase().startsWith(wanted)).slice(0, limit)) {
    items.push(chipItem("tag", `${not ? "Tasks not tagged" : "Tasks tagged"} <b>#${escape(tag.name)}</b>`, `${not}tag:${quote(tag.name)}`));
  }
}

/** Folders by name, each narrowing where the search looks. */
function folderItems(value: string): void {
  const wanted = value.toLowerCase();
  for (const f of searchableFolders().filter((f) => (f.label ?? f.display_name).toLowerCase().includes(wanted)).slice(0, 8)) {
    const pair: [string, string] = [f.account_name, f.folder_name!];
    items.push({
      icon: "folder",
      html: `In <b>${escape(f.label ?? f.display_name)}</b> <span class="d">${escape(f.account_name)}</span>`,
      hint: "",
      apply: () => {
        replaceWord();
        addFolder(pair);
      },
    });
  }
}

function accountItems(value: string): void {
  for (const a of state.accounts.filter((a) => a.name.toLowerCase().startsWith(value.toLowerCase()))) {
    items.push({
      icon: "inbox",
      html: `The mail of <b>${escape(a.name)}</b>`,
      hint: "",
      apply: () => {
        replaceWord();
        scope = { kind: "account", account: a.name };
      },
    });
  }
}

function searchableFolders() {
  return state.folders.filter((f) => !f.is_account_header && !f.is_pinned && f.folder_name);
}

function addFolder(pair: [string, string]): void {
  const folders = scope.kind === "folders" ? scope.folders : [];
  if (!folders.some(([a, f]) => a === pair[0] && f === pair[1])) folders.push(pair);
  scope = { kind: "folders", folders };
}

// ---------------------------------------------------------------------------
// Where to search
// ---------------------------------------------------------------------------

interface Pill {
  label: string;
  scope: Scope | "pick";
  on: boolean;
}

function pills(): Pill[] {
  if (!todayHasMail()) return [];
  const same = (a: [string, string][], b: [string, string][]) =>
    a.length === b.length && a.every(([x, y], i) => b[i]![0] === x && b[i]![1] === y);
  const list: Pill[] = [{ label: "Everywhere", scope: { kind: "all" }, on: scope.kind === "all" }];
  const hereOn = !!here && scope.kind === "folders" && same(scope.folders, [here]);
  if (here) list.push({ label: `This folder · ${escape(folderName(here))}`, scope: { kind: "folders", folders: [here] }, on: hereOn });
  for (const a of state.accounts) {
    list.push({ label: escape(a.name), scope: { kind: "account", account: a.name }, on: scope.kind === "account" && scope.account === a.name });
  }
  const picked = scope.kind === "folders" && !hereOn ? scope.folders.length : 0;
  list.push({ label: `Folders…${picked ? ` <span class="n">${picked}</span>` : ""}`, scope: "pick", on: picked > 0 || picking });
  return list;
}

/** `⌃F`: the next place to search, the picker last. */
function cycleScope(): void {
  const list = pills();
  if (list.length === 0) return;
  const at = picking ? list.length - 1 : Math.max(0, list.findIndex((p) => p.on && p.scope !== "pick"));
  const next = list[(at + 1) % list.length]!;
  if (next.scope === "pick") {
    picking = true;
    pickCursor = 0;
    renderField();
  } else choosePill(next);
}

function choosePill(pill: Pill): void {
  if (pill.scope === "pick") {
    picking = !picking;
    pickCursor = 0;
  } else {
    scope = pill.scope.kind === "folders" ? { kind: "folders", folders: [...pill.scope.folders] } : pill.scope;
    picking = false;
  }
  renderField();
}

// ---------------------------------------------------------------------------
// Drawing and keys
// ---------------------------------------------------------------------------

const chipsEl = document.getElementById("search-chips")!;
const scopeEl = document.getElementById("search-scope")!;
const kindEl = document.getElementById("search-kind")!;
const suggestEl = document.getElementById("search-suggest")!;
const opsEl = document.getElementById("search-ops")!;

function chipMarkup(term: string, index: number): string {
  const match = /^(-?)(\w+):(.*)$/.exec(term)!;
  const [, not, key, value] = match;
  return `<span class="tokn" data-chip="${index}"><span class="k">${not ? "not " : ""}${escape(key!)}</span>${escape(value!.replace(/^"|"$/g, ""))}</span>`;
}

export function renderField(): void {
  const scopeChips = scope.kind === "folders" && !(here && scope.folders.length === 1 && scope.folders[0]![0] === here[0] && scope.folders[0]![1] === here[1])
    ? scope.folders.map((f, i) => `<span class="tokn scope" data-scope-chip="${i}"><span class="k">in</span>${escape(folderName(f))}</span>`).join("")
    : "";
  chipsEl.innerHTML = scopeChips + chips.map(chipMarkup).join("");

  const kind = fieldKind();
  kindEl.hidden = !todayHasMail();
  kindEl.innerHTML =
    KINDS.map((k) => `<button type="button" role="radio" aria-checked="${k.kind === kind}" data-kind="${k.kind}" tabindex="-1">${k.label}</button>`).join("") +
    `<kbd title="What to search">⌃T</kbd>`;
  // Tasks are in no folder.
  scopeEl.hidden = !todayHasMail() || kind === "tasks";
  scopeEl.innerHTML = `<span class="lb">In</span>${pills()
    .map((p, i) => `<button type="button" class="pill${p.on ? " on" : ""}" data-pill="${i}" tabindex="-1">${p.scope === "pick" ? svgIcon("folder") : ""}${p.label}</button>`)
    .join("")}<kbd title="Where to search">⌃F</kbd>`;

  if (picking) {
    const folders = searchableFolders();
    let account = "";
    suggestEl.innerHTML = folders
      .map((f, i) => {
        const head = f.account_name !== account ? `<div class="sp-h">${escape((account = f.account_name))}</div>` : "";
        const on = scope.kind === "folders" && scope.folders.some(([a, n]) => a === f.account_name && n === f.folder_name);
        return `${head}<div class="sp-i${i === pickCursor ? " on" : ""}" data-pick="${i}"><span class="chk${on ? " on" : ""}"></span><span class="fl2" style="margin-left:${(f.depth ?? 0) * 18}px">${svgIcon("folder")}${escape(f.label ?? f.display_name)}</span><span class="d">${f.total_count || ""}</span></div>`;
      })
      .join("");
    suggestEl.querySelector(".sp-i.on")?.scrollIntoView({ block: "nearest" });
    opsEl.innerHTML = `<span><kbd>space</kbd> tick a folder</span><span><kbd>↩</kbd> search the folders ticked</span><span>A search narrowed to folders finds mail only.</span>`;
    return;
  }

  suggestEl.innerHTML = items.length
    ? `<div class="sp-h">Search for</div>` +
      items
        .map((it, i) => `<div class="sp-i${i === itemCursor ? " on" : ""}" data-item="${i}">${svgIcon(it.icon)}<span>${it.html}</span><span class="hint">${it.hint === "↩" ? "<kbd>↩</kbd>" : it.hint}</span></div>`)
        .join("")
    : "";
  const op = (code: string, what = "") => `<span><code>${code}</code>${what ? ` ${what}` : ""}</span>`;
  opsEl.innerHTML = kind !== "tasks"
    ? [op("f:", "from"), op("t:", "to"), op("s:", "subject"), op("b:", "body"), op("in:", "folder"), op("is:unread"),
       op("has:attachment"), op("after:"), op("before:"), ...(kind === "all" ? [op("tag:", "task tag")] : []), op("~", "by meaning"), op("OR"), op("-", "not")].join("")
    : [op("s:", "title"), op("b:", "notes"), op("tag:", "tag"), op("OR"), op("-", "not")].join("");
}

let timer: ReturnType<typeof setTimeout> | undefined;
/** After every change of the text: chips taken, suggestions asked again. */
export function onFieldInput(): void {
  takeChips();
  renderField();
  clearTimeout(timer);
  timer = setTimeout(() => void suggest(), 90);
}

/**
 * The field's own keys. Returns true when the key was one, so the caller
 * leaves it alone; Enter and Escape are the caller's.
 */
export function onFieldKey(e: KeyboardEvent): boolean {
  if (e.ctrlKey && e.key.toLowerCase() === "t" && todayHasMail()) {
    cycleKind();
    return true;
  }
  if (e.ctrlKey && e.key.toLowerCase() === "f") {
    if (fieldKind() !== "tasks") cycleScope();
    return true;
  }
  if (picking) {
    const folders = searchableFolders();
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      pickCursor = Math.min(Math.max(pickCursor + (e.key === "ArrowDown" ? 1 : -1), 0), folders.length - 1);
      renderField();
      return true;
    }
    if (e.key === " " && folders[pickCursor]) {
      toggleFolder(pickCursor);
      return true;
    }
  }
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    if (items.length === 0) return false;
    itemCursor = (itemCursor + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
    renderField();
    return true;
  }
  if (e.key === "Tab" && !e.shiftKey) {
    const item = items[itemCursor];
    if (!item?.apply) return true;
    item.apply();
    items = [];
    renderField();
    return true;
  }
  if (e.key === "Backspace") {
    const input = el.searchInput;
    if (input.selectionStart !== 0 || input.selectionEnd !== 0) return false;
    // Back past the start of the text: the last chip becomes text again.
    const last = chips.pop();
    if (last !== undefined) {
      input.value = `${last} ${input.value}`.trimEnd();
      input.setSelectionRange(last.length, last.length);
    } else if (scope.kind !== "all") scope = { kind: "all" };
    else return false;
    renderField();
    return true;
  }
  return false;
}

/** Enter on a suggestion that writes a chip writes it before searching. */
export function applyChosen(): void {
  if (picking) return;
  items[itemCursor]?.apply?.();
  items = [];
}

function toggleFolder(index: number): void {
  const f = searchableFolders()[index];
  if (!f?.folder_name) return;
  const folders = scope.kind === "folders" ? scope.folders : [];
  const at = folders.findIndex(([a, n]) => a === f.account_name && n === f.folder_name);
  if (at >= 0) folders.splice(at, 1);
  else folders.push([f.account_name, f.folder_name]);
  scope = folders.length ? { kind: "folders", folders } : { kind: "all" };
  renderField();
}

// The mouse: a pill, a suggestion, a folder, a chip. The field keeps the keyboard.
el.searchOverlay.addEventListener("mousedown", (e) => {
  const target = e.target as HTMLElement;
  const pill = target.closest<HTMLElement>("[data-pill]");
  const item = target.closest<HTMLElement>("[data-item]");
  const pick = target.closest<HTMLElement>("[data-pick]");
  const chip = target.closest<HTMLElement>("[data-chip]");
  const scoped = target.closest<HTMLElement>("[data-scope-chip]");
  const kind = target.closest<HTMLElement>("[data-kind]");
  if (!pill && !item && !pick && !chip && !scoped && !kind) return;
  e.preventDefault();
  if (kind) {
    setFieldKind(kind.dataset.kind as SearchKind);
    renderField();
    void suggest();
  } else if (pill) choosePill(pills()[Number(pill.dataset.pill)]!);
  else if (item) {
    itemCursor = Number(item.dataset.item);
    const chosen = items[itemCursor];
    if (chosen?.apply) {
      chosen.apply();
      items = [];
      renderField();
    } else el.searchInput.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
  } else if (pick) toggleFolder(Number(pick.dataset.pick));
  else if (chip) {
    // A chip clicked goes back to being text, to be changed.
    const [term] = chips.splice(Number(chip.dataset.chip), 1);
    el.searchInput.value = `${el.searchInput.value} ${term}`.trim();
    renderField();
  } else if (scoped && scope.kind === "folders") {
    scope.folders.splice(Number(scoped.dataset.scopeChip), 1);
    if (scope.folders.length === 0) scope = { kind: "all" };
    renderField();
  }
});
