/**
 * Finding links in plain text.
 *
 * An HTML message carries its own anchors; a plain-text one carries a URL as
 * characters, and a mail client is expected to notice. The catch is that the
 * text is hostile input, so this runs on text that has *already* been escaped
 * and only ever inserts markup of its own — it never passes anything from the
 * message through as HTML.
 */

/**
 * URLs and bare addresses in already-escaped text.
 *
 * Deliberately conservative. A pattern that reaches too far turns the rest of a
 * sentence into part of the link, which is worse than not linking at all: the
 * link goes somewhere unintended and the text stops being readable.
 *
 * - `https://…` and `http://…`, and `www.…` which is the other way people write
 *   one.
 * - bare email addresses, which become `mailto:`.
 * - `message://…`, the form Apple Mail and Things write a link to a mail in,
 *   which Orb opens itself.
 *
 * `&` appears as `&amp;` after escaping, so the pattern has to allow `;` and
 * `&` inside a URL — which is also why trailing punctuation is trimmed
 * afterwards rather than excluded here.
 */
const LINK = /\b(?:https?:\/\/|www\.|message:\/\/)[^\s<>"']+|\b[\w.+-]+@[\w-]+(?:\.[\w-]+)+\b/gi;

/**
 * Punctuation that ends a sentence rather than a URL.
 *
 * "see https://example.com." should not link the full stop, and "(see
 * https://example.com)" should not swallow the bracket. Balanced parentheses
 * inside a URL are common enough — Wikipedia, say — that a closing one is only
 * dropped when nothing opened it.
 */
function trimTrailing(url: string): string {
  let end = url.length;
  while (end > 0) {
    const ch = url[end - 1];
    if (".,;:!?".includes(ch)) {
      end -= 1;
      continue;
    }
    if (ch === ")") {
      const inside = url.slice(0, end);
      const opens = (inside.match(/\(/g) ?? []).length;
      const closes = (inside.match(/\)/g) ?? []).length;
      if (closes > opens) {
        end -= 1;
        continue;
      }
    }
    break;
  }
  return url.slice(0, end);
}

/** The address a match should actually open. */
function hrefFor(text: string): string {
  if (/^www\./i.test(text)) return `https://${text}`;
  if (text.includes("@") && !/^(?:https?|message):/i.test(text)) return `mailto:${text}`;
  return text;
}

/**
 * Turn links in escaped text into anchors.
 *
 * `escaped` must already be HTML-escaped: this inserts tags, and anything it
 * did not insert stays exactly as it was given.
 */
export function linkify(escaped: string): string {
  return escaped.replace(LINK, (match) => {
    const url = trimTrailing(match);
    if (!url) return match;
    const tail = match.slice(url.length);

    // The escaped form is what belongs in an attribute, but the *entity* form
    // is not what should be opened, so the two differ: `&amp;` is displayed and
    // `&` is followed.
    const href = hrefFor(url).replace(/&amp;/g, "&");
    return `<a href="#" data-link="${url}" title="${href}">${url}</a>${tail}`;
  });
}

/** The address behind a linkified anchor, ready to hand to the opener. */
export function linkTarget(displayed: string): string {
  return hrefFor(displayed)
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"');
}

/**
 * The `Message-ID` a `message://` link names, or null for any other link. The
 * URL is the header percent-encoded, angle brackets and all.
 */
export function messageUrlId(url: string): string | null {
  const match = /^message:\/\/(.+)$/i.exec(url);
  if (!match) return null;
  try {
    const id = decodeURIComponent(match[1]);
    return id.includes("@") ? id : null;
  } catch {
    return null;
  }
}
