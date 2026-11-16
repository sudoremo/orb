/**
 * Recipients as tokens, the way every mail client has done it since Mail.app.
 *
 * A recipient stops being text the moment it is complete: it becomes one
 * object that can be selected, deleted and dragged past as a unit. That is
 * worth the machinery, because the alternative — a comma-separated string —
 * makes the commonest edit in the whole window, "remove that one person",
 * a matter of finding two commas and not eating a third.
 *
 * What a token holds is what the header will carry, so a name that arrived with
 * an address survives the round trip. What it *shows* is the name alone, since
 * a row of `Name <address>` is unreadable at four recipients.
 *
 * Nothing here validates. A draft is allowed to hold a recipient that is not an
 * address yet, and saving one must never fail; an address that does not look
 * like an address is marked, not rejected.
 */

import { invoke } from "@tauri-apps/api/core";

/** One recipient: what to show, and what to send to. */
export interface Token {
  name?: string;
  address: string;
}

/** What the core offers while a recipient is being typed. */
interface Suggestion {
  name: string | null;
  recipients: { name: string | null; address: string }[];
  source: { kind: "alias" | "group" | "history" | "system"; shortcut?: string };
  score: number;
}

/** How long to wait before asking, so a fast typist asks once, not six times. */
const SUGGEST_DEBOUNCE = 90;

/** How many completions to show. Beyond this the list is read, not scanned. */
const SUGGEST_LIMIT = 8;

/** Characters that end a recipient, as they do on every mail client. */
const SEPARATORS = [",", ";"];

export class TokenField {
  private tokens: Token[] = [];
  readonly host: HTMLElement;
  /** The fields a recipient can be moved between, when it is one of them. */
  group: TokenGroup | null = null;
  private readonly input: HTMLInputElement;
  private readonly list: HTMLUListElement;
  private readonly onChange: () => void;
  private readonly onProblem: (reason: string) => void;

  /** Which token is selected, for the Backspace-then-Backspace idiom. */
  private selected: number | null = null;

  private suggestions: Suggestion[] = [];
  private highlighted = 0;
  private timer: number | null = null;
  /** Which query the open list belongs to, so a late answer cannot overwrite
   *  a newer one. */
  private outstanding = "";

  constructor(
    host: HTMLElement,
    onChange: () => void,
    onProblem: (reason: string) => void = () => {},
  ) {
    this.host = host;
    this.onChange = onChange;
    this.onProblem = onProblem;
    this.host.classList.add("tokens");

    this.input = document.createElement("input");
    this.input.type = "text";
    this.input.spellcheck = false;
    this.input.autocomplete = "off";
    this.input.className = "token-input";

    this.list = document.createElement("ul");
    this.list.className = "completions";
    this.list.hidden = true;

    this.host.append(this.input, this.list);
    this.bind();
    this.render();
  }

  /** The recipients, as a header carries them. */
  get value(): string[] {
    // Whatever is half-typed counts: someone who fills in the field and presses
    // Send without a comma first means the address they typed.
    const pending = this.input.value.trim();
    const all = pending ? [...this.tokens, parse(pending)] : this.tokens;
    return all.map(format);
  }

  set value(addresses: string[]) {
    this.tokens = addresses.flatMap(split).map(parse);
    this.input.value = "";
    this.render();
  }

  focus(): void {
    this.input.focus();
  }

  /** Whether the completion list is open, which changes what Enter means. */
  get isCompleting(): boolean {
    return !this.list.hidden;
  }

