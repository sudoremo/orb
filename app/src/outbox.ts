import { askFirst, call, setStatus } from "./bridge";
import { fromTemplate, openCompose, DraftTemplate } from "./composing";
import { escape, formatDate, icon, keyValue } from "./format";
import { QueuedSend } from "./model";
import { showPanel } from "./panels";
import { renderFolders } from "./sidebar";
import { el, state } from "./state";
import { renderSteady } from "./statusline";

const PANEL = "Outbox";

/** The messages the server has refused, which wait for the user. */
export const failedSends = (): QueuedSend[] => state.outbox.filter((q) => q.failed_at !== null);
/** The messages that wait for their server. */
export const waitingSends = (): QueuedSend[] => state.outbox.filter((q) => q.failed_at === null);

/** Read the Outbox again, and redraw everything that shows it. */
export async function refreshOutbox(): Promise<void> {
  state.outbox = (await call<QueuedSend[]>("outbox").catch(() => null)) ?? [];
  renderFolders();
  renderSteady();
  if (state.info?.title === PANEL) showOutbox();
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

function item(q: QueuedSend): string {
  const failed = q.failed_at !== null;
  const to = [...q.to, ...q.cc, ...q.bcc];
  const status = failed
    ? `${icon("fa-circle-exclamation", "fa-fw")} Not sent: ${escape(q.error ?? "refused")}`
    : `${icon("fa-clock", "fa-fw")} Waiting for the mail server${
        q.error ? `, ${plural(q.attempts, "attempt")} so far · ${escape(q.error)}` : ""
      } · tried again with every sync`;
  const body = q.body.length > 1200 ? `${q.body.slice(0, 1200)}…` : q.body;
  return `<section class="outbox-item ${failed ? "failed" : "waiting"}">
    <h2>${escape(q.subject || "(no subject)")}
      <button type="button" data-outbox="retry" data-id="${q.id}" title="Send it now">${failed ? "Retry" : "Send now"}</button>
      <button type="button" data-outbox="edit" data-id="${q.id}" title="Open it in a compose window; it moves to Drafts">Edit</button>
      <button type="button" data-outbox="delete" data-id="${q.id}" title="Delete it unsent">Delete</button>
    </h2>
    ${keyValue("hdr outbox-status", "Status", status)}
    ${keyValue("hdr", "From", `${escape(q.from)} <span class="dim">via ${escape(q.sending_as)}</span>`)}
    ${keyValue("hdr", "To", escape(to.join(", ")))}
    ${keyValue("hdr", "Queued", escape(formatDate(q.queued_at, "%F %R")))}
    ${q.attachments.length ? keyValue("hdr", "Attached", escape(q.attachments.join(", "))) : ""}
    <pre class="outbox-body">${escape(body)}</pre>
  </section>`;
}

/** The Outbox in the reader: every queued message, the refused ones first. */
export function showOutbox(): void {
  const failed = failedSends();
  const waiting = waitingSends();
  if (failed.length + waiting.length === 0) {
    showPanel(PANEL, `<div class="empty">The Outbox is empty: everything has been sent.</div>`);
    return;
  }
  const summary = [
    failed.length ? `${failed.length} not sent` : "",
    waiting.length ? `${waiting.length} waiting` : "",
  ]
    .filter(Boolean)
    .join(" · ");
  showPanel(
    PANEL,
    `<div class="panel-actions">${summary}
       <button type="button" data-outbox="retry-all">${failed.length ? "Retry all" : "Send all now"}</button>
     </div>${[...failed, ...waiting].map(item).join("")}`,
  );
}

/** Put every message back in line and try to send them now. */
export async function retryOutbox(ids = state.outbox.map((q) => q.id)): Promise<void> {
  if (ids.length === 0) {
    setStatus("the Outbox is empty");
    return;
  }
  setStatus(`sending ${plural(ids.length, "message")} from the Outbox…`);
  for (const id of ids) await call("retry_outbox", { id });
  await refreshOutbox();
  const failed = failedSends().filter((q) => ids.includes(q.id)).length;
  const waiting = waitingSends().filter((q) => ids.includes(q.id)).length;
  setStatus(
    failed + waiting === 0
      ? `sent ${plural(ids.length, "message")} from the Outbox`
      : [failed ? `${failed} not sent` : "", waiting ? `${waiting} still waiting for the server` : ""]
          .filter(Boolean)
          .join(" · "),
  );
}

/** Open a queued message in a compose window. It leaves the Outbox once the window has saved or sent it. */
async function editQueued(id: number): Promise<void> {
  const { account, draft } = await call<{ account: string; draft: DraftTemplate }>("edit_outbox", { id });
  await openCompose(account, fromTemplate(draft), undefined, id);
}

async function deleteQueued(id: number): Promise<void> {
  const q = state.outbox.find((x) => x.id === id);
  const what = q?.subject ? `“${q.subject}”` : "this message";
  if (!(await askFirst(`Delete ${what} unsent?\n\nIt is not in Drafts, and cannot be brought back.`, "Delete"))) return;
  await call("discard_outbox", { id });
  await refreshOutbox();
  setStatus(`deleted ${what} unsent`);
}

el.reader.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLElement>("[data-outbox]");
  if (!button) return;
  const id = Number(button.dataset.id);
  const work = {
    retry: () => retryOutbox([id]),
    "retry-all": () => retryOutbox(),
    edit: () => editQueued(id),
    delete: () => deleteQueued(id),
  }[button.dataset.outbox ?? ""];
  void work?.().catch(() => {
    // `call` has already said why.
  });
});
