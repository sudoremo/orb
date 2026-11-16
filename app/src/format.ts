
/**
 * A date as the list shows it, by the configured strftime format. The
 * common directives, no locale: `%Y %y %m %d %e %H %I %p %M %S %b %B %a %A
 * %j %F %R %T`, with `-` for no padding, and `%%`. An unknown directive
 * stays as typed.
 */
export function formatDate(iso: string, format: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso.slice(0, 16);
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const LONG_MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
  const LONG_DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  const pad = (n: number, width = 2) => String(n).padStart(width, "0");
  return format.replace(/%(-?)([a-zA-Z%])/g, (whole, bare, letter) => {
    const number = (n: number, width = 2) => (bare ? String(n) : pad(n, width));
    switch (letter) {
      case "Y": return String(d.getFullYear());
      case "y": return pad(d.getFullYear() % 100);
      case "m": return number(d.getMonth() + 1);
      case "d": return number(d.getDate());
      case "e": return bare ? String(d.getDate()) : String(d.getDate()).padStart(2, " ");
      case "H": return number(d.getHours());
      case "I": return number(((d.getHours() + 11) % 12) + 1);
      case "p": return d.getHours() < 12 ? "AM" : "PM";
      case "M": return number(d.getMinutes());
      case "S": return number(d.getSeconds());
      case "b": case "h": return MONTHS[d.getMonth()]!;
      case "B": return LONG_MONTHS[d.getMonth()]!;
      case "a": return DAYS[d.getDay()]!;
      case "A": return LONG_DAYS[d.getDay()]!;
      case "j": {
        // Whole days between the civil dates, so a clock change is not a day.
        const day = Date.UTC(d.getFullYear(), d.getMonth(), d.getDate());
        const start = Date.UTC(d.getFullYear(), 0, 0);
        return number(Math.round((day - start) / 86_400_000), 3);
      }
      case "F": return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
      case "R": return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
      case "T": return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
      case "%": return "%";
      default: return whole;
    }
  });
}
/**
 * The forms `auto` shows a date in, the fullest first: the time today, the
 * day of the week this week, the day and month this year, and the year
 * always for another — what the column has no room for goes first, the
 * year never.
 */
export function autoDateForms(iso: string, now: Date = new Date()): string[] {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return [iso.slice(0, 16)];
  const midnight = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  // Rounded, so the day a clock change makes 23 or 25 hours long is a day.
  const daysAgo = Math.round((midnight(now) - midnight(d)) / 86_400_000);
  const forms =
    daysAgo === 0
      ? ["%H:%M"]
      : daysAgo === 1
        ? ["Yesterday %H:%M", "Yesterday", "%a %H:%M", "%a"]
        : daysAgo > 1 && daysAgo < 7
          ? ["%a %H:%M", "%a"]
          : d.getFullYear() === now.getFullYear()
            ? ["%-d %b %H:%M", "%-d %b"]
            : ["%-d %b %Y %H:%M", "%-d %b %Y", "%b %Y", "%Y"];
  return forms.map((form) => formatDate(iso, form));
}
let measure: CanvasRenderingContext2D | null = null;
/** The width of `text` in `font`, in pixels. */
function textWidth(text: string, font: string): number {
  measure ??= document.createElement("canvas").getContext("2d");
  if (!measure) return 0;
  measure.font = font;
  return measure.measureText(text).width;
}
/**
 * The room a date cell has: its width without its padding, and its font.
 * Read once per drawing of the list, never per row, since reading a width
 * makes the browser lay out what was just written.
 */
export function dateRoom(cell: HTMLElement | undefined): { width: number; font: string } | null {
  if (!cell || !cell.isConnected) return null;
  const style = getComputedStyle(cell);
  const width = cell.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
  return width > 0 ? { width, font: style.font } : null;
}
/**
 * A date as the list's date column shows it: by the configured strftime
 * format, or under `auto` the fullest form that fits `room` (the fullest of
 * all where the room is not known).
 */
export function listDate(iso: string, format: string, room: { width: number; font: string } | null = null, now: Date = new Date()): string {
  if (format !== "auto") return formatDate(iso, format);
  const forms = autoDateForms(iso, now);
  if (!room) return forms[0]!;
  return forms.find((form) => textWidth(form, room.font) <= room.width) ?? forms[forms.length - 1]!;
}
export const escape = (s: string): string =>
  s.replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!,
  );
/**
 * The opening of a labelled line: the line in `data-k`, the label as an
 * element of its own, so a look can lowercase the label, hide it or draw the
 * line as `Key value` from CSS alone. The caller adds the value and closes
 * the element.
 */
export const labelled = (cls: string, key: string): string =>
  `<div class="${cls}" data-k="${escape(key)}"><span class="k lbl">${escape(key)}</span>`;
