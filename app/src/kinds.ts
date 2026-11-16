/**
 * The filter in Today narrows every kind it shows: the mail and the reading
 * by the search language, the tasks by the task filter, the threads by what
 * their rows show. `is:mail`, `is:task`, `is:chat` and `is:reading` keep one
 * kind, or several, and are taken off the text before the rest is read, since
 * the search language knows nothing of them.
 *
 * On its own, with nothing but the state under it, so that the chat can ask
 * without loading the views.
 */

import { state } from "./state";

export type TodayKind = "mail" | "task" | "chat" | "reading";

const KIND = /^is:(mail|tasks?|chat|reading)$/i;

/** The kinds a filter's text names, if any, and the text without them. */
export function splitKinds(text: string): { kinds: Set<TodayKind> | null; rest: string } {
  const kinds = new Set<TodayKind>();
  const rest: string[] = [];
  for (const word of text.split(/\s+/).filter(Boolean)) {
    const named = KIND.exec(word)?.[1]?.toLowerCase();
    if (named) kinds.add(named.startsWith("task") ? "task" : (named as TodayKind));
    else rest.push(word);
  }
  return { kinds: kinds.size > 0 ? kinds : null, rest: rest.join(" ") };
}

/** Today's filter, when one narrows Today: the kinds it keeps and the rest of its text. */
export function todayNarrowing(): { kinds: Set<TodayKind> | null; rest: string } | null {
  const filter = state.filter;
  if (!filter || filter.key !== "tasks\u0000today" || !filter.text.trim()) return null;
  return splitKinds(filter.text);
}

/** Whether Today's filter keeps a kind at all. */
export function keepsKind(kind: TodayKind): boolean {
  const kinds = todayNarrowing()?.kinds;
  return !kinds || kinds.has(kind);
}
