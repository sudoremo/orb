/**
 * The reader of a conversation: what a collapsed thread's row shows.
 *
 * The row stands for every message of its thread, and so does the reader —
 * the messages oldest first, the ones not yet read and the newest open, the
 * others a line each, with a cursor of their own that the reader's keys move
 * while the reader has the keyboard. The stack is drawn once per set of
 * members and patched in place after that: marking the messages read, a
 * body arriving or a message opened must not reload the frames of the others
 * or lose the place the reader was scrolled to.
 */
import { call, setStatus } from "./bridge";
import { currentFolder } from "./actions";
import { windowIsDark } from "./appearance";
import { escape, icon, listDate, parseFrom, readerDate, recipientNames, subjectIconsHtml } from "./format";
import { cursorRow } from "./list";
import { AttachmentMeta, InlineImage, MessageDetail, MessageHeader, SenderCheck } from "./model";
import { attachmentChips, plainBody, prepareHtml, senderWarningsHtml } from "./reader";
import { highlightMarks, iconExists } from "./sidebar";
import { el, state } from "./state";
import { renderMessageTasks } from "./taskdetail";
import type { TaskSummary } from "./tasklist";

export interface MailConversation {
  /** The id of the row it is shown for: the thread's oldest message. */
  rowId: number;
  thread: string;
  /** What the row said when the members were read (`rowKey`). */
  signature: string;
  /** Oldest first, each where it lives. */
  members: MessageHeader[];
  details: Map<number, MessageDetail>;
  /** Members whose body could not be fetched. */
  failed: Set<number>;
  /** Members shown open, by id. */
  open: Set<number>;
  /** The member the reader's keys act on. */
  focus: number;
  /** The read messages above the first open one are folded into one line. */
  folded: boolean;
  attachments: Map<number, AttachmentMeta[] | null>;
  checks: Map<number, SenderCheck | null>;
  inline: Map<number, Map<string, string> | null>;
  /** The tasks any member is linked to (§6.3). */
  linked: TaskSummary[];
}

/** How many read messages above the first open one are folded into a line. */
const FOLD_AT = 3;
/** How many bodies are fetched at once. */
const FETCHES = 3;

/** A collapsed thread's row, which stands for its conversation. */
export type ConversationRow = MessageHeader & { thread_id: string; thread_members: number[] };
/** Whether a row stands for a conversation rather than for one message. */
export function isConversationRow(m: MessageHeader | undefined): m is ConversationRow {
  return !!m && !!m.thread_id && !m.thread_expanded && (m.thread_members?.length ?? 0) > 1;
}
/**
 * Which members the row stands for: a change redraws the stack. Kept in an
 * attribute, so no NUL in it, which the parser would turn into U+FFFD.
 */
function memberKey(row: MessageHeader): string {
  return `${row.thread_id} ${(row.thread_members ?? []).join(",")}`;
}
/** What the row says about its members: a change reads them again. */
export function rowKey(row: MessageHeader): string {
  return `${memberKey(row)}\u0000${row.thread_unread ?? 0}\u0000${row.is_flagged}\u0000${row.pending_action ?? ""}`;
}
/** The folders the threaded list on screen is read from. */
export function listFolders(): [string, string][] {
  if (state.inboxFolders) {
    return state.inboxFolders.filter((f) => f.folder_name).map((f) => [f.account_name, f.folder_name!]);
  }
  const folder = currentFolder();
  return folder?.folder_name ? [[folder.account_name, folder.folder_name]] : [];
}
/** The members of conversations, read in one call, by thread. */
export async function membersOf(rows: MessageHeader[]): Promise<Map<string, MessageHeader[]>> {
  const threads = [...new Set(rows.map((r) => r.thread_id!).filter(Boolean))];
  const found = new Map<string, MessageHeader[]>();
  const shown = state.conversation;
  // The conversation in the reader is answered from what it holds, so that
  // what is done to its members shows in the reader at once.
  const ask = threads.filter((t) => !(shown && shown.thread === t));
  if (shown && threads.includes(shown.thread)) found.set(shown.thread, shown.members);
  if (ask.length === 0) return found;
  const members = await call<MessageHeader[]>("thread_members", { folders: listFolders(), threads: ask });
  const byId = new Map(rows.map((r) => [r.thread_id!, r]));
  for (const thread of ask) {
    const ids = new Set(byId.get(thread)?.thread_members ?? []);
    found.set(thread, members.filter((m) => ids.has(m.id)));
  }
  return found;
}
/** The conversation the reader holds for this row, if it holds it. */
export function conversationFor(row: MessageHeader | undefined): MailConversation | null {
  const conv = state.conversation;
  return conv && row && conv.rowId === row.id && conv.thread === row.thread_id ? conv : null;
}
/** The conversation the reader is showing, while it shows one. */
export function shownConversation(): MailConversation | null {
  const row = cursorRow();
  return isConversationRow(row) ? conversationFor(row) : null;
}
/** The newest member: the one a reply from the list answers. */
function newest(conv: MailConversation): MessageHeader {
  return conv.members[conv.members.length - 1]!;
}
/**
 * The message of the conversation a command for one message means: the one
 * the reader's keys are on while the reader has the keyboard, else the newest.
 */
