import { restoreScrolls, scrollsIn } from "./scrollkeep";
import { attachChatReader, chatActive, chatReaderMarkup } from "./chat";
import { cursorMessageId } from "./actions";
import { renderTaskDetail, renderMessageTasks } from "./taskdetail";
import { linkify, linkTarget, messageUrlId } from "./links";
import { startDrag } from "@crabnebula/tauri-plugin-drag";
import { runTask } from "./tasks";
import { VIEW_LABELS, windowIsDark } from "./appearance";
import { invoke } from "@tauri-apps/api/core";
import { call, setStatus } from "./bridge";
import { dispatchCommand } from "./commands";
import {
  SPOOF_TITLE,
  escape,
  icon,
  keyValue,
  parseFrom,
  readerDate,
  recipientNames,
  spoofedName,
  subjectIconsHtml,
} from "./format";
import { svgIcon } from "./icons";
import { conversationAttachments, drawConversation, focusedBody, isConversationRow } from "./conversation";
import { applyReaderPresence, cursorRow, markTerms } from "./list";
import { AttachmentMeta, InlineImage, MessageDetail, ViewMode, MessageHeader, SenderCheck } from "./model";
import { checkedHref, isTrackingPixel, linkSuspicion, openSuspiciousLink } from "./safety";
import { highlightMarks, taskSectionActive, iconExists } from "./sidebar";
import { el, openPanel, state } from "./state";
import { focusTaskEditor } from "./taskedit";
import { focusStepEditor, redrawKeepingStepField } from "./stepsui";
import { openLinkedMessage, taskKeys } from "./taskview";

/**
 * Which renderings this message actually has.
 *
 * Offering a view a message cannot produce is worse than not offering it: the
 * control implies there is a difference to see, and clicking it shows nothing.
 */
export function availableViews(d: MessageDetail): ViewMode[] {
  const views: ViewMode[] = [];
  if (d.body_html) views.push("html");
  // A message with no body at all, such as an empty draft, is shown as the
  // empty text it is, rather than sent to the source view.
  if (d.body_text || !d.body_html) views.push("text");
  // Always available: the source exists whether or not anything could be made
  // of it, and it is the only view that answers what a message really contains.
  views.push("raw");
  return views;
}
/** The view to show: the one asked for, or the best this message can manage. */
function effectiveView(d: MessageDetail): ViewMode {
  const available = availableViews(d);
  if (available.includes(state.view)) return state.view;
  // What was asked for is not on offer, so the configured default, and failing
  // that whatever this message does have.
  if (available.includes(state.settings.defaultView)) return state.settings.defaultView;
  return available[0];
}
/**
 * What the reader last drew for the message list: whose message — `null`
 * for none, `undefined` for a row not loaded yet — and whether its body or
 * only its headers. `null` while a task or a panel has the reader.
 */
let drawn: { id: number | null | undefined; body: boolean; conversation?: boolean } | null = null;
export function readerDrawn(): { id: number | null | undefined; body: boolean; conversation?: boolean } | null {
  return drawn;
}
/**
 * The markup of the message last drawn, while the reader still shows it. A
 * render that would write the same again leaves the pane alone: rewriting it
 * reloads the message frame and drops whatever text was selected, and renders
 * come from syncs and status updates that have nothing to do with the message.
 */
