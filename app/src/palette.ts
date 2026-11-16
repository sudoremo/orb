/**
 * The command palette: one box for everything, as VS Code's Quick Open.
 *
 * What the field starts with chooses the list — nothing for the places the
 * sidebar can open, `>` for the commands, `#` for the tags, `@` for people
 * and `/` for mail and tasks found by the search. `go-to` opens it on the
 * places and `command-palette` on the commands; the prefix is typed or
 * deleted to move between them.
 *
 * The commands are those of `commanddocs.ts` with a title. Those that cannot
 * apply in this window are left out; those that cannot apply to what the
 * cursor is on now stay, greyed out, saying why. What they would act on is
 * noted as the palette opens and checked again before running, so a sync
 * that moved the cursor meanwhile cannot redirect the command.
 */

import { currentFolder } from "./actions";
import { call, setStatus } from "./bridge";
import { CHAT_COMMANDS, chatActive } from "./chat";
import { COMMAND_DOCS, COMMAND_GROUP, COMMAND_GROUPS, COMMAND_VARIANTS } from "./commanddocs";
import { TASK_COMMANDS, activeBindings, expandSpec, runCommand } from "./commands";
import { showMessage } from "./jump";
import { keysOf } from "./keyhints";
import { spellKey } from "./keys";
import { MenuState, messageMenuState, targetRows, taskMenuState } from "./menus";
import { SearchHit } from "./model";
import { runSearchQuery } from "./moving";
import { PickerItem, PickerMode, openPicker } from "./picker";
import { openPlace, placeItems, placesPlaceholder } from "./sidebar";
import { WINDOW, state, todayHasMail } from "./state";
import { TaskSummary } from "./tasklist";
import { openTaskById, targetTaskIds } from "./taskview";

/** A command as the palette lists it: a command, a command with its argument, or one of the user's own. */
interface Entry {
  spec: string;
  /** The built-in command it runs first, which says whether it applies here. */
  name: string;
  title: string;
  group: string;
  keywords: string;
}

/** Every command and variant with a title, in the order of the `?` overlay. */
function entries(): Entry[] {
  const out: Entry[] = [];
  for (const [group, docs] of Object.entries(COMMAND_GROUPS)) {
    for (const [name, doc] of Object.entries(docs)) {
      if (doc.title) out.push({ spec: name, name, title: doc.title, group, keywords: doc.keywords ?? "" });
    }
  }
  for (const [spec, variant] of Object.entries(COMMAND_VARIANTS)) {
    const name = spec.split(" ")[0]!;
    out.push({ spec, name, title: variant.title, group: COMMAND_GROUP[name] ?? "", keywords: variant.keywords ?? "" });
  }
  for (const [spec, own] of Object.entries(state.userCommands)) {
    const name = expandSpec(spec)?.[0]?.split(/\s+/)[0];
    if (name) out.push({ spec, name, title: own.title ?? spec, group: "Your commands", keywords: `${spec} ${own.run}` });
  }
  return out;
}

/** What the commands would act on, noted as the palette opens. */
interface Targets {
  mail: number[];
  tasks: number[];
}
function targetsNow(): Targets {
  return { mail: targetRows().map((m) => m.id), tasks: targetTaskIds() };
}
const same = (a: number[], b: number[]) => a.length === b.length && a.every((id, i) => id === b[i]);

/** Whether the command has nothing to do in this window, and is left out of the palette. */
function hidden(name: string): boolean {
  const doc = COMMAND_DOCS[name];
  if (!state.tasks.available && name in TASK_COMMANDS) return true;
  if (!state.chat.available && name in CHAT_COMMANDS) return true;
  if (WINDOW === "chat" && doc?.chatWindow === false) return true;
  if (WINDOW === "tasks" && doc?.on === "mail") return true;
  const sidebar = name === "go-to" || name === "next-folder" || name === "prev-folder" || name === "toggle-pin";
  return (WINDOW === "message" || WINDOW === "task") && sidebar;
}

/** What the palette was opened over, read once per opening. */
interface Context {
  targets: Targets;
  mail: MenuState;
  tasks: MenuState;
  chat: boolean;
}

/** Why a command cannot run on what the cursor is on, or undefined when it can. */
function why(name: string, context: Context): string | undefined {
  const on = COMMAND_DOCS[name]?.on;
  if (on === "mail") {
    if (context.chat) return "acts on mail; the cursor is in chat";
    if (context.targets.mail.length === 0) return "no message under the cursor";
    return context.mail.why[name];
  }
  if (on === "task") {
    if (context.targets.tasks.length === 0) return "no task under the cursor";
    return context.tasks.why[name];
  }
  return context.mail.why[name];
}

