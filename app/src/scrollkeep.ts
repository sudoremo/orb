/**
 * Scroll positions carried across a redraw.
 *
 * A pane redrawn by replacing its markup gets new elements for the regions
 * inside it that scroll on their own, and a new element starts at the top.
 * Each redraw that replaces such a region records the positions before and
 * puts them back after, so that nothing short of going somewhere else scrolls
 * a list back to its start.
 */

/** The regions that scroll on their own, inside the list pane and the reader. */
const REGIONS = [".mail-scroll", ".tk-list", ".rd-list", ".ch-list", ".tk-detail-body"];

export type Scrolls = Map<string, number>;

/** Where each region inside `root` is scrolled to, by its selector. */
export function scrollsIn(root: ParentNode): Scrolls {
  const kept: Scrolls = new Map();
  for (const selector of REGIONS) {
    const region = root.querySelector<HTMLElement>(selector);
    if (region && region.scrollTop > 0) kept.set(selector, region.scrollTop);
  }
  return kept;
}

/** Scroll the regions now inside `root` to where `scrollsIn` found them. */
export function restoreScrolls(root: ParentNode, kept: Scrolls): void {
  for (const [selector, top] of kept) {
    const region = root.querySelector<HTMLElement>(selector);
    if (region && region.scrollTop !== top) region.scrollTop = top;
  }
}

/** Run a redraw of what is inside `root`, keeping its regions where they were. */
export function keepingScroll(root: ParentNode, redraw: () => void): void {
  const kept = scrollsIn(root);
  redraw();
  restoreScrolls(root, kept);
}