let written: string | null = null;
/** The task the reader last showed, so that drawing it again keeps its scroll position. */
let drawnTask: number | null = null;
export function renderReader(): void {
  drawn = null;
  const previous = written;
  written = null;
  const taskBefore = drawnTask;
  drawnTask = null;
  if (state.booting) {
    el.reader.innerHTML = "";
    return;
  }
  // A panel opened over an empty list brings the pane back to hold it.
  applyReaderPresence();
  // A panel asked for is shown whatever the cursor is on: left under a thread
  // or a task it would hold the reader without anybody seeing it.
  const panel = openPanel();
  if (panel) {
    el.reader.className = "pane focused";
    el.reader.innerHTML =
      `<header><h1>${escape(panel.title)}</h1></header>` +
      (panel.html ?? `<pre class="raw">${escape(panel.text ?? "")}</pre>`);
    return;
  }
  // The reader shows whatever the cursor is on, which in Today's mail half is a
  // message even though a task view is open. A task and a message are both "the
  // thing being looked at", and showing one under the other would leave no
  // answer to what the pane's own bindings act on.
  if (chatActive()) {
    el.reader.className = `pane chat${state.focus === "reader" ? " focused" : ""}`;
    el.reader.innerHTML = chatReaderMarkup();
    attachChatReader();
    return;
  }
  const showingTask = taskSectionActive();
  el.reader.className =
    `pane${state.focus === "reader" ? " focused" : ""}` + (showingTask ? " task-detail" : "");

  if (showingTask) {
    const task = state.tasks.detail;
    // The same task drawn again stays where it was scrolled; another starts at its top.
    const kept = task && task.task.id === taskBefore ? scrollsIn(el.reader) : new Map<string, number>();
    drawnTask = task?.task.id ?? null;
    redrawKeepingStepField(() => {
      el.reader.innerHTML = task
        ? renderTaskDetail(task, {
            today: state.tasks.today,
            selectedSteps: state.tasks.stepSelection,
            stepEditing: state.tasks.stepEditing,
            editing: state.tasks.editing,
            provisional: state.tasks.provisional === task.task.id,
            focused: state.focus === "reader",
            notifications: state.notificationPermission,
            keys: taskKeys(["postpone-task", "set-due-date", "add-reminder", "set-recurrence", "add-tag", "add-step", "attach-file"]),
          })
        : `<div class="empty">Select a task</div>`;
    });
    restoreScrolls(el.reader, kept);
    // The field has to exist before it can be focused, so this follows the
    // write rather than preceding it.
    focusTaskEditor();
    focusStepEditor();
    return;
  }

  // The message under the cursor, or that it is loading: never another one,
  // whatever is still in `state.detail` (see `followCursor`).
  const cursorId = cursorMessageId();
  if (cursorId === null) {
    drawn = { id: null, body: false };
    el.reader.innerHTML = `<div class="empty">Select a message</div>`;
    return;
  }
  // A collapsed thread's row stands for the conversation, and the reader
  // shows all of it.
  const row = cursorRow();
  if (cursorId !== undefined && isConversationRow(row)) {
    drawn = { id: cursorId, body: drawConversation(row), conversation: true };
    return;
  }
  const d = state.detail?.id === cursorId ? state.detail : null;
  if (!d) {
    drawn = { id: cursorId, body: false };
    el.reader.innerHTML = loadingMessage(cursorRow());
    return;
  }
  drawn = { id: d.id, body: true };

  const available = availableViews(d);
  const view = effectiveView(d);
  // A message with one rendering has nothing to switch between.
  const toggle =
    available.length > 1
      ? `<nav class="views">${available
          .map(
            (mode) =>
              `<button type="button" data-view="${mode}"${
                mode === view ? ' class="on"' : ""
              }>${VIEW_LABELS[mode]}</button>`,
          )
          .join("")}</nav>`
      : "";

  const terms = state.search?.terms ?? [];
  const headers = messageHeader(d, toggle, terms);

  let body: string;
  if (view === "raw") {
    if (state.raw?.id !== d.id) fetchRaw(d.id);
    body =
      state.raw?.id === d.id
        ? `<pre class="raw">${escape(state.raw.text)}</pre>`
        : `<div class="empty">Fetching the source…</div>`;
  } else if (view === "html") {
    // HTML mail goes into a sandboxed iframe — no scripts, no same-origin
    // access — under a policy of its own that keeps it off the network.
    const html = d.body_html ?? "";
    if (/cid:/i.test(html) && state.inline?.id !== d.id) loadInlineImages(d.id);
    const images = state.inline?.id === d.id ? state.inline.images : null;
    const remote = d.remote_images_allowed || state.remoteImages.has(d.id);
    const dark = state.darkMail && windowIsDark();
    const prepared = prepareHtml(html, remote, images ?? new Map(), terms, dark);
    const notice =
      prepared.hasRemote && !remote
        ? `<div class="notice">Remote images are blocked.
            <button type="button" data-action="load-remote-images">Load them for this message</button>
          </div>`
        : "";
    // Same origin, still without scripts, so the window can see what is
    // selected in the frame and copy it, and hear the keys pressed there. A
    // clicked link may navigate the window, which the shell turns into
    // opening the browser (see `prepareHtml`).
    body = `${notice}<iframe sandbox="allow-same-origin allow-top-navigation-by-user-activation" title="Message" class="${dark ? "dark-mail" : ""}" srcdoc="${escape(prepared.srcdoc)}"></iframe>`;
  } else {
    body = d.body_text
      ? `<pre>${plainBody(d.body_text, terms)}</pre>`
      : `<div class="empty">This message has no text</div>`;
  }

  const markup = headers + senderWarnings(d) + body;
  written = markup;
  if (markup === previous) return;
  el.reader.innerHTML = markup;
  state.matchIndex = -1;
}
/** The line a forwarded or an Outlook-quoted message starts below. */
const FORWARDED = /^-{5,} ?(Forwarded message|Original Message) ?-{5,}$/i;

