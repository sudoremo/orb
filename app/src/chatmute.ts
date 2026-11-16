/**
 * Muted rooms (chat-spec §13): a muted room's threads leave Waiting, Today and
 * the notifications, until the mute ends or one of them names the user or
 * calls the room. The room's own row in the sidebar still counts them.
 */
import { call, setStatus } from "./bridge";
import { ChatRoom, chatTargets, muteSpan, refreshChat, roomName } from "./chat";
import { state } from "./state";

/** The durations the menus and the picker offer, by the argument that asks for each. */
const DURATIONS: [string, string][] = [
  ["1h", "For 1 hour"],
  ["4h", "For 4 hours"],
  ["today", "Until tomorrow"],
  ["forever", "Until I unmute"],
];

/**
 * When a mute asked for by `spec` ends, in ms: `null` for one the user ends,
 * `undefined` for a spec that says nothing. `90m`, `4h` and `2d` count from
 * now, `today` runs to midnight, and `17:00` to the next time the clock says so.
 */
export function muteEnds(spec: string, now = new Date()): number | null | undefined {
  const text = spec.trim().toLowerCase();
  if (text === "forever" || text === "indefinitely") return null;
  if (text === "today") {
    const midnight = new Date(now);
    midnight.setHours(24, 0, 0, 0);
    return midnight.getTime();
  }
  const span = /^(\d+)\s*(m|min|h|d)$/.exec(text);
  if (span) {
    const unit = { m: 60_000, min: 60_000, h: 3_600_000, d: 86_400_000 }[span[2] as "m" | "min" | "h" | "d"];
    const n = Number(span[1]);
    return n > 0 ? now.getTime() + n * unit : undefined;
  }
  const clock = /^(\d{1,2}):(\d{2})$/.exec(text);
  if (clock) {
    const [h, m] = [Number(clock[1]), Number(clock[2])];
    if (h > 23 || m > 59) return undefined;
    const ends = new Date(now);
    ends.setHours(h, m, 0, 0);
    if (ends <= now) ends.setDate(ends.getDate() + 1);
    return ends.getTime();
  }
  return undefined;
}

/**
 * The rooms a mute acts on: the room under the sidebar's cursor while the
 * sidebar has the keyboard, the open room, or the rooms of the threads the
 * chat list acts on.
 */
async function targetRooms(): Promise<ChatRoom[]> {
  let rids: string[] = [];
  if (state.focus === "folders") {
    const entry = (await import("./sidebar")).currentEntry();
    if (entry?.kind === "chat-room") rids = [entry.rid];
  } else if (state.chat.view === "room" && state.chat.room) {
    rids = [state.chat.room];
  } else {
    rids = [...new Set(chatTargets().map((t) => t.rid))];
  }
  return rids.map((rid) => state.chat.rooms.find((r) => r.rid === rid)).filter((r): r is ChatRoom => !!r);
}

const named = (rooms: ChatRoom[]): string =>
  rooms.length === 1 ? roomName(rooms[0]!) : `${rooms.length} rooms`;

/** Mute the rooms for what `args` asks, or for what the picker is told; `off` unmutes them. */
export async function muteRooms(args: string[]): Promise<void> {
  const rooms = await targetRooms();
  if (rooms.length === 0) {
    setStatus("no chat room here to mute");
    return;
  }
  let spec = args.join(" ");
  if (!spec) {
    const { openPicker } = await import("./picker");
    const muted = rooms.filter((r) => r.muted);
    const picked = await openPicker({
      title: `Mute ${named(rooms)}`,
      placeholder: "for 1h, 90m, 2d, until 17:00",
      items: [
        ...(muted.length ? [{ value: "off", label: "Unmute", detail: muted.length === 1 ? muteSpan(muted[0]!) : "" }] : []),
        ...DURATIONS.map(([value, label]) => ({ value, label })),
      ],
      typed: (text) =>
        muteEnds(text) === undefined ? `“${text}”: say 90m, 4h, 2d or 17:00` : `${text.includes(":") ? "Until" : "For"} ${text}`,
    });
    if (!picked) return;
    spec = picked;
  }
  if (spec === "off") return unmute(rooms);
  const ends = muteEnds(spec);
  if (ends === undefined) {
    setStatus(`cannot tell how long “${spec}” is: say 1h, 90m, 2d, today, 17:00 or forever`);
    return;
  }
  const { groupUndo, offerUndo } = await import("./moving");
  const end = await groupUndo();
  try {
    for (const room of rooms) await call("chat_mute", { rid: room.rid, ends, undo: `mute ${roomName(room)}` }).catch(() => null);
  } finally {
    await end(`mute ${named(rooms)}`);
  }
  await refreshChat();
  const now = state.chat.rooms.find((r) => r.rid === rooms[0]!.rid);
  offerUndo(`Muted ${named(rooms)} ${now ? muteSpan(now) : ""}`.trim());
}

/** Unmute the rooms a mute would act on. */
export async function unmuteRooms(): Promise<void> {
  const rooms = (await targetRooms()).filter((r) => r.muted);
  if (rooms.length === 0) {
    setStatus("no muted chat room here");
    return;
  }
  await unmute(rooms);
}

async function unmute(rooms: ChatRoom[]): Promise<void> {
  const { groupUndo, offerUndo } = await import("./moving");
  const end = await groupUndo();
  try {
    for (const room of rooms) await call("chat_unmute", { rid: room.rid, undo: `unmute ${roomName(room)}` }).catch(() => null);
  } finally {
    await end(`unmute ${named(rooms)}`);
  }
  await refreshChat();
  offerUndo(`Unmuted ${named(rooms)}`);
}

/** The muted rooms, in a picker: choosing one unmutes it. */
export async function pickMutedRoom(): Promise<void> {
  const muted = state.chat.rooms.filter((r) => r.muted);
  if (muted.length === 0) {
    setStatus("no chat room is muted");
    return;
  }
  const { openPicker } = await import("./picker");
  const picked = await openPicker({
    title: "Muted chat rooms — choose one to unmute it",
    placeholder: "room or person",
    items: muted.map((r) => ({ value: r.rid, label: roomName(r), detail: muteSpan(r) })),
  });
  const room = muted.find((r) => r.rid === picked);
  if (room) await unmute([room]);
}

let unmuteTimer: number | undefined;
/** Draw Waiting again when the next mute ends, which nothing in the database announces. */
export function followMutes(): void {
  window.clearTimeout(unmuteTimer);
  const now = Date.now();
  const next = Math.min(...state.chat.rooms.map((r) => (r.muted && r.muted_until != null ? r.muted_until : Infinity)));
  if (!Number.isFinite(next)) return;
  // A timer holds at most about 24.8 days; a longer mute is looked at again then.
  unmuteTimer = window.setTimeout(() => void refreshChat(), Math.min(Math.max(next - now, 0) + 500, 2 ** 31 - 1));
}
