/**
 * A minimal modal key dispatcher.
 *
 * Counts, multi-key sequences (`gg`) and pending state live in one place and
 * resolve to a *command string* from `config.yml` — not to a function — so the
 * user's binding table is the single source of truth. Running the command is
 * the caller's job.
 *
 * The table is supplied as a callback because the active bindings depend on the
 * focused pane (`index_normal` vs `index_detail`), which changes between
 * keystrokes.
 */

import { eventKey } from "./keys";

/** Keys that only modify the next one. */
const MODIFIERS = new Set(["Shift", "Control", "Alt", "Meta", "CapsLock", "Fn"]);

/** Runs a resolved command string, e.g. `stage-read && mark-move Archive`. */
export type CommandRunner = (command: string, count: number) => void | Promise<void>;

/** Key notation -> command string, already canonicalised. */
export type BindingTable = Record<string, string>;

export class Vim {
  /** Digits typed before a motion, e.g. the `12` of `12j`. */
  private count = "";
  /** Keys of an in-progress multi-key sequence, e.g. the `g` of `gg`. */
  private pending = "";

  constructor(
    private readonly bindings: () => BindingTable,
    private readonly run: CommandRunner,
    /** Told the pending sequence whenever it changes, `""` once it is gone. */
    private readonly onPending: (sequence: string, table: BindingTable) => void = () => {},
  ) {}

  /** Feed a key event. Returns true when it was consumed by a binding. */
  handle(e: KeyboardEvent): boolean {
    // Shift pressed on its way to a capital arrives as a key of its own, and
    // would end the sequence the capital continues: `gT`, `zR`.
    if (MODIFIERS.has(e.key)) return false;
    const table = this.bindings();
    const key = eventKey(e);

    // A bare digit builds the count, except `0` which is a motion of its own
    // until a count is already in progress — and a digit that carries on a
    // pending sequence, as the `1` of a `g1` bound in the config does.
    const continues = this.pending !== "" && (table[this.pending + key] !== undefined || this.isPrefix(table, this.pending + key));
    if (!continues && (/^[1-9]$/.test(key) || (key === "0" && this.count !== ""))) {
      this.count += key;
      return true;
    }

    if (key === "<esc>") {
      // Escape always clears pending state first; only a clean Escape falls
      // through to the bound command.
      const hadPending = this.count !== "" || this.pending !== "";
      this.reset();
      if (hadPending) return true;
    }

    const sequence = this.pending + key;

    // A prefix of some longer binding: wait for the next key.
    if (!table[sequence] && this.isPrefix(table, sequence)) {
      this.pending = sequence;
      this.onPending(sequence, table);
      return true;
    }

    const command = table[sequence];
    if (this.pending) this.onPending("", table);
    this.pending = "";
    if (!command) {
      this.count = "";
      return false;
    }

    const count = this.count === "" ? 1 : parseInt(this.count, 10);
    this.count = "";
    void this.run(command, count);
    return true;
  }

  /** True when `sequence` starts, but does not complete, some binding. */
  private isPrefix(table: BindingTable, sequence: string): boolean {
    return Object.keys(table).some(
      (b) => b.length > sequence.length && b.startsWith(sequence),
    );
  }

  private reset(): void {
    this.count = "";
    if (this.pending) this.onPending("", {});
    this.pending = "";
  }
}
