import { call, setStatus } from "./bridge";
import { escape, icon, keyValue, parseFrom } from "./format";
import { render } from "./list";
import { FilterActivity, PendingActionInfo } from "./model";
import { applyPending, offerUndo } from "./moving";
import { describeHealth, healthGlyph, refreshPendingCounts, renderFolders } from "./sidebar";
import { el, state } from "./state";
import { refreshList, settleWrites } from "./targets";

/** Show a built panel in the reader and give it the keyboard. */
export function showPanel(title: string, html: string): void {
  state.info = { title, html };
  state.focus = "reader";
  render();
}
/** What the filter rules did in each account's last run, as panel sections. */
function filterActivitySections(activity: FilterActivity[]): string {
  if (activity.length === 0) return "";
  const groups = new Map<string, FilterActivity[]>();
  for (const a of activity) {
    groups.set(a.account, [...(groups.get(a.account) ?? []), a]);
  }
  const sections = [...groups.entries()]
    .map(([account, list]) => {
      const at = list[0]!.at.slice(0, 16).replace("T", " ");
      const rows = list
        .map((a) => {
          const who = a.from ? parseFrom(a.from).name || parseFrom(a.from).email : "";
          const failed = a.actions.includes("(failed:");
          return `<tr${failed ? ' class="failed"' : ""}><td class="does">${escape(a.rule)}</td><td>${escape(a.subject ?? "(no subject)")}</td><td class="dim">${escape(who)}</td><td class="dim">${escape(a.folder)}: ${escape(a.actions)}</td></tr>`;
        })
        .join("");
      return `<section class="pending-group filtered">
        <h2>${icon("fa-filter", "fa-fw")} ${escape(account)} <span class="dim">filtered ${escape(at)}</span></h2>
        <table>${rows}</table>
      </section>`;
    })
    .join("");
  return `<h2 class="panel-section">What the filters did in the last sync</h2>${sections}`;
}
/**
 * The staged actions, grouped by folder, with what each would do; and below
 * them, what the filter rules did on their own.
 */
export async function showPendingReview(): Promise<void> {
  await settleWrites();
  const actions = await call<PendingActionInfo[]>("pending_actions");
  const filtered = filterActivitySections(await call<FilterActivity[]>("filter_activity"));
  if (actions.length === 0) {
    showPanel("Staged actions", `<div class="empty">Nothing is staged.</div>${filtered}`);
    return;
  }
  const groups = new Map<string, PendingActionInfo[]>();
  for (const a of actions) {
    const key = `${a.account_name}\u0000${a.folder_name}`;
    groups.set(key, [...(groups.get(key) ?? []), a]);
  }
  const sections = [...groups.entries()]
    .map(([key, list]) => {
      const [account, folder] = key.split("\u0000") as [string, string];
      const rows = list
        .map((a) => {
          const what =
            a.action_type === "Delete"
              ? `${icon("fa-trash-can", "fa-fw")} delete`
              : `${icon("fa-right-from-bracket", "fa-fw")} move to ${escape(a.target_folder ?? "?")}`;
          const who = parseFrom(a.from_display).name || parseFrom(a.from_display).email;
          const subject = a.subject === null ? `UID ${a.uid}` : a.subject || "(no subject)";
          return `<tr><td class="what">${what}</td><td>${escape(subject)}</td><td class="dim">${escape(who)}</td></tr>`;
        })
        .join("");
      return `<section class="pending-group">
        <h2>${escape(account)} / ${escape(folder)} <span class="dim">${list.length}</span>
          <button type="button" data-pending="apply" data-account="${escape(account)}" data-folder="${escape(folder)}">Apply</button>
          <button type="button" data-pending="discard" data-account="${escape(account)}" data-folder="${escape(folder)}">Discard</button>
        </h2>
        <table>${rows}</table>
      </section>`;
    })
    .join("");
  const total = actions.length;
  showPanel(
    "Staged actions",
    `<div class="panel-actions">${total} staged
       <button type="button" data-action="expunge-all">Apply all</button>
       <button type="button" data-action="discard-pending all">Discard all</button>
     </div>${sections}${filtered}`,
  );
}
el.reader.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLElement>("[data-pending]");
  if (!button) return;
  const account = button.dataset.account ?? "";
  const folder = button.dataset.folder ?? "";
  const work =
    button.dataset.pending === "apply"
      ? applyPending([{ account, folder }], `${account}/${folder}`)
      : settleWrites()
          .then(() => call<number>("discard_pending", { account, folder, undo: "discard staged actions" }))
          .then(async (dropped) => {
            await refreshPendingCounts();
            await refreshList();
            renderFolders();
            await showPendingReview();
            const said = `discarded ${dropped} staged action${dropped === 1 ? "" : "s"}`;
            if (dropped > 0) offerUndo(said);
            else setStatus(said);
          });
  void work.catch(() => {
    // `call` has already said why.
  });
});
/** Each account's standing, with the last error in full. */
export function showAccountStatus(): void {
  const rows = state.health
    .map((h) => {
      const info = state.accounts.find((a) => a.name === h.account);
      const lastOk = h.last_ok ? h.last_ok.slice(0, 16).replace("T", " ") : "never this session";
      return `<section class="account-health ${h.state}">
        <h2>${healthGlyph(h.account) || icon("fa-circle-check", "health ok")} ${escape(h.account)}
          <span class="dim">${escape(info?.email ?? "")}</span></h2>
        ${keyValue("hdr", "State", escape(describeHealth(h)))}
        ${keyValue("hdr", "Last good sync", escape(lastOk))}
        ${h.tasks ? keyValue("hdr", "Tasks", escape(`not syncing: ${h.tasks}`)) : ""}
      </section>`;
    })
    .join("");
  showPanel("Accounts", rows || `<div class="empty">No accounts configured.</div>`);
}
/** Each feed of the reading list: when it was last asked, and what went wrong. */
export async function showFeedStatus(): Promise<void> {
  const feeds = await call<{ url: string; name: string | null; polled_at: number | null; last_error: string | null }[]>(
    "feed_statuses",
  );
  const rows = feeds
    .map((feed) => {
      const polled = feed.polled_at
        ? new Date(feed.polled_at * 1000).toISOString().slice(0, 16).replace("T", " ")
        : "not yet";
      return `<section class="account-health ${feed.last_error ? "error" : "ok"}">
        <h2>${feed.last_error ? icon("fa-triangle-exclamation", "health error") : icon("fa-circle-check", "health ok")} ${escape(feed.name ?? feed.url)}
          <span class="dim">${escape(feed.url)}</span></h2>
        ${keyValue("hdr", "Last asked", escape(polled))}
        ${feed.last_error ? keyValue("hdr", "Problem", escape(feed.last_error)) : ""}
      </section>`;
    })
    .join("");
  showPanel("Feeds", rows || `<div class="empty">No feeds: add them under reading.feeds in config.yml.</div>`);
}