/**
 * The lines of a plain-text body that are its signature: from the `-- ` line
 * (its space is often trimmed on the way) to the end, or to the quote below
 * it when the signature was put above what it answers.
 */
export function signatureLines(lines: string[]): [number, number] | null {
  const start = lines.findIndex((line) => /^-- ?\r?$/.test(line));
  if (start < 0) return null;
  let end = lines.findIndex((line, i) => i > start && (line.startsWith(">") || FORWARDED.test(line)));
  if (end < 0) return [start, lines.length];
  // The attribution, "On Monday, Thomas wrote:", belongs to the quote.
  if (end - 1 > start && lines[end].startsWith(">") && lines[end - 1].trim()) end--;
  return [start, end];
}

/** A plain-text body as markup, its signature set apart. */
export function plainBody(text: string, terms: string[]): string {
  // Escape first, then linkify, then mark: the anchors and the marks are
  // the only markup here that did not come from the escaper, so nothing
  // from the message can become HTML.
  const render = (part: string) => markTerms(linkify(escape(part)), terms);
  const lines = text.split("\n");
  const signature = signatureLines(lines);
  if (!signature) return render(text);
  const [start, end] = signature;
  const parts: string[] = [];
  if (start > 0) parts.push(`${render(lines.slice(0, start).join("\n"))}\n`);
  parts.push(`<span class="sig">${render(lines.slice(start, end).join("\n"))}</span>`);
  if (end < lines.length) parts.push(`\n${render(lines.slice(end).join("\n"))}`);
  return parts.join("");
}

/**
 * The reader while a message's body is on its way: the headers its row
 * already has, so moving the cursor shows the new message at once, and a line
 * saying the rest is coming.
 */
function loadingMessage(row: MessageHeader | undefined): string {
  const loading = `<div class="fetching">${icon("fa-rotate", "spin")} Loading message…</div>`;
  if (!row) return loading;
  return `<header class="hdrs">
      <h1 class="h-subj" data-k="Subject">${subjectIconsHtml(row.subject_icons, iconExists)}${escape(row.subject || "(no subject)")}</h1>
      ${fromLine(row.from_display, [])}
      ${keyValue("hdr h-date", "Date", escape(readerDate(row.date)))}
    </header>${loading}`;
}
/** The sender as the header shows it: the name, and the address beside it. */
function fromLine(from: string | null, terms: string[]): string {
  const { name, email } = parseFrom(from);
  const spw = spoofedName(name, email)
    ? `<span class="spw" title="${SPOOF_TITLE}" aria-label="${SPOOF_TITLE}">${svgIcon("alert")}</span>`
    : "";
  const value = email
    ? `<b>${markTerms(escape(name), terms)}</b>${spw}<span class="addr${spw ? " spoof" : ""}">${markTerms(escape(email), terms)}</span>`
    : `<b>${markTerms(escape(name), terms)}</b>`;
  return keyValue("hdr h-from", "From", value);
}
/**
 * What the headers say against the sender, as notices under the header:
 * the receiving server found the message forged, or answers go to another
 * domain.
 *
 * Fetched once per message, after the body, and quietly: a message read
 * offline is no less readable for the check not coming.
 */