export function conversationMessage(): MessageHeader | undefined {
  const conv = shownConversation();
  if (!conv) return undefined;
  if (state.focus === "reader") return conv.members.find((m) => m.id === conv.focus) ?? newest(conv);
  return newest(conv);
}

/**
 * Read the conversation a row stands for. A conversation the reader already
 * holds for the same thread keeps which messages are open, which one the
 * keys are on and the bodies it has; the messages that arrived since open,
 * as unread ones do.
 */
export async function readConversation(row: MessageHeader): Promise<MailConversation> {
  const signature = rowKey(row);
  const members = (await call<MessageHeader[]>("thread_members", {
    folders: listFolders(),
    threads: [row.thread_id],
  })) ?? [];
  // Filed away since the row was read: there is nothing to show.
  if (members.length === 0) throw new Error("the conversation is no longer here");
  const before = state.conversation?.thread === row.thread_id ? state.conversation : null;
  const known = new Set(before?.members.map((m) => m.id) ?? []);
  const last = members[members.length - 1];
  const open = new Set<number>();
  for (const m of members) {
    if (before ? before.open.has(m.id) || !known.has(m.id) : !m.is_read) open.add(m.id);
  }
  if (last && !before) open.add(last.id);
  const firstUnread = members.find((m) => !m.is_read);
  const keep = before && members.some((m) => m.id === before.focus) ? before.focus : null;
  const conv: MailConversation = {
    rowId: row.id,
    thread: row.thread_id!,
    signature,
    members,
    details: new Map(),
    failed: new Set(),
    open,
    focus: keep ?? firstUnread?.id ?? last?.id ?? row.id,
    folded: before?.folded ?? true,
    attachments: new Map(),
    checks: new Map(),
    inline: new Map(),
    linked: before?.linked ?? [],
  };
  if (before) {
    for (const m of members) {
      const d = before.details.get(m.id);
      if (d) conv.details.set(m.id, d);
      if (before.attachments.has(m.id)) conv.attachments.set(m.id, before.attachments.get(m.id)!);
      if (before.checks.has(m.id)) conv.checks.set(m.id, before.checks.get(m.id)!);
      if (before.inline.has(m.id)) conv.inline.set(m.id, before.inline.get(m.id)!);
    }
  }
  return conv;
}
/** Bodies being fetched, by member id, so a repaint does not ask twice. */
const fetching = new Set<number>();
/** Fetch the bodies of the open messages and of the one the keys are on, a few at a time. */
export async function fillConversation(conv: MailConversation): Promise<void> {
  const wanted = conv.members
    .map((m) => m.id)
    .filter((id) => (conv.open.has(id) || id === conv.focus) && !conv.details.has(id) && !conv.failed.has(id) && !fetching.has(id));
  // The one the keys are on first, then from the top down.
  wanted.sort((a, b) => Number(b === conv.focus) - Number(a === conv.focus));
  for (const id of wanted) fetching.add(id);
  const next = async (): Promise<void> => {
    const id = wanted.shift();
    if (id === undefined) return;
    try {
      const detail = await call<MessageDetail>("message", { id });
      conv.details.set(id, detail);
      if (state.conversation === conv && conv.focus === id) state.detail = detail;
    } catch {
      conv.failed.add(id);
    } finally {
      fetching.delete(id);
    }
    paintShown(conv);
    return next();
  };
  await Promise.all(Array.from({ length: Math.min(FETCHES, wanted.length) }, next));
}
/** Read which tasks the members are linked to, for the header (§6.3). */
export async function loadConversationTasks(conv: MailConversation): Promise<void> {
  if (!state.tasks.available) return;
  const lists = await Promise.all(
    conv.members.map((m) => call<TaskSummary[]>("tasks_for_message", { message: m.id }).catch(() => [] as TaskSummary[])),
  );
  const seen = new Set<number>();
  conv.linked = lists.flat().filter((t) => !seen.has(t.id) && seen.add(t.id));
  paintShown(conv);
}