/** The commands run from the palette, newest first, kept on this Mac. */
const RECENT_KEY = "orb.palette.recent";
const MAX_RECENT = 12;
const RECENT = "recently used";
function recent(): string[] {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]");
    return Array.isArray(stored) ? stored.filter((s): s is string => typeof s === "string") : [];
  } catch {
    return [];
  }
}
function noteRecent(spec: string): void {
  localStorage.setItem(RECENT_KEY, JSON.stringify([spec, ...recent().filter((s) => s !== spec)].slice(0, MAX_RECENT)));
}

/** The shortest key bound to exactly this spec in the bindings in force, spelled for the eye. */
function keyOf(spec: string, table: Record<string, string>): string | undefined {
  const key = Object.entries(table)
    .filter(([, bound]) => bound.trim() === spec)
    .map(([key]) => key)
    .sort((a, b) => keysOf(a).length - keysOf(b).length || a.length - b.length)[0];
  return key && keysOf(key).map(spellKey).join("");
}

function commandItems(context: Context): PickerItem[] {
  const table = activeBindings();
  const used = recent();
  const items = entries()
    .filter((entry) => !hidden(entry.name))
    .map((entry): PickerItem => {
      // A toggle says which way it goes now, as its menu entry does, and is
      // found by either name.
      const label = entry.spec === entry.name ? (context.mail.labels[entry.name] ?? context.tasks.labels[entry.name]) : undefined;
      const at = used.indexOf(entry.spec);
      return {
        value: `>${entry.spec}`,
        label: label ?? entry.title,
        detail: entry.group,
        keywords: [label ? entry.title : "", entry.keywords, entry.spec].filter(Boolean).join(" "),
        keys: keyOf(entry.spec, table),
        disabled: why(entry.name, context),
        section: at >= 0 ? RECENT : entry.group,
        boost: at >= 0 ? (MAX_RECENT - at) / MAX_RECENT : 0,
      };
    });
  return [
    ...items.filter((i) => i.section === RECENT).sort((a, b) => b.boost! - a.boost!),
    ...items.filter((i) => i.section !== RECENT),
  ];
}

/** The tags, for `#`. */
function tagItems(): PickerItem[] {
  return [...state.tasks.tags]
    .filter((tag) => tag.is_known !== false)
    .sort((a, b) => a.position - b.position || a.name.localeCompare(b.name))
    .map((tag) => ({ value: `#${tag.name}`, label: tag.name, detail: "Tag" }));
}

/** People from the address book, for `@`. */
async function people(query: string): Promise<PickerItem[]> {
  if (!query) return [];
  const found = await call<{ name: string | null; recipients: { name: string | null; address: string }[] }[]>(
    "suggest_recipients",
    { query, limit: 8 },
  ).catch(() => []);
  return found.flatMap((person) => {
    const r = person.recipients[0];
    if (!r) return [];
    return [{ value: `@${r.address}`, label: r.name ?? person.name ?? r.address, detail: r.address, keywords: r.address }];
  });
}

/** At most this many messages and tasks under `/`. */
const MAX_FOUND = 20;

/** Mail and tasks found by the search, for `/`: the mail first, the tasks given half the rows if they need them. */
async function found(query: string): Promise<PickerItem[]> {
  if (!query) return [];
  const [hits, tasks] = await Promise.all([
    todayHasMail()
      ? call<SearchHit[]>("search", { query, scope: "All", sort: null }).catch(() => [] as SearchHit[])
      : Promise.resolve([] as SearchHit[]),
    state.tasks.available
      ? call<TaskSummary[]>("search_tasks", { query }).catch(() => [] as TaskSummary[])
      : Promise.resolve([] as TaskSummary[]),
  ]);
  const mail = hits.slice(0, MAX_FOUND - Math.min(tasks.length, MAX_FOUND / 2));
  return [
    ...mail.map((h) => ({
      value: `/message ${h.message.id} ${h.account_name}\u0000${h.folder_name}`,
      label: h.message.subject || "(no subject)",
      detail: `${(h.message.from_display ?? "").replace(/\s*<.*>$/, "")} · ${h.folder_name}`,
    })),
    ...tasks.slice(0, MAX_FOUND - mail.length).map((t) => ({
      value: `/task ${t.id}`,
      label: t.title,
      detail: t.is_completed ? "Task, completed" : "Task",
    })),
  ];
}