function senderWarnings(d: MessageDetail): string {
  if (state.senderCheck?.id !== d.id) {
    state.senderCheck = { id: d.id, check: null };
    void invoke<SenderCheck>("sender_check", { id: d.id })
      .then((check) => {
        if (state.senderCheck?.id !== d.id) return;
        state.senderCheck = { id: d.id, check };
        if (check.failed || check.reply_elsewhere) renderReader();
      })
      .catch(() => {});
    return "";
  }
  return senderWarningsHtml(state.senderCheck.check);
}
/** The notices for what the headers say against a sender; nothing until checked. */
export function senderWarningsHtml(check: SenderCheck | null): string {
  if (!check) return "";
  const warn = (text: string) => `<div class="notice warn">${svgIcon("alert")}<span>${text}</span></div>`;
  return [
    check.failed
      ? warn(`The sender could not be verified (${escape(check.failed)}): this message may not be from who it says.`)
      : "",
    check.reply_elsewhere
      ? warn(`Replies go to <b>${escape(check.reply_elsewhere)}</b>, outside the sender's domain.`)
      : "",
  ].join("");
}
/** The addresses that are the user's own, to be called `me`. */
function ownAddresses(): Set<string> {
  return new Set(state.accounts.map((a) => a.email.toLowerCase()));
}
/**
 * The header block: the subject, the sender beside the date, the recipients
 * on one line, and the files and linked tasks as chips under them.
 */
function messageHeader(d: MessageDetail, toggle: string, terms: string[]): string {
  const own = ownAddresses();
  const to = recipientNames(d.to_display, own);
  const cc = recipientNames(d.cc_display, own);
  const recipients = [to, cc ? `<span class="cc">cc</span> ${escape(cc)}` : ""]
    .map((part, i) => (i === 0 ? escape(part) : part))
    .filter(Boolean)
    .join(" · ");
  const chips = renderMessageTasks(state.tasks.linked, state.tasks.today) + renderAttachments(d);
  return `<header class="hdrs">
      ${toggle}
      <h1 class="h-subj" data-k="Subject">${subjectIconsHtml(d.subject_icons, iconExists)}${markTerms(escape(d.subject || "(no subject)"), terms)}${
        d.highlights?.length ? `<span class="hl-marks">${highlightMarks(d.highlights)}</span>` : ""
      }</h1>
      ${fromLine(d.from_display, terms)}
      ${keyValue("hdr h-date", "Date", `<span title="${escape(d.date ?? "")}">${escape(readerDate(d.date))}</span>`)}
      ${recipients ? keyValue("hdr h-to", "To", recipients) : ""}
      ${chips ? `<div class="h-att">${chips}</div>` : ""}
    </header>`;
}
/** Bytes as a person reads them: 12 KB, 3.4 MB. */
export function describeSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
/** The attachments a reader wants to see: everything the body does not show. */
export function listedAttachments(list: AttachmentMeta[]): AttachmentMeta[] {
  return list.filter((a) => !(a.is_inline && a.content_id));
}
/**
 * The attachments of the message in the reader, each openable, previewable,
 * saveable and draggable.
 *
 * Fetched once per message, after the body: listing them needs the full
 * source, which the body did not, and a message is read before its
 * attachments are. The list fills in when it arrives.
 */
function renderAttachments(d: MessageDetail): string {
  if (!d.has_attachments) return "";
  if (state.attachments?.id !== d.id) {
    state.attachments = { id: d.id, list: null };
    void call<AttachmentMeta[]>("attachments", { id: d.id })
      .then((list) => {
        if (state.attachments?.id !== d.id) return;
        state.attachments = { id: d.id, list: list ?? [] };
        renderReader();
      })
      .catch(() => {
        // `call` has already said why; the message is still readable.
      });
    return `<span class="chip dim-chip">${svgIcon("clip")}<span>Listing attachments…</span></span>`;
  }
  const list = state.attachments.list;
  if (!list) {
    return `<span class="chip dim-chip">${svgIcon("clip")}<span>Listing attachments…</span></span>`;
  }
  return attachmentChips(list);
}
/**
 * Each file a chip: a click opens it, Quick Look and Save sit at its end,
 * and it can be dragged out onto the Finder. `message` names whose files
 * they are where the reader shows more than one message.
 */