  private bind(): void {
    this.input.addEventListener("input", () => {
      this.select(null);
      this.scheduleSuggest();
    });

    this.input.addEventListener("keydown", (e) => this.onKey(e));

    // Leaving the field completes what was typed, which is what someone means
    // when they type an address and click Send.
    this.input.addEventListener("blur", () => {
      // Deferred: a click on a completion blurs the input before the click
      // lands, and committing here first would close the list underneath it.
      window.setTimeout(() => {
        if (document.activeElement !== this.input) {
          this.commitTyped();
          this.closeList();
        }
      }, 120);
    });

    // Pasting a list of addresses should produce a list of tokens, not one
    // token holding a list.
    this.input.addEventListener("paste", (e) => {
      const text = e.clipboardData?.getData("text") ?? "";
      if (!SEPARATORS.some((s) => text.includes(s))) return;
      e.preventDefault();
      for (const part of split(text)) this.add(parse(part));
    });

    this.host.addEventListener("pointerdown", (e) => {
      const target = e.target as HTMLElement;
      const pill = target.closest<HTMLElement>(".token");
      if (!pill || target.closest("[data-remove]") || !this.group) return;
      if (e.button !== 0 || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
      const token = this.tokens[Number(pill.dataset.index)];
      if (token) this.group.arm(this, token, pill, e);
    });

    this.host.addEventListener("mousedown", (e) => {
      const target = e.target as HTMLElement;
      const remove = target.closest<HTMLElement>("[data-remove]");
      if (remove) {
        e.preventDefault();
        this.removeAt(Number(remove.dataset.remove));
        return;
      }
      const token = target.closest<HTMLElement>(".token");
      if (token) {
        e.preventDefault();
        this.select(Number(token.dataset.index));
        this.input.focus();
        return;
      }
      // A click on the empty part of the field is a click into it.
      if (target === this.host) {
        this.select(null);
        this.input.focus();
      }
    });

    // Editing a token turns it back into the text it came from — the way to
    // fix a typo in an address without retyping the whole thing.
    this.host.addEventListener("dblclick", (e) => {
      const token = (e.target as HTMLElement).closest<HTMLElement>(".token");
      if (!token) return;
      const index = Number(token.dataset.index);
      this.commitTyped();
      const [edited] = this.tokens.splice(index, 1);
      this.input.value = format(edited);
      this.render();
      this.input.focus();
      this.scheduleSuggest();
    });

    this.list.addEventListener("mousedown", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-suggestion]");
      if (!row) return;
      // Before the blur handler can run: the click is the commit.
      e.preventDefault();
      this.accept(Number(row.dataset.suggestion));
    });
  }

  private onKey(e: KeyboardEvent): void {
    // ⌥↑/⌥↓ take the selected recipient to the field above or below.
    const vertical = e.key === "ArrowUp" || e.key === "ArrowDown";
    if (vertical && e.altKey && !e.metaKey && !e.ctrlKey && !e.shiftKey && this.selected !== null && this.group) {
      e.preventDefault();
      this.group.moveSelected(this, e.key === "ArrowUp" ? -1 : 1);
      return;
    }
    // Down opens the list when it is closed, which is how every completion
    // field on the platform behaves and the only way to see who you write to
    // before typing anything.
    if (e.key === "ArrowDown" && !this.isCompleting) {
      e.preventDefault();
      void this.suggest(this.input.value.trim());
      return;
    }

    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (!this.isCompleting) return;
      e.preventDefault();
      const step = e.key === "ArrowDown" ? 1 : -1;
      const count = this.suggestions.length;
      this.highlighted = (this.highlighted + step + count) % count;
      this.renderList();
      return;
    }

    if (e.key === "Escape" && this.isCompleting) {
      e.preventDefault();
      this.closeList();
      return;
    }

    if (e.key === "Enter" || e.key === "Tab") {
      if (this.isCompleting) {
        e.preventDefault();
        this.accept(this.highlighted);
        return;
      }
      // Enter on a typed address completes it; Tab is left alone so it can
      // still move to the next field.
      if (e.key === "Enter" && this.input.value.trim()) {
        e.preventDefault();
        this.commitTyped();
      }
      return;
    }

    if (SEPARATORS.includes(e.key)) {
      e.preventDefault();
      this.commitTyped();
      return;
    }

    if (e.key === "Backspace" && this.input.selectionStart === 0 && !this.input.value) {
      e.preventDefault();
      // The idiom every mail client shares: the first Backspace selects the
      // last recipient, the second deletes it. Deleting outright means one
      // keystroke too many silently removes somebody from the message.
      if (this.selected !== null) {
        this.removeAt(this.selected);
      } else if (this.tokens.length) {
        this.select(this.tokens.length - 1);
      }
      return;
    }

    // Any other key means typing, and typing means the selection is over.
    if (e.key.length === 1) this.select(null);
  }

  tokenAt(index: number): Token | undefined {
    return this.tokens[index];
  }

  /** Whether the field holds this very recipient. */
  holds(token: Token): boolean {
    return this.tokens.includes(token);
  }

  /** How many recipients the field holds, half-typed text aside. */
  get count(): number {
    return this.tokens.length;
  }

  /**
   * Take a recipient out without a word: the move it is part of says what
   * changed, once, when it is over.
   */
  take(token: Token): void {
    const index = this.tokens.indexOf(token);
    if (index < 0) return;
    this.tokens.splice(index, 1);
    this.selected = null;
    this.render();
  }

  /**
   * Put a recipient in at `index`, and say where it stands. An address the
   * field already holds stays where it is, once.
   */
  insert(token: Token, index: number): number {
    const address = token.address.toLowerCase();
    const already = this.tokens.findIndex((t) => t.address.toLowerCase() === address);
    if (already >= 0) return already;
    const at = Math.max(0, Math.min(index, this.tokens.length));
    this.tokens.splice(at, 0, token);
    this.render();
    return at;
  }

  /** Select the recipient at `index` and give the field the keyboard. */
  selectAndFocus(index: number): void {
    this.select(index);
    this.input.focus();
  }

  /** The selected recipient's place, if one is selected. */
  get selectedIndex(): number | null {
    return this.selected;
  }

  /**
   * Where a recipient let go at a point would go: its index among the
   * recipients other than `moving`. The rows wrap, so a point above a pill's
   * line, or on it and left of its middle, comes before that pill.
   */
  dropIndex(x: number, y: number, moving: Token): number {
    const others = this.pills().filter((p) => p.token !== moving);
    const before = others.findIndex(({ pill }) => {
      const r = pill.getBoundingClientRect();
      return y < r.top || (y < r.bottom && x < r.left + r.width / 2);
    });
    return before < 0 ? others.length : before;
  }

  /**
   * Show where a dropped recipient would land, as a bar between two pills;
   * `null` takes it away. Drawn over the pills rather than among them, so
   * nothing moves under the pointer while it is aimed.
   */
  showDropCaret(index: number | null, moving: Token): void {
    let caret = this.host.querySelector<HTMLElement>(":scope > .token-caret");
    if (index === null) {
      caret?.remove();
      return;
    }
    if (!caret) {
      caret = document.createElement("span");
      caret.className = "token-caret";
      this.host.append(caret);
    }
    const others = this.pills().filter((p) => p.token !== moving).map((p) => p.pill);
    const next = others[index];
    const last = others[others.length - 1];
    const left = next ? next.offsetLeft - 3 : last ? last.offsetLeft + last.offsetWidth + 2 : 0;
    const line = next ?? last ?? this.input;
    caret.style.left = `${left}px`;
    caret.style.top = `${line.offsetTop}px`;
    caret.style.height = `${line.offsetHeight}px`;
  }

  /** The pills on screen, each with the recipient it stands for. */
  private pills(): { pill: HTMLElement; token: Token }[] {
    return Array.from(this.host.querySelectorAll<HTMLElement>(":scope > .token")).flatMap((pill) => {
      const token = this.tokens[Number(pill.dataset.index)];
      return token ? [{ pill, token }] : [];
    });
  }

  /** Let the completions go, as when the field is left. */
  dismissCompletions(): void {
    this.closeList();
  }

  private select(index: number | null): void {
    this.selected = index;
    // Marked in place: a pill drawn again under a press is not the one the
    // second click of a double-click lands on, nor the one a drag lifts.
    for (const { pill } of this.pills()) pill.classList.toggle("selected", Number(pill.dataset.index) === index);
  }

  private add(token: Token): void {
    // The same person twice is a mistake every time, and mail servers deliver
    // both copies.
    if (!this.tokens.some((t) => t.address.toLowerCase() === token.address.toLowerCase())) {
      this.tokens.push(token);
    }
    this.input.value = "";
    this.closeList();
    this.render();
    this.onChange();
  }

  private removeAt(index: number): void {
    this.tokens.splice(index, 1);
    this.selected = null;
    this.render();
    this.onChange();
    this.input.focus();
  }

  private commitTyped(): void {
    const text = this.input.value.trim();
    if (!text) return;
    for (const part of split(text)) this.add(parse(part));
  }

  private accept(index: number): void {
    const chosen = this.suggestions[index];
    if (!chosen) return;
    // A group is several recipients wearing one row.
    for (const r of chosen.recipients) {
      this.add({ name: r.name ?? undefined, address: r.address });
    }
    this.input.focus();
  }

  private scheduleSuggest(): void {
    if (this.timer !== null) window.clearTimeout(this.timer);
    const query = this.input.value.trim();
    if (!query) {
      // Deleting back to nothing closes the list rather than falling back to
      // the general answer, which is not what someone clearing a field wants.
      this.closeList();
      return;
    }
    this.timer = window.setTimeout(() => void this.suggest(query), SUGGEST_DEBOUNCE);
  }

  private async suggest(query: string): Promise<void> {
    this.outstanding = query;
    let found: Suggestion[] = [];
    try {
      found = await invoke<Suggestion[]>("suggest_recipients", {
        query,
        limit: SUGGEST_LIMIT,
      });
    } catch (e) {
      // Swallowing this made a broken completion list indistinguishable from
      // one that simply found nobody — which is the difference between "type
      // it out" and "something is wrong". The field still works either way, so
      // the report goes to the status line rather than in the way.
      this.closeList();
      this.onProblem(typeof e === "string" ? e : ((e as Error)?.message ?? JSON.stringify(e)));
      return;
    }
    // Two keystrokes in flight at once: the older answer is not an answer to
    // what is on screen now.
    if (this.outstanding !== query || this.input.value.trim() !== query) return;

    this.suggestions = found;
    this.highlighted = 0;
    this.renderList();
  }

  private closeList(): void {
    this.suggestions = [];
    this.list.hidden = true;
    this.list.replaceChildren();
  }

  private renderList(): void {
    if (!this.suggestions.length) {
      this.closeList();
      return;
    }
    this.list.hidden = false;
    this.list.replaceChildren(
      ...this.suggestions.map((s, i) => {
        const row = document.createElement("li");
        row.dataset.suggestion = String(i);
        row.className = i === this.highlighted ? "highlighted" : "";

        const name = document.createElement("span");
        name.className = "who";
        name.textContent = s.name ?? s.recipients[0]?.address ?? "";

        const detail = document.createElement("span");
        detail.className = "detail";
        detail.textContent = describe(s);

        row.append(name, detail);
        return row;
      }),
    );
  }

  private render(): void {
    for (const old of Array.from(this.host.querySelectorAll(".token"))) old.remove();

    this.tokens.forEach((token, index) => {
      const pill = document.createElement("span");
      pill.className = "token";
      if (index === this.selected) pill.classList.add("selected");
      // Marked, never rejected: a draft is allowed to be half-written.
      if (!looksLikeAddress(token.address)) pill.classList.add("incomplete");
      pill.dataset.index = String(index);
      // The address is what the pill hides, so it is what the tooltip shows.
      pill.title = format(token);

      const label = document.createElement("span");
      label.textContent = token.name?.trim() || token.address;

      const remove = document.createElement("button");
      remove.type = "button";
      remove.dataset.remove = String(index);
      remove.title = "Remove";
      remove.textContent = "✕";

      pill.append(label, remove);
      this.host.insertBefore(pill, this.input);
    });
  }
}

