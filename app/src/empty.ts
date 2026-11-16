import { chatEmptyView, inChatView } from "./chat";
import { currentFolder } from "./actions";
import { dispatchCommand } from "./commands";
import { escape } from "./format";
import { IconName, roleIcon, svgIcon } from "./icons";
import { spellKey } from "./keys";
import { el, state, todayHasMail } from "./state";
import { shownSections, shows } from "./today";
import { TaskViewName, isPaged, taskViewLabel } from "./tasklist";
import { todayAccount } from "./taskview";

/**
 * What a list says when it has nothing in it: what is empty, a line of facts,
 * and the keys that lead on. It stands alone in the window — the reader pane
 * goes, because there is nothing it could show.
 */
interface EmptyView {
  icon: IconName;
  title: string;
  /** A folder's name before the title: data, which keeps its case. */
  name?: string;
  facts?: string;
  /** Commands with their labels, in the order they are offered. */
  leads: [command: string, label: string][];
}

/** The key the tables bind to a command, as the user would press it. */
function keyOf(table: Record<string, string>, command: string): string | null {
  return Object.entries(table).find(([, spec]) => spec.trim() === command)?.[0] ?? null;
}

function markup(view: EmptyView, table: Record<string, string>): string {
  const leads = view.leads
    .map(([command, label]) => {
      const key = keyOf(table, command);
      return `<button type="button" data-empty-command="${escape(command)}">${
        key ? `<kbd>${escape(spellKey(key))}</kbd>` : ""
      }<span class="lbl">${escape(label)}</span></button>`;
    })
    .join("");
  return `<div class="empty-view">${svgIcon(view.icon, "big")}
      <h3>${view.name ? `${escape(view.name)} ` : ""}<span class="lbl">${escape(view.title)}</span></h3>
      ${view.facts ? `<p>${escape(view.facts)}</p>` : ""}
      <div class="leads">${leads}</div>
    </div>`;
}

const TASK_VIEWS: Record<TaskViewName, Omit<EmptyView, "leads">> = {
  today: { icon: "sun", title: "Nothing left for today" },
  upcoming: { icon: "cal", title: "Nothing scheduled", facts: "No open task has a date ahead" },
  someday: { icon: "cloud", title: "No undated tasks", facts: "Every open task has a date" },
  completed: { icon: "done", title: "Nothing completed yet" },
  archive: { icon: "archive", title: "Nothing archived yet", facts: "Tasks completed long enough ago land here" },
  trash: { icon: "trash", title: "The trash is empty" },
};

/**
 * The list's empty view, or `null` while it has something to show.
 *
 * Today is empty only when both halves are, and only while it shows every
 * inbox: narrowed to one account, the switch that widens it again has to stay.
 */
export function emptyView(): string | null {
  const view = state.tasks.view;
  const taskTable = { ...state.bindings.index_normal, ...state.bindings.task_normal };

  if (inChatView()) {
    const chat = chatEmptyView();
    return chat && markup(chat, { ...state.bindings.index_normal, ...state.bindings.chat_normal });
  }

  // Today's threads and reading items, which a filter may have left alone.
  const others =
    view === "today" && todayHasMail()
      ? (shows("chat") ? state.chat.rows.length : 0) + (shows("reading") ? state.reading.rows.length : 0)
      : 0;

  // Narrowed to nothing: the filter is what to change, not the view.
  const filtered = state.filter?.text.trim();
  if (filtered && state.total === 0 && others === 0 && (!view || state.tasks.rows.length === 0)) {
    return markup(
      {
        icon: "filter",
        title: "Nothing matches",
        facts: `Nothing here matches “${filtered}”`,
        leads: [["filter-mode", "Change the filter"], ["clear-filter", "Clear it"]],
      },
      taskTable,
    );
  }

  if (view === "today" && !todayHasMail()) {
    if (state.tasks.rows.length > 0) return null;
    return markup({ ...TASK_VIEWS.today, facts: "No task is due", leads: [["create-task", "New task"]] }, taskTable);
  }
  if (view === "today") {
    // Only Today showing just its mail and its tasks, both empty, stands
    // alone; otherwise each section says for itself that it is empty.
    if (state.tasks.rows.length > 0 || state.total > 0 || others > 0 || todayAccount() !== null) return null;
    if (shownSections().join() !== "mail,tasks") return null;
    return markup(
      { ...TASK_VIEWS.today, facts: "No task is due · Every inbox is empty", leads: [["create-task", "New task"], ["compose", "Compose"]] },
      taskTable,
    );
  }
  if (view === "search") {
    if (state.tasks.rows.length > 0 || state.total > 0 || state.search?.tasks.length) return null;
  } else if (view) {
    if (state.tasks.rows.length > 0) return null;
    if (state.tasks.search.trim()) {
      return markup(
        {
          icon: "search",
          title: "No task matches",
          facts: `Nothing in ${taskViewLabel(view)} matches “${state.tasks.search.trim()}”`,
          leads: [["clear-search", "Clear the filter"]],
        },
        taskTable,
      );
    }
    const leads: EmptyView["leads"] = isPaged(view) || view === "trash" ? [] : [["create-task", "New task"]];
    return markup({ ...TASK_VIEWS[view], leads }, taskTable);
  }

  if (state.total > 0) return null;
  const mail = state.bindings.index_normal;
  if (state.search) {
    return markup(
      {
        icon: "search",
        title: "Nothing matches",
        facts: `No message matches “${state.search.query}”`,
        leads: [["search-mode", "Search again"], ["clear-search", "Leave the search"]],
      },
      mail,
    );
  }
  const folder = currentFolder();
  const name = folder ? (folder.label ?? folder.display_name) : "This folder";
  return markup(
    {
      icon: roleIcon(folder?.role),
      name,
      title: "is empty",
      facts: folder ? folder.account_name : undefined,
      leads: [["compose", "Compose"], ["sync", "Check for mail"]],
    },
    mail,
  );
}

/** Whether the list stands alone: empty, with no panel open in the reader. */
export function listStandsAlone(): boolean {
  return !state.info && emptyView() !== null;
}

el.list.addEventListener("click", (e) => {
  const lead = (e.target as HTMLElement).closest<HTMLElement>("[data-empty-command]");
  if (lead) void dispatchCommand(lead.dataset.emptyCommand!, 1);
});