// ---------------------------------------------------------------- Drawing

/** The pane's stack, when it holds this conversation's members. */
function drawnStack(conv: MailConversation): HTMLElement | null {
  const root = el.reader.querySelector<HTMLElement>(":scope > .conv");
  return root && root.dataset.members === memberKey(rowOf(conv)) ? root : null;
}
/** The row a conversation is shown for, as the list holds it now. */
function rowOf(conv: MailConversation): MessageHeader {
  const row = cursorRow();
  if (row && row.id === conv.rowId) return row;
  // Not on screen: the members as they were read.
  return { ...conv.members[0]!, thread_id: conv.thread, thread_members: conv.members.map((m) => m.id) };
}
/** Repaint the conversation, if the reader is showing it. */
export function paintShown(conv: MailConversation | null = state.conversation): void {
  if (!conv || state.conversation !== conv || shownConversation() !== conv) return;
  const root = drawnStack(conv);
  if (root) paint(conv, root);
}
/**
 * Draw the conversation a row stands for into the reader: the whole stack
 * the first time, only what changed after that. Returns whether it is there
 * to read, rather than still being read in.
 */
export function drawConversation(row: MessageHeader): boolean {
  const conv = conversationFor(row);
  if (!conv) {
    el.reader.innerHTML = loadingMarkup(row);
    return false;
  }
  let root = el.reader.querySelector<HTMLElement>(":scope > .conv");
  const members = memberKey(row);
  if (!root || root.dataset.members !== members || root.dataset.row !== String(conv.rowId)) {
    // A stack drawn for the same thread keeps its place as members arrive.
    const kept = root?.dataset.thread === conv.thread ? stackOf(root)?.scrollTop ?? null : null;
    el.reader.innerHTML = stackMarkup(conv, members);
    root = el.reader.querySelector<HTMLElement>(":scope > .conv")!;
    paint(conv, root);
    const stack = stackOf(root)!;
    if (kept !== null) stack.scrollTop = kept;
    else revealCard(conv, conv.focus, true);
    return true;
  }
  paint(conv, root);
  return true;
}
function stackOf(root: HTMLElement): HTMLElement | null {
  return root.querySelector<HTMLElement>(":scope > .conv-stack");
}
/** The reader while the members are on their way: what the row already says. */
function loadingMarkup(row: MessageHeader): string {
  return `<header class="hdrs conv-hdrs">
      <h1 class="h-subj" data-k="Subject">${subjectIconsHtml(row.subject_icons, iconExists)}${escape(row.subject || "(no subject)")}</h1>
      <div class="conv-sub">${row.thread_count ?? 0} messages</div>
    </header><div class="fetching">${icon("fa-rotate", "spin")} Loading conversation…</div>`;
}
function stackMarkup(conv: MailConversation, members: string): string {
  const first = conv.members[0]!;
  const cards = conv.members
    .map(
      (m) => `<article class="card" data-card="${m.id}">
        <div class="card-head" data-card-head>
          <i class="card-dot"></i>
          <span class="card-who"></span>
          <span class="card-pv"></span>
          <span class="card-marks"></span>
          <span class="card-when"></span>
        </div>
        <div class="card-body"><div class="card-meta"></div><div class="card-text"></div></div>
      </article>`,
    )
    .join("");
  return `<div class="conv" data-members="${escape(members)}" data-thread="${escape(conv.thread)}" data-row="${conv.rowId}">
      <header class="hdrs conv-hdrs">
        <nav class="views"><button type="button" data-view="html">HTML</button><button type="button" data-view="text">Text</button></nav>
        <h1 class="h-subj" data-k="Subject">${subjectIconsHtml(first.subject_icons, iconExists)}${escape(first.subject || "(no subject)")}<span class="hl-marks"></span></h1>
        <div class="conv-sub"></div>
        <div class="h-att conv-tasks"></div>
      </header>
      <div class="conv-stack"><button type="button" class="conv-earlier" data-conv-earlier></button>${cards}</div>
    </div>`;
}
/** The read messages above the first open one, when they are folded into a line. */
function foldedIds(conv: MailConversation): Set<number> {
  if (!conv.folded) return new Set();
  const above: number[] = [];
  for (const m of conv.members) {
    if (conv.open.has(m.id) || m.id === conv.focus) break;
    above.push(m.id);
  }
  return new Set(above.length >= FOLD_AT ? above : []);
}
/** Your own addresses, said as "me". */
function ownAddresses(): Set<string> {
  return new Set(state.accounts.map((a) => a.email.toLowerCase()));
}
/** A sender as a short name: the display name, else the address. */
function shortName(from: string | null): string {
  const { name, email } = parseFrom(from);
  if (email && ownAddresses().has(email.toLowerCase())) return "me";
  return name || email || "(unknown)";
}
/** Bring the drawn stack in line with the conversation, touching only what changed. */
function paint(conv: MailConversation, root: HTMLElement): void {
  const view = state.view === "text" ? "text" : "html";
  for (const button of root.querySelectorAll<HTMLElement>(".conv-hdrs .views [data-view]")) {
    button.classList.toggle("on", button.dataset.view === view);
  }
  const unread = conv.members.filter((m) => !m.is_read).length;
  const people = [...new Set(conv.members.map((m) => shortName(m.from_display)))];
  const named = people.length > 4 ? `${people.slice(0, 3).join(", ")} and ${people.length - 3} more` : people.join(", ");
  setHtml(
    root.querySelector(".conv-sub"),
    `${conv.members.length} messages${unread ? ` · <b>${unread} unread</b>` : ""} · ${escape(named)}`,
  );
  const rules = [...state.highlightStyles.keys()];
  const marks = [...new Set(conv.members.flatMap((m) => m.highlights ?? []))].sort(
    (a, b) => rules.indexOf(a) - rules.indexOf(b),
  );
  setHtml(root.querySelector(".conv-hdrs .hl-marks"), marks.length ? highlightMarks(marks) : "");
  setHtml(root.querySelector(".conv-tasks"), renderMessageTasks(conv.linked, state.tasks.today));

  const folded = foldedIds(conv);
  const earlier = root.querySelector<HTMLElement>("[data-conv-earlier]");
  if (earlier) {
    earlier.hidden = folded.size === 0;
    const text = `${folded.size} earlier message${folded.size === 1 ? "" : "s"}`;
    if (earlier.textContent !== text) earlier.textContent = text;
  }
  for (const m of conv.members) {
    const card = root.querySelector<HTMLElement>(`[data-card="${m.id}"]`);
    if (card) paintCard(conv, m, card, folded.has(m.id), view);
  }
}
function setHtml(target: Element | null, html: string): void {
  if (target && target.innerHTML !== html) target.innerHTML = html;
}
function paintCard(conv: MailConversation, m: MessageHeader, card: HTMLElement, folded: boolean, view: string): void {
  const open = conv.open.has(m.id);
  card.hidden = folded;
  card.classList.toggle("open", open);
  card.classList.toggle("unread", !m.is_read);
  card.classList.toggle("focus", m.id === conv.focus);
  card.classList.toggle("staged", !!m.pending_action);

  const d = conv.details.get(m.id);
  setHtml(card.querySelector(".card-who"), escape(shortName(m.from_display)));
  const own = ownAddresses();
  const to = recipientNames(m.to_display ?? d?.to_display ?? null, own);
  const preview = open ? (to ? `to ${escape(to)}` : "") : escape(m.snippet ?? "");
  setHtml(card.querySelector(".card-pv"), preview);
  setHtml(card.querySelector(".card-marks"), m.highlights?.length ? highlightMarks(m.highlights) : "");
  const when = card.querySelector<HTMLElement>(".card-when");
  if (when) {
    const text = m.date ? listDate(m.date, state.settings.dateFormat, null) : "";
    if (when.textContent !== text) when.textContent = text;
    const title = readerDate(m.date);
    if (when.title !== title) when.title = title;
  }
  if (!open) return;
  if (!d) {
    const text = card.querySelector<HTMLElement>(".card-text")!;
    const waiting = conv.failed.has(m.id)
      ? `<div class="empty">This message could not be loaded.</div>`
      : `<div class="fetching">${icon("fa-rotate", "spin")} Loading message…</div>`;
    if (text.dataset.key !== "waiting" || text.innerHTML !== waiting) {
      text.dataset.key = "waiting";
      text.innerHTML = waiting;
    }
    return;
  }
  paintMeta(conv, d, card.querySelector<HTMLElement>(".card-meta")!);
  paintText(conv, d, card.querySelector<HTMLElement>(".card-text")!, view);
}
/** The recipients, what the headers say against the sender, and the files. */
function paintMeta(conv: MailConversation, d: MessageDetail, meta: HTMLElement): void {
  if (!conv.checks.has(d.id)) {
    conv.checks.set(d.id, null);
    void call<SenderCheck>("sender_check", { id: d.id })
      .then((check) => {
        conv.checks.set(d.id, check);
        paintShown(conv);
      })
      .catch(() => {});
  }
  if (d.has_attachments && !conv.attachments.has(d.id)) {
    conv.attachments.set(d.id, null);
    void call<AttachmentMeta[]>("attachments", { id: d.id })
      .then((list) => {
        conv.attachments.set(d.id, list ?? []);
        paintShown(conv);
      })
      .catch(() => {});
  }
  const own = ownAddresses();
  const cc = recipientNames(d.cc_display, own);
  const files = conv.attachments.get(d.id);
  const chips = !d.has_attachments
    ? ""
    : files
      ? attachmentChips(files, d.id)
      : `<span class="chip dim-chip"><span>Listing attachments…</span></span>`;
  const html =
    (cc ? `<div class="card-cc"><span class="cc">cc</span> ${escape(cc)}</div>` : "") +
    senderWarningsHtml(conv.checks.get(d.id) ?? null) +
    (chips ? `<div class="h-att">${chips}</div>` : "");
  setHtml(meta, html);
}
/** The body, as HTML in a frame of its own or as text; redrawn only when how it is drawn changes. */
function paintText(conv: MailConversation, d: MessageDetail, target: HTMLElement, view: string): void {
  const html = view === "html" && d.body_html ? d.body_html : null;
  if (!html) {
    if (target.dataset.key === "text") return;
    target.dataset.key = "text";
    target.innerHTML = d.body_text
      ? `<pre>${plainBody(d.body_text, [])}</pre>`
      : `<div class="empty">This message has no text</div>`;
    return;
  }
  const remote = d.remote_images_allowed || state.remoteImages.has(d.id);
  const dark = state.darkMail && windowIsDark();
  const wantsImages = /cid:/i.test(html);
  if (wantsImages && !conv.inline.has(d.id)) loadInline(conv, d.id);
  const images = conv.inline.get(d.id) ?? null;
  const key = `html\u0000${remote}\u0000${dark}\u0000${wantsImages && images ? "images" : ""}`;
  if (target.dataset.key === key) return;
  target.dataset.key = key;
  const prepared = prepareHtml(html, remote, images ?? new Map(), [], dark);
  const notice =
    prepared.hasRemote && !remote
      ? `<div class="notice">Remote images are blocked.
          <button type="button" data-conv-images="${d.id}">Load them for this message</button></div>`
      : "";
  target.innerHTML = `${notice}<iframe sandbox="allow-same-origin allow-top-navigation-by-user-activation" title="Message" class="${dark ? "dark-mail" : ""}" srcdoc="${escape(prepared.srcdoc)}"></iframe>`;
  const frame = target.querySelector("iframe")!;
  frame.addEventListener("load", () => fitFrame(frame));
}
/**
 * Give a message's frame the height of what it holds, so the stack scrolls
 * rather than each frame, and keep doing so as its images arrive. A frame
 * above the top of the view that grows pushes the rest down by as much, so
 * the view moves with it rather than jumping.
 */