/** A field of a group, with how to show it when it is put away. */
export interface GroupMember {
  field: TokenField;
  /** Whether the field's row is put away, as Bcc is until asked for. */
  hidden?: () => boolean;
  reveal?: () => void;
  conceal?: () => void;
}

/** How far the pointer goes before a press on a recipient becomes a drag. */
const DRAG_THRESHOLD = 4;

/** A drag on its way, from the press that may start it to the release. */
interface Drag {
  source: TokenField;
  token: Token;
  pill: HTMLElement;
  pointer: number;
  startX: number;
  startY: number;
  /** Where the pointer holds the pill, so the ghost does not jump to it. */
  offsetX: number;
  offsetY: number;
  ghost: HTMLElement | null;
  target: TokenField | null;
  /** Rows shown for the drag alone, put away again unless it lands there. */
  revealed: GroupMember[];
  detach: () => void;
}

/**
 * The recipient fields of one message, top to bottom: a recipient is dragged
 * from one to another or along its own, or taken up or down with ⌥↑/⌥↓.
 *
 * A move is one change, said once when it is over. The fields take and insert
 * without a word, so a save never sees the recipient in neither field or in
 * both, and a move that changes nothing says nothing.
 *
 * Dragging follows the pointer rather than the browser's drag and drop, which
 * the window gives over to files dropped in from outside.
 */