function modes(context: Context): PickerMode[] {
  const list: PickerMode[] = [
    { prefix: "", title: "Go to", placeholder: `${placesPlaceholder()} — or > for a command`, items: placeItems },
    { prefix: ">", title: "Commands", placeholder: "a command", items: () => commandItems(context) },
  ];
  if (state.tasks.available) list.push({ prefix: "#", title: "Tags", placeholder: "a tag", items: tagItems });
  if (todayHasMail()) list.push({ prefix: "@", title: "People", placeholder: "a name or an address", items: () => [], search: people });
  if (WINDOW === "main" || WINDOW === "tasks") {
    list.push({ prefix: "/", title: "Mail and tasks", placeholder: "words, as the search takes them", items: () => [], search: found });
  }
  return list;
}

/** Ask for a word: what is typed is the answer. */
function ask(title: string, placeholder: string): Promise<string | null> {
  return openPicker({ title, placeholder, items: [], typed: (text) => `“${text}”` });
}
/** Pick one of the tags. */
function askTag(title: string): Promise<string | null> {
  return openPicker({ title, placeholder: "a tag", items: tagItems().map((t) => ({ ...t, value: t.label! })) });
}

/**
 * The arguments a command asks for when the palette runs it without them, one
 * step after another; a step answered with nothing cancels. A command not
 * here takes none, or asks for its own — `mark-move` its folder, `add-tag` its
 * tag.
 */
const ASKS: Record<string, (() => Promise<string | null>)[]> = {
  search: [() => ask("Search", "f:anna s:invoice")],
  "today-mail": [
    () =>
      openPicker({
        title: "Today shows",
        placeholder: "an account",
        items: [{ value: "all", label: "Every inbox" }, ...state.accounts.map((a) => ({ value: a.name, label: a.name }))],
      }),
  ],
  "create-folder": [() => ask("New folder", "its name")],
  "rename-folder": [() => ask(`Rename ${currentFolder()?.display_name ?? "the folder"} to`, "its new name")],
  "remove-tag": [() => askTag("Remove the tag")],
  "delete-tag": [() => askTag("Delete the tag from every task")],
  "rename-tag": [() => askTag("Rename the tag"), () => ask("Rename to", "its new name")],
  "recolour-tag": [() => askTag("Recolour the tag"), () => ask("Colour", "#e06060")],
  "chat-create-room": [() => ask("New chat room", "its name")],
};

/** Run a command the palette picked, asking for its arguments first. */
async function runPicked(spec: string, count: number, opened: Targets): Promise<void> {
  const name = spec.split(" ")[0]!;
  const first = expandSpec(spec)?.[0]?.split(/\s+/)[0] ?? name;
  const on = COMMAND_DOCS[first]?.on;
  const now = targetsNow();
  if ((on === "mail" && !same(opened.mail, now.mail)) || (on === "task" && !same(opened.tasks, now.tasks))) {
    setStatus(`${name}: what the cursor was on changed while the palette was open; nothing done`);
    return;
  }
  const words: string[] = [];
  if (spec === name) {
    for (const step of ASKS[name] ?? []) {
      const answer = (await step())?.trim();
      if (!answer) return;
      words.push(answer);
    }
  }
  noteRecent(spec);
  // Inside the turn of the command that opened the palette: queued, it would wait for itself.
  await runCommand([spec, ...words].join(" "), count);
}

/** Open what a row of the palette stands for. */
async function openPicked(value: string, count: number, opened: Targets): Promise<void> {
  const rest = value.slice(1);
  switch (value[0]) {
    case ">":
      return runPicked(rest, count, opened);
    case "#":
      if (WINDOW === "tasks" && (await openPlace(`tag:${rest}`))) return;
      return runSearchQuery(`tag:${rest}`, `tag:${rest}`, undefined, undefined, "tasks");
    case "@":
      return runSearchQuery(`f:${rest} OR t:${rest}`, rest, undefined, undefined, "mail");
    case "/": {
      const [kind, id, where] = rest.split(" ");
      if (kind === "task") {
        await openTaskById(Number(id));
        return;
      }
      const [account, folder] = (where ?? "").split("\u0000");
      return showMessage(Number(id), account ?? null, folder ?? null);
    }
    default:
      if (!(await openPlace(value))) setStatus("that place is gone from the sidebar");
  }
}

/**
 * Open the palette on a prefix: `>` the commands, nothing the places. `count`
 * goes to the command picked, as a count typed before a key does.
 */
export async function openPalette(prefix: string, count = 1): Promise<void> {
  const context: Context = {
    targets: targetsNow(),
    mail: messageMenuState(),
    tasks: taskMenuState(),
    chat: chatActive(),
  };
  const all = modes(context);
  const picked = await openPicker({ title: all[0]!.title, items: [], modes: all, initial: prefix });
  if (picked !== null) await openPicked(picked, count, context.targets);
}