function fitFrame(frame: HTMLIFrameElement): void {
  const doc = frame.contentDocument;
  if (!doc?.documentElement) return;
  const fit = () => {
    const height = Math.max(doc.documentElement.scrollHeight, doc.body?.scrollHeight ?? 0);
    const before = frame.offsetHeight;
    if (Math.abs(height - before) < 2) return;
    const stack = frame.closest<HTMLElement>(".conv-stack");
    const card = frame.closest<HTMLElement>(".card");
    const above = !!stack && !!card && card.offsetTop + card.offsetHeight <= stack.scrollTop;
    frame.style.height = `${height}px`;
    if (above && stack) stack.scrollTop += frame.offsetHeight - before;
  };
  fit();
  if (doc.body) new ResizeObserver(fit).observe(doc.body);
}
function loadInline(conv: MailConversation, id: number): void {
  conv.inline.set(id, null);
  void call<InlineImage[]>("inline_images", { id })
    .then((list) => {
      conv.inline.set(id, new Map(list.map((i) => [i.content_id, `data:${i.content_type};base64,${i.data_base64}`])));
      paintShown(conv);
    })
    .catch(() => {});
}

// ---------------------------------------------------------------- Moving

/** Put the reader's keys on a member, and say so in the reader. */
function focusCard(conv: MailConversation, id: number, reveal: boolean): void {
  if (conv.focus !== id) {
    conv.focus = id;
    state.detail = conv.details.get(id) ?? null;
    if (!state.detail) void fillConversation(conv);
  }
  if (foldedIds(conv).has(id)) conv.folded = false;
  paintShown(conv);
  if (reveal) revealCard(conv, id, false);
}
/**
 * Scroll a member into view: its top at the top when it is new to the view
 * or taller than it, else just enough to show all of it.
 */