export class TokenGroup {
  private drag: Drag | null = null;

  constructor(
    private readonly members: GroupMember[],
    private readonly onChange: () => void,
  ) {
    for (const m of members) m.field.group = this;
  }

  /** Take the field's selected recipient to the next field up or down. */
  moveSelected(field: TokenField, step: 1 | -1): void {
    const index = field.selectedIndex;
    const from = this.members.findIndex((m) => m.field === field);
    const to = this.members[from + step];
    const token = index === null ? undefined : field.tokenAt(index);
    if (!token || from < 0 || !to) return;
    this.place(field, token, to, Number.MAX_SAFE_INTEGER);
  }

  /** A press on a recipient, which becomes a drag once the pointer moves. */
  arm(source: TokenField, token: Token, pill: HTMLElement, e: PointerEvent): void {
    this.end(null);
    const rect = pill.getBoundingClientRect();
    const move = (ev: PointerEvent) => {
      if (ev.pointerId === drag.pointer) this.follow(ev);
    };
    const up = (ev: PointerEvent) => {
      if (ev.pointerId === drag.pointer) this.end(ev.type === "pointerup" ? ev : null);
    };
    const key = (ev: KeyboardEvent) => {
      if (ev.key !== "Escape" || !drag.ghost) return;
      ev.preventDefault();
      ev.stopPropagation();
      this.end(null);
    };
    const blur = () => this.end(null);
    const drag: Drag = {
      source,
      token,
      pill,
      pointer: e.pointerId,
      startX: e.clientX,
      startY: e.clientY,
      offsetX: e.clientX - rect.left,
      offsetY: e.clientY - rect.top,
      ghost: null,
      target: null,
      revealed: [],
      detach: () => {
        window.removeEventListener("pointermove", move, true);
        window.removeEventListener("pointerup", up, true);
        window.removeEventListener("pointercancel", up, true);
        window.removeEventListener("keydown", key, true);
        window.removeEventListener("blur", blur);
      },
    };
    window.addEventListener("pointermove", move, true);
    window.addEventListener("pointerup", up, true);
    window.addEventListener("pointercancel", up, true);
    window.addEventListener("keydown", key, true);
    window.addEventListener("blur", blur);
    this.drag = drag;
  }