export function attachmentChips(list: AttachmentMeta[], message?: number): string {
  const owner = message === undefined ? "" : ` data-att-msg="${message}"`;
  return listedAttachments(list)
    .map(
      (a, i) =>
        `<span class="chip att${a.risk ? " risky" : ""}" draggable="true" data-att="${i}"${owner} title="${
          a.risk ? `This file ${escape(a.risk)} · ` : ""
        }${escape(a.content_type)} · drag to copy out">` +
        `<span class="att-name" data-att-action="open">${svgIcon(a.risk ? "alert" : "clip")}${escape(a.filename)} <span class="dim">${describeSize(a.size)}</span></span>` +
        `<button type="button" data-att-action="look" title="Quick Look">${icon("fa-eye")}</button>` +
        `<button type="button" data-att-action="save" title="Save…">${icon("fa-download")}</button>` +
        `</span>`,
    )
    .join("");
}
/**
 * The attachment a chip in the reader stands for, if the list is loaded: of
 * the message `message` names in a conversation, else of the message in the
 * reader — in a conversation, the one its keys act on.
 */
function attachmentAt(index: number, message?: number): { id: number; a: AttachmentMeta } | undefined {
  const id = message ?? state.detail?.id;
  if (id === undefined) return undefined;
  const list = conversationAttachments(id) ?? (state.attachments?.id === id ? state.attachments.list : null);
  const a = list ? listedAttachments(list)[index] : undefined;
  return a ? { id, a } : undefined;
}
/** The attachment a chip stands for, from the chip. */
function attachmentOfChip(chip: HTMLElement): { id: number; a: AttachmentMeta } | undefined {
  const message = chip.dataset.attMsg;
  return attachmentAt(Number(chip.dataset.att), message === undefined ? undefined : Number(message));
}
/** Open, preview or save an attachment of the message in the reader. */
export async function actOnAttachment(index: number, action: string, message?: number): Promise<void> {
  const found = attachmentAt(index, message);
  if (!found) {
    setStatus(`no attachment ${index + 1}`);
    return;
  }
  const { a } = found;
  const args = { id: found.id, part: a.part_id };
  if (action === "look") {
    await call("quick_look_attachment", args);
    return;
  }
  if (action === "save") {
    // The shell asks where: the path the bytes go to is one a person picked.
    const saved = await call<string | null>("save_attachment", args);
    setStatus(saved ? `saved ${a.filename}` : "not saved");
    return;
  }
  // A dangerous file is asked about by the shell, in a native dialog.
  const opened = await call<{ path: string; revealed: boolean; declined: boolean }>("open_attachment", args);
  if (opened.declined) {
    setStatus(`${a.filename} not opened`);
    return;
  }
  setStatus(opened.revealed ? `revealed ${a.filename} in Finder` : `opened ${a.filename}`);
}
el.reader.addEventListener("click", (e) => {
  const target = (e.target as HTMLElement).closest<HTMLElement>("[data-att-action]");
  if (!target) return;
  const row = target.closest<HTMLElement>("[data-att]");
  if (!row) return;
  e.preventDefault();
  const message = row.dataset.attMsg === undefined ? undefined : Number(row.dataset.attMsg);
  void actOnAttachment(Number(row.dataset.att), target.dataset.attAction ?? "open", message).catch(() => {
    // `call` has already said why.
  });
});
/** Files already written out for dragging, by message and part. */
const draggable = new Map<string, string>();
// Dragging needs a file on disk and a native drag session, neither of which
// the web platform's own drag gives. The path is materialised on the press
// that begins the drag, so that by the time the pointer leaves the row the
// file is usually there; the drag itself is handed to the plugin.
el.reader.addEventListener("mousedown", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-att]");
  const found = row ? attachmentOfChip(row) : undefined;
  if (!found) return;
  const { id, a } = found;
  const key = `${id}\u0000${a.part_id}`;
  if (draggable.has(key)) return;
  void call<string>("attachment_path", { id, part: a.part_id })
    .then((path) => draggable.set(key, path))
    .catch(() => {
      // `call` has already said why; the row still opens on click.
    });
});
el.reader.addEventListener("dragstart", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-att]");
  const found = row ? attachmentOfChip(row) : undefined;
  if (!found) return;
  const { id, a } = found;
  e.preventDefault();
  const key = `${id}\u0000${a.part_id}`;
  const known = draggable.get(key);
  const path = known
    ? Promise.resolve(known)
    : call<string>("attachment_path", { id, part: a.part_id });
  void path
    .then(async (file) => {
      draggable.set(key, file);
      const image = a.content_type.toLowerCase().startsWith("image/");
      const iconPath = image ? file : await call<string>("drag_icon");
      await startDrag({ item: [file], icon: iconPath });
    })
    .catch(() => {
      // `call` has already said why; a drag that did not start is nothing.
    });
});
/**
 * The attributes that can name a resource to fetch, and so can carry a `cid:`
 * reference or a remote URL.
 */