function revealCard(conv: MailConversation, id: number, atTop: boolean): void {
  const root = drawnStack(conv);
  const stack = root && stackOf(root);
  const card = root?.querySelector<HTMLElement>(`[data-card="${id}"]`);
  if (!stack || !card || card.hidden) return;
  const top = card.offsetTop - stack.offsetTop;
  const bottom = top + card.offsetHeight;
  const margin = 8;
  if (atTop) {
    if (conv.members[0]?.id !== id) stack.scrollTop = Math.max(0, top - margin);
    return;
  }
  const viewTop = stack.scrollTop;
  const viewBottom = viewTop + stack.clientHeight;
  if (top < viewTop || card.offsetHeight > stack.clientHeight || top > viewBottom - 40) {
    stack.scrollTop = Math.max(0, top - margin);
  } else if (bottom > viewBottom) {
    stack.scrollTop = bottom - stack.clientHeight + margin;
  }
}
/** Move the reader's keys to the next or the previous message. */
export function moveInConversation(delta: number): boolean {
  const conv = shownConversation();
  if (!conv) return false;
  const at = conv.members.findIndex((m) => m.id === conv.focus);
  const next = Math.min(Math.max(at + delta, 0), conv.members.length - 1);
  if (next === at) {
    setStatus(delta > 0 ? "the newest message · h goes back to the list" : "the first message");
    return true;
  }
  focusCard(conv, conv.members[next]!.id, true);
  return true;
}
/** Move the reader's keys to the first or the last message. */
export function edgeOfConversation(to: "top" | "bottom"): boolean {
  const conv = shownConversation();
  if (!conv) return false;
  const m = to === "top" ? conv.members[0] : conv.members[conv.members.length - 1];
  if (m) focusCard(conv, m.id, true);
  return true;
}
/** Open the message the reader's keys are on, or close it. */
export function toggleInConversation(): boolean {
  const conv = shownConversation();
  if (!conv) return false;
  toggleCard(conv, conv.focus);
  return true;
}
function toggleCard(conv: MailConversation, id: number): void {
  if (conv.open.has(id)) conv.open.delete(id);
  else conv.open.add(id);
  conv.focus = id;
  state.detail = conv.details.get(id) ?? null;
  paintShown(conv);
  void fillConversation(conv);
  revealCard(conv, id, false);
}
/** Open every message of the conversation, or close every one. */
export function openAllInConversation(open: boolean): boolean {
  const conv = shownConversation();
  if (!conv) return false;
  conv.open = new Set(open ? conv.members.map((m) => m.id) : []);
  if (open) conv.folded = false;
  paintShown(conv);
  if (open) void fillConversation(conv);
  revealCard(conv, conv.focus, false);
  setStatus(open ? "every message open" : "every message closed");
  return true;
}
/** Scroll the conversation by pages, as the reader's paging keys do. */
export function scrollConversation(pages: number): boolean {
  const conv = shownConversation();
  const root = conv && drawnStack(conv);
  const stack = root && stackOf(root);
  if (!stack) return false;
  stack.scrollBy({ top: pages * stack.clientHeight * 0.8 });
  return true;
}
/**
 * Put the reader's keys on the message whose frame was clicked. The frame
 * runs no listener of its own (see `events.ts`), so the window reports it.
 */
