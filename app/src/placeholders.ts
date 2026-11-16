/**
 * Placeholders of mail templates, as the compose window handles them.
 *
 * In Neovim the core keeps each placeholder as an extmark, which moves with
 * the text around it. In the plain text field the window keeps the same
 * thing itself: a span of the field's text per placeholder, moved along with
 * every edit, and the text Orb last wrote there — a span edited by hand since
 * is left to the hand that edited it.
 */

/** Where a placeholder stands in expanded template text (see the core's `templates::Mark`). */
export interface Mark {
  name: string;
  line: number;
  start: number;
  end: number;
  start16: number;
  end16: number;
}

/** Template text with its escapes resolved and its placeholders marked. */
export interface Expanded {
  lines: string[];
  marks: Mark[];
}

/** A placeholder's span in the plain field's text, in UTF-16 units. */
export interface Span {
  name: string;
  start: number;
  end: number;
  /** What Orb last wrote there. */
  text: string;
}

/** A placeholder as it is written while nothing fills it. */
export const braced = (name: string): string => `{${name}}`;

/** `text` with each mark replaced by its value, or left in braces without one. */
export function render(text: Expanded, values: Record<string, string>): string[] {
  return text.lines.map((line, index) => {
    const marks = text.marks.filter((m) => m.line === index).sort((a, b) => b.start16 - a.start16);
    let out = line;
    for (const m of marks) {
      out = out.slice(0, m.start16) + (values[m.name] || braced(m.name)) + out.slice(m.end16);
    }
    return out;
  });
}

/**
 * Put `text` into `value` at `caret`, as Neovim does: in place of the caret's
 * line when it is blank, below it otherwise. Returns the new text, the spans
 * of its placeholders and where the caret goes.
 */
export function insertPlain(
  value: string,
  caret: number,
  text: Expanded,
): { value: string; spans: Span[]; caret: number } {
  const lineStart = value.lastIndexOf("\n", caret - 1) + 1;
  const lineEndAt = value.indexOf("\n", caret);
  const lineEnd = lineEndAt < 0 ? value.length : lineEndAt;
  const blank = value.slice(lineStart, lineEnd).trim() === "";
  const inserted = text.lines.join("\n");
  let at: number;
  let next: string;
  if (blank) {
    at = lineStart;
    next = value.slice(0, lineStart) + inserted + value.slice(lineEnd);
  } else {
    at = lineEnd + 1;
    next = `${value.slice(0, lineEnd)}\n${inserted}${value.slice(lineEnd)}`;
  }
  const lineOffsets: number[] = [];
  let offset = at;
  for (const line of text.lines) {
    lineOffsets.push(offset);
    offset += line.length + 1;
  }
  const spans = text.marks.map((m) => ({
    name: m.name,
    start: lineOffsets[m.line] + m.start16,
    end: lineOffsets[m.line] + m.end16,
    text: braced(m.name),
  }));
  return { value: next, spans, caret: at };
}

/**
 * The spans moved along with an edit that turned `before` into `after`.
 * An edit is one contiguous change, as a text field makes them: a span
 * after it moves by the change in length, one before it stays, and one it
 * overlaps keeps its start, ending where the edit left its end.
 */
export function followEdit(spans: Span[], before: string, after: string): Span[] {
  if (before === after) return spans;
  let prefix = 0;
  const max = Math.min(before.length, after.length);
  while (prefix < max && before[prefix] === after[prefix]) prefix++;
  let suffix = 0;
  while (
    suffix < max - prefix &&
    before[before.length - 1 - suffix] === after[after.length - 1 - suffix]
  ) {
    suffix++;
  }
  const oldEnd = before.length - suffix;
  const delta = after.length - before.length;
  return spans.map((s) => {
    if (s.end <= prefix) return s;
    if (s.start >= oldEnd) return { ...s, start: s.start + delta, end: s.end + delta };
    return { ...s, end: Math.max(s.start, s.end + delta) };
  });
}

/**
 * Write `value` (or the placeholder again, when empty) wherever `name`
 * stands in `text`, except where the span was edited by hand.
 */
export function fillPlain(
  text: string,
  spans: Span[],
  name: string,
  value: string,
): { text: string; spans: Span[] } {
  const written = value.replace(/[\r\n]/g, "") || braced(name);
  let out = text;
  let current = spans;
  for (let i = 0; i < current.length; i++) {
    const s = current[i];
    if (s.name !== name || out.slice(s.start, s.end) !== s.text) continue;
    const next = out.slice(0, s.start) + written + out.slice(s.end);
    const delta = written.length - (s.end - s.start);
    current = current.map((o, j) => {
      if (j === i) return { ...o, end: o.start + written.length, text: written };
      if (o.start >= s.end) return { ...o, start: o.start + delta, end: o.end + delta };
      return o;
    });
    out = next;
  }
  return { text: out, spans: current };
}