const RESOURCE_ATTRIBUTES = ["src", "background", "poster", "srcset", "href"];
/** True for a URL that would leave the machine: `http:`, `https:` or `//`. */
const isRemoteUrl = (value: string): boolean => /^\s*(https?:)?\/\//i.test(value);
/**
 * Select what the reader shows, for ⌘A there: the body of a message — in its
 * frame, when it is HTML — or the whole pane for a task or a panel. Returns
 * false when there is nothing to select.
 */
export function selectReaderText(): boolean {
  // In a conversation, the message the reader's keys act on.
  const focused = focusedBody();
  if (focused && !(focused instanceof HTMLIFrameElement)) {
    window.getSelection()?.selectAllChildren(focused);
    return true;
  }
  const frame = focused ?? el.reader.querySelector<HTMLIFrameElement>("iframe");
  const frameBody = frame?.contentDocument?.body;
  if (frame && frameBody) {
    // The keyboard stays with the pane; ⌘C copies from the frame all the same.
    window.getSelection()?.removeAllRanges();
    frame.contentDocument.getSelection()?.selectAllChildren(frameBody);
    return true;
  }
  const target = drawn?.body ? el.reader.querySelector("pre") : drawn ? null : el.reader;
  if (!target) return false;
  window.getSelection()?.selectAllChildren(target);
  return true;
}
/**
 * Prepare a message's HTML for the sandboxed iframe.
 *
 * Mail is hostile input, and a `srcdoc` document inherits this window's
 * policy rather than escaping it, so the document gets a policy of its own
 * that allows no network at all: images from `data:` only — the embedded
 * ones, resolved here from the message's own parts — and styles only inline.
 * Loading remote images, per message or for a sender on the allow list,
 * widens the image rule and nothing else: a stylesheet or a font fetched from
 * the sender's server is as much a tracker as a pixel is.
 *
 * `hasRemote` says whether there was anything to block, so that a message
 * with nothing remote in it is not decorated with a notice about it.
 */
