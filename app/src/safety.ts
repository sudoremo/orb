/**
 * What the reader does about a message trying to fool or follow the person
 * reading it: tracking pixels taken out before they can load, and links
 * that are not what they say they are.
 */
import { askFirst, call } from "./bridge";

/**
 * The host a suspicious link is pointed at instead of its own address. The
 * shell's navigation guard catches it and hands it back to ask first (see
 * `stay_in_app` in the shell).
 */
const LINK_CHECK_HOST = "link-check.orb.invalid";

/** Hosts that serve nothing but open-tracking pixels. */
const TRACKER_HOSTS = [
  "list-manage.com",
  "mailtrack.io",
  "mailstat.us",
  "emltrk.com",
  "t.yesware.com",
  "track.hubspot.com",
  "t.hubspotemail.net",
  "t.hubspotfree.net",
  "r.superhuman.com",
  "track.mixmax.com",
  "trk.klclick.com",
  "open.convertkit-mail.com",
  "pixel.app.returnpath.net",
  "ea.pstmrk.it",
  "mandrillapp.com",
];

/** Paths the large sending services serve their open pixels from. */
const TRACKER_PATHS = /\/(track\/open|wf\/open|open\.php)\b/i;

/** True for a length of at most a pixel: `1`, `0`, `1px`. */
const tiny = (value: string | null | undefined): boolean => !!value && /^\s*[01](\.0+)?(px)?\s*$/i.test(value);

/**
 * True for an image that is there to report the message was opened: a
 * remote image a pixel or less in size, one hidden from view, or one from a
 * host that serves nothing else.
 */
export function isTrackingPixel(image: Element): boolean {
  const src = image.getAttribute("src") ?? "";
  if (!/^\s*(https?:)?\/\//i.test(src)) return false;
  const style = (image.getAttribute("style") ?? "").toLowerCase().replace(/\s+/g, "");
  const styled = (property: string) => new RegExp(`(^|;)${property}:([^;!]+)`).exec(style)?.[2];
  const width = image.getAttribute("width") ?? styled("width");
  const height = image.getAttribute("height") ?? styled("height");
  if ((tiny(width) && (tiny(height) || !height)) || (tiny(height) && !width)) return true;
  if (/(^|;)(display:none|visibility:hidden|opacity:0(\.0+)?)(;|$)/.test(style)) return true;
  let url: URL;
  try {
    url = new URL(src.trim(), "https://invalid.invalid");
  } catch {
    return false;
  }
  const host = url.hostname.toLowerCase();
  return TRACKER_HOSTS.some((t) => host === t || host.endsWith(`.${t}`)) || TRACKER_PATHS.test(url.pathname);
}

/**
 * Top-level domains a bare name in a link's text has to end in to be read
 * as an address: without a scheme or `www.`, `Node.js` is a name, not a
 * site.
 */
const COMMON_TLDS = new Set(
  (
    "com org net edu gov info biz io co me app dev ch de at li fr it es nl be lu uk " +
    "eu us ca au nz se no dk fi pl cz ie pt"
  ).split(" "),
);

/** The host a link's text shows, if the text is an address. */
function hostInText(text: string): string | null {
  const shown = /^\s*(https?:\/\/)?(www\.)?((?:[\p{L}\p{N}-]+\.)+([\p{L}]{2,}))(:\d+)?([/?#]\S*)?\s*$/iu.exec(text);
  if (!shown) return null;
  const [, scheme, www, host, tld] = shown;
  if (!scheme && !www && !COMMON_TLDS.has(tld!.toLowerCase())) return null;
  try {
    return new URL(`https://${host}`).hostname;
  } catch {
    return null;
  }
}

/** True when two hosts are one site: the same name, or one under the other. */
function sameSite(a: string, b: string): boolean {
  const bare = (host: string) => host.toLowerCase().replace(/^www\./, "");
  const [x, y] = [bare(a), bare(b)];
  return x === y || x.endsWith(`.${y}`) || y.endsWith(`.${x}`);
}

/**
 * Why a link should not be opened without asking, if there is a reason:
 * its text names another site than it opens, or its address is built to be
 * misread.
 */
export function linkSuspicion(href: string, text: string): string | null {
  let url: URL;
  try {
    // `//host/path` takes the scheme of the page, so it is a link out too.
    url = new URL(href.trim(), "https://relative.invalid");
  } catch {
    return null;
  }
  if (url.hostname === "relative.invalid") return null;
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  const host = url.hostname.toLowerCase();
  if (url.username || url.password) {
    return "The address puts a name before an @, which hides the site it really opens.";
  }
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(host) || host.startsWith("[")) {
    return "The address is a bare IP address rather than a named site.";
  }
  if (host.split(".").some((label) => label.startsWith("xn--"))) {
    return "The address is spelled with international characters, which can imitate a familiar name.";
  }
  const shown = hostInText(text);
  if (shown && !sameSite(shown, host)) return "The link shows one address and opens another.";
  return null;
}

/** The address a suspicious link is rewritten to, carrying what it was. */
export function checkedHref(href: string, text: string, why: string): string {
  const query = new URLSearchParams({ url: href.trim(), text: text.trim(), why });
  return `https://${LINK_CHECK_HOST}/?${query}`;
}

/**
 * Ask before opening a link `linkSuspicion` found something wrong with,
 * and open it if told to.
 */
export async function openSuspiciousLink(url: string, text: string, why: string): Promise<void> {
  const shown = text && text !== url ? `The link reads “${text}”.\n` : "";
  const go = await askFirst(`Open this link?\n\n${why}\n\n${shown}It opens ${url}`, "Open Link");
  if (go) await call("open_external", { url });
}

/** Answer a link the shell's guard sent back, from its rewritten address. */
export function openCheckedLink(address: string): Promise<void> {
  const query = new URL(address).searchParams;
  return openSuspiciousLink(query.get("url") ?? "", query.get("text") ?? "", query.get("why") ?? "");
}