/** A labelled line whose value is one piece of markup. */
export const keyValue = (cls: string, key: string, value: string): string =>
  `${labelled(cls, key)}<span class="v">${value}</span></div>`;
// Brand icons (`fa-github`, `fa-linux`) are only in the brands font; drawn
// with `fa-solid` they are an empty box.
const BRANDS = new Set(__FA_BRANDS__);
const REGULAR = new Set(__FA_REGULAR__);
/** A configured Font Awesome icon (`fa-gears`), in the font that has it. */
export const configIcon = (name: string, color: string | null = null, cls = ""): string =>
  `<i class="${BRANDS.has(name) ? "fa-brands" : "fa-solid"} ${escape(name)}${cls ? ` ${cls}` : ""}"${
    color ? ` style="color:${escape(color)}"` : ""
  }></i>`;
const knownIcons = new Map<string, boolean>();
/**
 * The icons a subject is drawn with, as markup, leaving out a name the
 * bundled Font Awesome set does not have rather than drawing an empty box.
 * `exists` is asked once per name.
 */
export function subjectIconsHtml(
  icons: { icon: string; color: string | null }[] | undefined,
  exists: (name: string) => boolean,
): string {
  if (!icons?.length) return "";
  return icons
    .filter(({ icon: name }) => {
      if (!knownIcons.has(name)) knownIcons.set(name, exists(name));
      return knownIcons.get(name);
    })
    .map(({ icon: name, color }) => configIcon(name, color, "subject-icon"))
    .join("");
}
/**
 * One of Orb's own icons: in the lighter regular style where the free set
 * has it, solid otherwise.
 */
export const icon = (glyph: string, cls = ""): string =>
  `<i class="${REGULAR.has(glyph) ? "fa-regular" : "fa-solid"} ${glyph} ${cls}"></i>`;
/**
 * Split a From header into its display name and address.
 *
 * Handles the `Name <addr>` form and the bare-address form; a quoted name has
 * its quotes stripped. When there is no name, the address becomes the primary
 * line and the secondary line is left empty rather than repeating it.
 */
export function parseFrom(raw: string | null): { name: string; email: string } {
  const s = (raw ?? "").trim();
  const angled = s.match(/^(.*?)\s*<([^>]+)>$/);
  if (angled) {
    const name = angled[1].trim().replace(/^["'](.*)["']$/, "$1").trim();
    const email = angled[2].trim();
    return name && name !== email ? { name, email } : { name: email, email: "" };
  }
  return { name: s, email: "" };
}
/** What reads as an address inside a display name. */
const NAMED_ADDRESS = /[^\s<>()"',;:@]+@[^\s<>()"',;:@]+\.[^\s<>()"',;:@]+/g;
/**
 * Whether a sender's name is itself an address other than the one the
 * message is from — `"service@paypal.com" <security@paypa1-support.ru>` —
 * which is how a forged sender passes for a known one where only names show.
 */
export function spoofedName(name: string, email: string): boolean {
  if (!email) return false;
  const named = name.match(NAMED_ADDRESS) ?? [];
  return named.length > 0 && !named.some((a) => a.toLowerCase() === email.toLowerCase());
}
/** The mark before the real address of a sender whose name is another address. */
export const SPOOF_TITLE = "The name is an address other than the sender's";
/**
 * A moment as short as a line has room for: `12:45` today, `13 Oct` within
 * the year, `13 Oct 2025` before it.
 */
export function shortWhen(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  if (d.toDateString() === now.toDateString()) return formatDate(iso, "%H:%M");
  if (d.getFullYear() === now.getFullYear()) return formatDate(iso, "%-d %b");
  return formatDate(iso, "%-d %b %Y");
}
/**
 * When a message was sent, as the reader's header says it: `Today 12:04`,
 * `Yesterday 18:30`, `Tue 13 Oct, 09:12` within the year, `4 Mar 2025, 09:12`
 * before it. An unreadable date is shown as written.
 */
export function readerDate(iso: string | null, now: Date = new Date()): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const time = formatDate(iso, "%H:%M");
  const day = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((day(now) - day(d)) / 86_400_000);
  if (days === 0) return `Today ${time}`;
  if (days === 1) return `Yesterday ${time}`;
  if (d.getFullYear() === now.getFullYear()) return `${formatDate(iso, "%a %-d %b")}, ${time}`;
  return `${formatDate(iso, "%-d %b %Y")}, ${time}`;
}
/**
 * The recipients as one line of names: a name where there is one, else the
 * address, and `me` for an address of the user's own.
 */
export function recipientNames(list: string | null, own: Set<string>): string {
  return (list ?? "")
    .split(",")
    .map((entry) => parseFrom(entry.trim()))
    .map(({ name, email }) => {
      const address = (email || name).toLowerCase();
      if (own.has(address)) return "me";
      return name || email;
    })
    .filter(Boolean)
    .join(", ");
}
