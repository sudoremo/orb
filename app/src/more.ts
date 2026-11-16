/**
 * The scroll regions that fade out at the bottom while there is more below,
 * rather than ending on a row's edge as though that were the end.
 */
const REGIONS = ["#list", ".mail-scroll", ".tk-list", "#reader pre", ".tk-detail-body"].join(",");

/** Mark each region that has more below its fold with `more`. */
export function refreshMore(): void {
  for (const region of document.querySelectorAll<HTMLElement>(REGIONS)) {
    const more = region.scrollTop + region.clientHeight < region.scrollHeight - 1;
    if (region.classList.contains("more") !== more) region.classList.toggle("more", more);
  }
}

let pending = false;
function schedule(): void {
  if (pending) return;
  pending = true;
  requestAnimationFrame(() => {
    pending = false;
    refreshMore();
  });
}
// Scroll events do not bubble; a capturing listener sees every region's.
document.addEventListener("scroll", schedule, { passive: true, capture: true });
window.addEventListener("resize", schedule);
// Content arrives after the render that made room for it — a message body,
// a page of rows — so a change anywhere is looked at too, once a frame.
new MutationObserver(schedule).observe(document.body, { childList: true, subtree: true });