  private follow(e: PointerEvent): void {
    const drag = this.drag;
    if (!drag) return;
    if (!drag.ghost) {
      if (Math.hypot(e.clientX - drag.startX, e.clientY - drag.startY) < DRAG_THRESHOLD) return;
      this.lift(drag);
    }
    drag.ghost!.style.transform = `translate(${e.clientX - drag.offsetX}px, ${e.clientY - drag.offsetY}px)`;
    const target = this.memberAt(e.clientX, e.clientY)?.field ?? null;
    if (drag.target && drag.target !== target) drag.target.showDropCaret(null, drag.token);
    drag.target = target;
    target?.showDropCaret(target.dropIndex(e.clientX, e.clientY, drag.token), drag.token);
  }

  /** The press has moved far enough: the recipient comes off its field. */
  private lift(drag: Drag): void {
    const ghost = drag.pill.cloneNode(true) as HTMLElement;
    ghost.classList.add("token-ghost");
    ghost.classList.remove("selected");
    ghost.style.width = `${drag.pill.getBoundingClientRect().width}px`;
    document.body.append(ghost);
    drag.ghost = ghost;
    drag.pill.classList.add("dragging");
    document.documentElement.classList.add("dragging-token");
    drag.source.dismissCompletions();
    try {
      drag.pill.setPointerCapture(drag.pointer);
    } catch {
      // A pointer the browser no longer tracks ends the drag on its own.
    }
    // A field put away is a field it can go to: shown while the drag lasts.
    for (const m of this.members) {
      if (m.hidden?.() && m.reveal) {
        m.reveal();
        drag.revealed.push(m);
      }
    }
  }