export function focusFrameCard(frame: HTMLIFrameElement): void {
  const conv = shownConversation();
  const id = Number(frame.closest<HTMLElement>("[data-card]")?.dataset.card);
  if (conv && id) focusCard(conv, id, false);
}
/**
 * The body of the message the reader's keys are on: its frame, or the text
 * it is drawn as. Null outside a conversation.
 */
export function focusedBody(): HTMLIFrameElement | HTMLElement | null {
  const conv = shownConversation();
  const root = conv && drawnStack(conv);
  const card = root?.querySelector<HTMLElement>(`[data-card="${conv!.focus}"]`);
  if (!card) return null;
  return card.querySelector<HTMLIFrameElement>(".card-text iframe") ?? card.querySelector<HTMLElement>(".card-text pre");
}
/** The files of a member, as far as they are listed; undefined outside the conversation. */
export function conversationAttachments(id: number): AttachmentMeta[] | null | undefined {
  const conv = state.conversation;
  return conv?.attachments.has(id) ? conv.attachments.get(id) : undefined;
}

el.reader.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;
  const conv = shownConversation();
  if (!conv || !target.closest(".conv")) return;
  if (target.closest("[data-conv-earlier]")) {
    conv.folded = false;
    paintShown(conv);
    return;
  }
  const images = target.closest<HTMLElement>("[data-conv-images]");
  if (images) {
    state.remoteImages.add(Number(images.dataset.convImages));
    paintShown(conv);
    return;
  }
  const card = target.closest<HTMLElement>("[data-card]");
  if (!card) return;
  const id = Number(card.dataset.card);
  if (target.closest("[data-card-head]") && !target.closest("a, button, [data-att]")) {
    toggleCard(conv, id);
    return;
  }
  focusCard(conv, id, false);
});