export function prepareHtml(
  html: string,
  remote: boolean,
  images: Map<string, string>,
  terms: string[] = [],
  dark = false,
): { srcdoc: string; hasRemote: boolean } {
  const doc = new DOMParser().parseFromString(html, "text/html");
  let hasRemote = false;
  if (terms.length) markTextNodes(doc, terms);

  // Taken out rather than blocked, so that a pixel stays out once remote
  // images are let in, and a message whose only remote image was one has
  // nothing to ask about. A refresh would take the frame to the sender's
  // page, and the hints and `base` reach past the policy or re-aim links.
  for (const image of doc.querySelectorAll("img")) if (isTrackingPixel(image)) image.remove();
  const outOfBounds = [
    "dns-prefetch",
    "preconnect",
    "prefetch",
    "prerender",
    "preload",
  ].map((rel) => `link[rel~="${rel}" i]`);
  for (const element of doc.querySelectorAll([...outOfBounds, 'meta[http-equiv="refresh" i]', "base"].join(", "))) {
    element.remove();
  }

  for (const element of doc.querySelectorAll("*")) {
    for (const name of RESOURCE_ATTRIBUTES) {
      const value = element.getAttribute(name);
      if (value === null) continue;
      const cid = /^\s*cid:(.+)$/i.exec(value);
      if (cid) {
        const resolved = images.get(decodeCid(cid[1]!));
        if (resolved) element.setAttribute(name, resolved);
      } else if (isRemoteUrl(value) && (name !== "href" || element.tagName === "LINK")) {
        hasRemote = true;
      }
    }
    const style = element.getAttribute("style");
    if (style && /url\(\s*['"]?\s*(https?:)?\/\//i.test(style)) hasRemote = true;
  }
  // A link opens in the browser. WebKit runs no listener in a frame without
  // scripts, so the click is left to navigate the window instead, and the
  // shell's navigation guard opens the browser and cancels it. A link within
  // the message (`#section`) stays in the frame. One that is not what it says
  // is pointed at an address the guard hands back, to ask first; its title
  // shows where it really goes, whatever the message set it to.
  for (const link of doc.querySelectorAll("a[href], area[href]")) {
    const raw = link.getAttribute("href")!.trim();
    link.removeAttribute("ping");
    if (raw.startsWith("#")) {
      // A `srcdoc` document resolves `#section` against this window's
      // address, which would load the app into the frame.
      link.setAttribute("href", `about:srcdoc${raw}`);
      link.removeAttribute("target");
      continue;
    }
    // `//host/path` takes the scheme of the page it is on, which here is the
    // app's own; on the web it would be https.
    const href = raw.startsWith("//") ? `https:${raw}` : raw;
    if (!opensOutside(href)) {
      // A relative address would resolve against this window's own, and a
      // scheme the shell does not open goes nowhere: the text stays, the
      // link does not.
      link.removeAttribute("href");
      link.removeAttribute("target");
      link.setAttribute("title", "This link leads nowhere Orb opens");
      continue;
    }
    link.setAttribute("href", href);
    link.setAttribute("target", "_top");
    link.setAttribute("title", href);
    const text = link.textContent || link.getAttribute("alt") || "";
    const why = linkSuspicion(href, text);
    if (why) link.setAttribute("href", checkedHref(href, text, why));
  }
  for (const style of doc.querySelectorAll("style")) {
    if (/(url\(\s*['"]?\s*(https?:)?\/\/|@import)/i.test(style.textContent ?? "")) {
      hasRemote = true;
    }
  }

  const policy = [
    "default-src 'none'",
    `img-src data: cid:${remote ? " http: https:" : ""}`,
    "style-src 'unsafe-inline'",
    "font-src data:",
  ].join("; ");
  const meta = doc.createElement("meta");
  meta.httpEquiv = "Content-Security-Policy";
  meta.content = policy;
  // Mail that names no font would be set in the browser's default, a serif;
  // other mail clients use a sans-serif. Inherited from the root, so any
  // font the message sets, on any element, still wins.
  const defaults = doc.createElement("style");
  defaults.textContent = 'html { font-family: -apple-system, "Helvetica Neue", Helvetica, sans-serif; }';
  doc.head.prepend(meta, defaults);
  if (dark) {
    // The frame is inverted whole to draw it dark; inverting the pictures
    // again puts them back as they were.
    const style = doc.createElement("style");
    style.textContent =
      'img, picture, video, svg, [style*="background-image"], [background] ' +
      "{ filter: invert(1) hue-rotate(180deg); }";
    doc.head.append(style);
  }

  const doctype = doc.doctype ? "<!DOCTYPE html>" : "";
  return { srcdoc: doctype + doc.documentElement.outerHTML, hasRemote };
}
/**
 * Whether a link in a message is one the shell opens: an absolute `http`,
 * `https` or `mailto` address. Anything relative resolves against the
 * window's own address, which is the app; anything else is a scheme the shell
 * refuses, so the link is better gone than dead.
 */
export function opensOutside(href: string): boolean {
  let url: URL;
  try {
    url = new URL(href, "https://relative.invalid");
  } catch {
    return false;
  }
  if (url.protocol === "mailto:") return true;
  if (url.protocol !== "http:" && url.protocol !== "https:") return false;
  return url.hostname !== "" && url.hostname !== "relative.invalid";
}
/**
 * Mark the search's words in a parsed HTML message.
 *
 * Text nodes only, so nothing in an attribute or a style changes, and the
 * markup added is a `<mark>` with its colour inline — the message's own
 * stylesheet is not going to style it, and the sandbox allows inline styles.
 */
function markTextNodes(doc: Document, terms: string[]): void {
  const pattern = new RegExp(
    `(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`,
    "gi",
  );
  const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
  const nodes: Text[] = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const parent = node.parentElement;
    if (!parent || /^(SCRIPT|STYLE|TITLE)$/.test(parent.tagName)) continue;
    if (pattern.test(node.nodeValue ?? "")) nodes.push(node as Text);
    pattern.lastIndex = 0;
  }
  for (const node of nodes) {
    const span = doc.createElement("span");
    span.innerHTML = (node.nodeValue ?? "")
      .replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]!)
      .replace(pattern, '<mark style="background:#f0c674;color:#000">$1</mark>');
    node.replaceWith(...Array.from(span.childNodes));
  }
}
/** A `cid:` reference as the Content-ID it names, percent-decoded if encoded. */
function decodeCid(reference: string): string {
  const bare = reference.trim();
  try {
    return decodeURIComponent(bare);
  } catch {
    return bare;
  }
}
/**
 * Fetch the embedded images of the message being read, then show them.
 *
 * Asked for only once per message, and only for HTML that refers to one:
 * the images come out of the full source, which may have to be fetched.
 */
function loadInlineImages(id: number): void {
  state.inline = { id, images: null };
  void call<InlineImage[]>("inline_images", { id })
    .then((list) => {
      if (state.inline?.id !== id) return;
      state.inline = {
        id,
        images: new Map(
          list.map((i) => [i.content_id, `data:${i.content_type};base64,${i.data_base64}`]),
        ),
      };
      if (state.detail?.id === id) renderReader();
    })
    .catch(() => {
      // `call` has already said why; the message shows without its images.
    });
}
/**
 * Switch rendering.
 *
 * The choice sticks, because it is a preference rather than a per-message
 * decision — someone who prefers plain text prefers it for the next message
 * too. `raw` is left out of that: it is something you go and look at, not a way
 * you read your mail.
 */
export function setView(mode: ViewMode): void {
  state.view = mode;
  renderReader();
}
/** The message whose source is being fetched, so a redraw does not ask again. */
let rawPending: number | null = null;
/** Fetch a message's source for the source view, which draws it once it is in. */
function fetchRaw(id: number): void {
  if (rawPending === id) return;
  rawPending = id;
  void runTask({ id: "raw", label: "Fetching source" }, () => call<string>("raw_message", { id }))
    .then((text) => {
      // The reader may have moved on while the server was answering.
      if (state.detail?.id !== id) return;
      state.raw = { id, text };
      renderReader();
    })
    .catch(() => {
      // Nothing to show as source; fall back rather than sit on "fetching".
      if (state.detail?.id !== id) return;
      if (state.view === "raw") state.view = "text";
      renderReader();
    })
    .finally(() => {
      if (rawPending === id) rawPending = null;
    });
}
el.reader.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLElement>("[data-view]");
  if (!button) return;
  setView(button.dataset.view as ViewMode);
});
// A button in the reader runs a command by name, so it cannot do anything a
// binding could not.
el.reader.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLElement>("[data-action]");
  if (!button) return;
  void dispatchCommand(button.dataset.action ?? "", 1);
});
/**
 * A link in a message opens in the browser, never in this window.
 *
 * Navigating the webview to a page from an email would replace the app with
 * whatever the message linked to, which is both a broken client and the start
 * of a phishing page wearing Orb's own chrome. The scheme is checked again in
 * the core, because this side of it is reachable from message content.
 */
el.reader.addEventListener("click", (e) => {
  const link = (e.target as HTMLElement).closest<HTMLElement>("[data-link]");
  if (!link) return;
  e.preventDefault();
  const url = linkTarget(link.dataset.link ?? "");
  // A link to a mail opens the mail here, found as a task's link is (§6.2).
  const mail = messageUrlId(url);
  if (mail) {
    const task = taskSectionActive() ? (state.tasks.detail?.task.id ?? 0) : 0;
    void openLinkedMessage(task, mail).catch(() => {});
    return;
  }
  const why = linkSuspicion(url, url);
  const opening = why ? openSuspiciousLink(url, url, why) : call("open_external", { url });
  void opening.catch(() => {
    // `call` has already said why in the status line.
  });
});

// A message drawn dark follows the system when it changes appearance.
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (state.settings.theme === "system" && state.detail) renderReader();
});