  /** Let go: dropped where `release` happened, put back without one. */
  private end(release: PointerEvent | null): void {
    const drag = this.drag;
    if (!drag) return;
    this.drag = null;
    drag.detach();
    if (!drag.ghost) return;
    drag.ghost.remove();
    drag.pill.classList.remove("dragging");
    document.documentElement.classList.remove("dragging-token");
    drag.target?.showDropCaret(null, drag.token);
    const target = release ? this.memberAt(release.clientX, release.clientY) : undefined;
    if (target && release) {
      this.place(drag.source, drag.token, target, target.field.dropIndex(release.clientX, release.clientY, drag.token));
    }
    for (const m of drag.revealed) {
      if (m.field.count === 0) m.conceal?.();
    }
  }

  /** The field whose row is under a point, label and empty space included. */
  private memberAt(x: number, y: number): GroupMember | undefined {
    return this.members.find((m) => {
      if (m.hidden?.()) return false;
      const r = (m.field.host.closest<HTMLElement>(".field") ?? m.field.host).getBoundingClientRect();
      return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
    });
  }

  /** Move a recipient to `index` of a field: the one change it is, or none. */
  private place(source: TokenField, token: Token, target: GroupMember, index: number): void {
    // Changed under the drag, by a draft read in again: nothing to move.
    if (!source.holds(token)) return;
    const before = [source.value, target.field.value].join("\n");
    source.take(token);
    if (target.hidden?.()) target.reveal?.();
    const at = target.field.insert(token, index);
    target.field.selectAndFocus(at);
    if ([source.value, target.field.value].join("\n") !== before) this.onChange();
  }
}

/** What a completion row says on the right. */
function describe(s: Suggestion): string {
  if (s.source.kind === "group") {
    return `${s.recipients.length} recipients`;
  }
  const address = s.recipients[0]?.address ?? "";
  return s.source.kind === "alias" ? `${s.source.shortcut} · ${address}` : address;
}

/** `Name <address>`, or the bare address when there is no name. */
export function format(token: Token): string {
  const name = token.name?.trim();
  if (!name) return token.address;
  // A name with punctuation in it has to be quoted, or the comma in
  // `Doe, Jane` reads as the end of one recipient and the start of another.
  const quoted = /[",;<>@]/.test(name) ? `"${name.replace(/"/g, '\\"')}"` : name;
  return `${quoted} <${token.address}>`;
}

/** Split `Name <a@x>, b@y` into its recipients, respecting quotes and angles. */
export function split(value: string): string[] {
  const out: string[] = [];
  let current = "";
  let quoted = false;
  let angled = false;
  for (const ch of value) {
    if (ch === '"') quoted = !quoted;
    else if (ch === "<" && !quoted) angled = true;
    else if (ch === ">" && !quoted) angled = false;
    else if (SEPARATORS.includes(ch) && !quoted && !angled) {
      out.push(current);
      current = "";
      continue;
    }
    current += ch;
  }
  out.push(current);
  return out.map((s) => s.trim()).filter(Boolean);
}

/** Pull `Name <address>` apart. Anything else is taken as an address. */
export function parse(value: string): Token {
  const text = value.trim();
  const match = text.match(/^(.*)<([^>]*)>\s*$/);
  if (!match) return { address: text };
  const name = match[1].trim().replace(/^"|"$/g, "").replace(/\\"/g, '"').trim();
  const address = match[2].trim();
  // `<a@x>` with nothing in front, or a name that only repeats the address.
  if (!name || name.toLowerCase() === address.toLowerCase()) return { address };
  return { name, address };
}

/** Enough of an address to send to — not a verdict, just a hint in the UI. */
export function looksLikeAddress(value: string): boolean {
  const [local, domain, ...rest] = value.split("@");
  return rest.length === 0 && !!local && !!domain && domain.includes(".");
}
