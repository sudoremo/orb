import { WINDOW, el } from "./state";

/**
 * A line between two panes that can be dragged.
 *
 * Every split is one of these, run by the same listeners: the size it is
 * dragged to is measured in `frame`, kept on this Mac under `key`, and drawn
 * by `show`, which sets the CSS variables the layout reads. A drag mutates
 * those variables and nothing else — no re-render, and the panes keep their
 * scroll positions.
 */
interface Split {
  /** The handle: an element drawn once, or a selector for one drawn with the list. */
  handle: HTMLElement | string;
  axis: "x" | "y";
  /** Where the line can move, along `axis`, in client coordinates. */
  frame: () => { start: number; end: number };
  /** The size is of the pane after the line, measured from the frame's end. */
  fromEnd?: boolean;
  /** The least either side is dragged down to, in pixels. */
  min: number;
  /** What is kept for a size, in pixels, of a frame `extent` pixels long. */
  encode: (size: number, extent: number) => string;
  /** Draw what is kept; `null` draws the default. */
  show: (kept: string | null) => void;
  key: string;
  /** Whether a double-click goes back to the default. */
  resets: boolean;
  /** Whether the split is in force just now; another may share its handle. */
  active?: () => boolean;
}

const splits: Split[] = [];

/** Make a line draggable, showing what was kept for it. Earlier splits win a shared handle. */
function addSplit(split: Split): void {
  splits.push(split);
  split.show(readSetting(split.key));
}

/** The split whose handle `target` is on, and the handle. */
function splitAt(target: EventTarget | null): { split: Split; handle: HTMLElement } | null {
  if (!(target instanceof Element)) return null;
  for (const split of splits) {
    if (split.active && !split.active()) continue;
    const handle =
      typeof split.handle === "string"
        ? target.closest<HTMLElement>(split.handle)
        : split.handle.contains(target)
          ? split.handle
          : null;
    if (handle) return { split, handle };
  }
  return null;
}

// Capturing, so that a split sees its handle before anything the handle sits in.
document.addEventListener(
  "pointerdown",
  (e) => {
    const found = splitAt(e.target);
    if (!found) return;
    const { split, handle } = found;
    e.preventDefault();
    e.stopPropagation();
    const frame = split.frame();
    const extent = frame.end - frame.start;
    const cls = split.axis === "x" ? "resizing-col" : "resizing-row";
    let kept: string | null = null;
    const onMove = (ev: PointerEvent) => {
      const at = split.axis === "x" ? ev.clientX : ev.clientY;
      const offset = split.fromEnd ? frame.end - at : at - frame.start;
      const size = Math.min(Math.max(offset, split.min), extent - split.min);
      kept = split.encode(size, extent);
      split.show(kept);
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      document.body.classList.remove(cls);
      handle.classList.remove("dragging");
      if (kept !== null) writeSetting(split.key, kept);
    };
    document.body.classList.add(cls);
    handle.classList.add("dragging");
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  },
  true,
);
document.addEventListener(
  "dblclick",
  (e) => {
    const found = splitAt(e.target);
    if (!found?.split.resets) return;
    found.split.show(null);
    removeSetting(found.split.key);
  },
  true,
);

/** A variable on an element, set or removed. */
function setVariable(host: HTMLElement, property: string, value: string | null): void {
  if (value === null) host.style.removeProperty(property);
  else host.style.setProperty(property, value);
}
const px = (size: number) => `${Math.round(size)}px`;
const percent = (size: number, extent: number) => `${((size / extent) * 100).toFixed(2)}%`;
const across = (box: DOMRect, axis: "x" | "y") =>
  axis === "x" ? { start: box.left, end: box.right } : { start: box.top, end: box.bottom };

/**
 * The lines between the window's panes: the folders and the list, and the
 * list and the reader. In the tasks and chat windows, wide enough for the
 * reader to sit beside the list, the second is dragged sideways instead, and
 * a double-click gives the list its own share back.
 */
export function initPaneSplits(): void {
  const wide = window.matchMedia("(min-width: 900px)");
  if (WINDOW === "tasks" || WINDOW === "chat") {
    const chat = WINDOW === "chat";
    addSplit({
      handle: el.hsplit,
      axis: "x",
      active: () => wide.matches,
      frame: () => ({ start: el.list.getBoundingClientRect().left, end: el.app.getBoundingClientRect().right }),
      min: 240,
      encode: px,
      show: (kept) => setVariable(el.app, chat ? "--pane-chat-list" : "--pane-tasks-list", kept),
      key: chat ? ".chatList" : ".tasksList",
      resets: true,
    });
  }
  addSplit({
    handle: el.vsplit,
    axis: "x",
    frame: () => across(el.app.getBoundingClientRect(), "x"),
    min: 140,
    encode: px,
    show: (kept) => setVariable(el.app, "--pane-sidebar", kept),
    key: "--pane-sidebar",
    resets: false,
  });
  addSplit({
    handle: el.hsplit,
    axis: "y",
    // The list's track starts below the toolbar, not at the top of the grid.
    frame: () => ({ start: el.list.getBoundingClientRect().top, end: el.app.getBoundingClientRect().bottom }),
    min: 120,
    encode: px,
    show: (kept) => setVariable(el.app, "--pane-list", kept),
    key: "--pane-list",
    resets: false,
  });
}

/**
 * The lines inside Today: between the mail and the tasks, stacked or in
 * columns, and above the chat and the reading. Until one is dragged, the
 * section after it takes the room it needs, up to a ceiling; dragged, it keeps
 * the share of the pane it was given, so a bigger window grows every part, and
 * a double-click goes back to fitting it. The handles are redrawn with the
 * list; the variables sit in its style attribute, which a render leaves alone.
 */
export function initTodaySplits(relayout: () => void): void {
  const list = () => across(el.list.getBoundingClientRect(), "y");
  addSplit({
    handle: "[data-tk-split]",
    axis: "y",
    frame: list,
    fromEnd: true,
    min: 60,
    encode: percent,
    show: (kept) => {
      setVariable(el.list, "--today-tasks", kept);
      setVariable(el.list, "--today-tasks-max", kept === null ? null : "none");
    },
    key: ".todayTasks",
    resets: true,
  });
  addSplit({
    handle: "[data-rd-split]",
    axis: "y",
    frame: list,
    fromEnd: true,
    min: 60,
    encode: percent,
    show: (kept) => {
      setVariable(el.list, "--today-reading", kept);
      setVariable(el.list, "--today-reading-max", kept === null ? null : "none");
      relayout();
    },
    key: ".todayReadingHeight",
    resets: true,
  });
  // The chat has the reading section below it, so it is measured from its own
  // bottom, but kept as a share of the whole pane, as the others are.
  addSplit({
    handle: "[data-ch-split]",
    axis: "y",
    frame: () => {
      const pane = el.list.getBoundingClientRect();
      const section = el.list.querySelector(":scope > .ch-section")?.getBoundingClientRect();
      return { start: pane.top, end: section?.bottom ?? pane.bottom };
    },
    fromEnd: true,
    min: 60,
    encode: (size) => percent(size, el.list.getBoundingClientRect().height),
    show: (kept) => {
      setVariable(el.list, "--today-chat", kept);
      setVariable(el.list, "--today-chat-max", kept === null ? null : "none");
      relayout();
    },
    key: ".todayChatHeight",
    resets: true,
  });
  // The mail's share of the width, in percent; the columns are `fr` tracks.
  addSplit({
    handle: "[data-tk-vsplit]",
    axis: "x",
    frame: () => across(el.list.getBoundingClientRect(), "x"),
    min: 240,
    encode: (size, extent) => ((size / extent) * 100).toFixed(2),
    show: (kept) => {
      const share = Number(kept);
      const valid = kept !== null && share > 0 && share < 100;
      setVariable(el.list, "--today-mail-w", valid ? `${share.toFixed(2)}fr` : null);
      setVariable(el.list, "--today-tasks-w", valid ? `${(100 - share).toFixed(2)}fr` : null);
    },
    key: ".todayMailWidth",
    resets: true,
  });
}
export function removeSetting(key: string): void {
  try {
    localStorage.removeItem(`orb${key}`);
  } catch {
    // As for writeSetting.
  }
}
export function readSetting(key: string): string | null {
  try {
    return localStorage.getItem(`orb${key}`);
  } catch {
    return null;
  }
}
export function writeSetting(key: string, value: string): void {
  try {
    localStorage.setItem(`orb${key}`, value);
  } catch {
    // Layout persistence is a convenience; ignore quota/private-mode failures.
  }
}
