/**
 * The one fuzzy match every list in the app ranks by: the pickers, the
 * palette, the tag picker, the template picker and the command line.
 *
 * A match falls into a tier, and a better tier always wins, whatever the
 * scores within the tiers say:
 *
 * 1. the text is the query (`gsi` → "GSI");
 * 2. the text starts with it (`gsi` → "GSI Releases");
 * 3. a word of the text starts with it (`inv` → "Seedling Inventory");
 * 4. the text holds it whole (`ven` → "Inventory");
 * 5. the text holds its letters in order (`gsi` → "Garden Seedling
 *    Inventory"), word starts and runs of letters costing less than letters
 *    scattered about.
 *
 * Within a tier the shorter text ranks first. Case is never told apart.
 */

/** How well a query matches a text, lower is better, and where its letters fell. */
export interface FuzzyMatch {
  score: number;
  positions: number[];
}

/** How far apart the tiers' scores lie: further than any score within a tier can reach. */
const TIER = 1000;

const isBoundary = (t: string, at: number): boolean => at === 0 || /[^\p{L}\p{N}]/u.test(t[at - 1] ?? "");

/** Where a query found whole at `at` falls. */
const run = (at: number, length: number): number[] => Array.from({ length }, (_, i) => at + i);

/** The first place a query stands whole at the start of a word, or -1. */
function atWordStart(q: string, t: string): number {
  for (let at = t.indexOf(q); at !== -1; at = t.indexOf(q, at + 1)) {
    if (isBoundary(t, at)) return at;
  }
  return -1;
}

/**
 * One way of laying the query's letters over the text: each at its first
 * occurrence, or — with `starts` — at the next start of a word when the
 * letter does not simply continue the run before it.
 */
function lay(q: string, t: string, starts: boolean): FuzzyMatch | null {
  const positions: number[] = [];
  let score = 0;
  let from = 0;
  for (const ch of q) {
    let at = t.indexOf(ch, from);
    if (at === -1) return null;
    if (starts && !(at === from && from > 0)) {
      for (let i = at; i < t.length; i++) {
        if (t[i] === ch && isBoundary(t, i)) {
          at = i;
          break;
        }
      }
    }
    score += isBoundary(t, at) ? 0 : at === from ? 1 : 2 + (at - from);
    positions.push(at);
    from = at + 1;
  }
  return { score, positions };
}

/** Match `query` against `text`: `null` when it does not match. */
export function fuzzyMatch(query: string, text: string): FuzzyMatch | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  if (q.length === 0) return { score: 0, positions: [] };
  const shorter = Math.min(t.length, TIER - 1) / TIER;
  if (t === q) return { score: shorter, positions: run(0, q.length) };
  if (t.startsWith(q)) return { score: TIER + shorter, positions: run(0, q.length) };
  const word = atWordStart(q, t);
  if (word !== -1) return { score: 2 * TIER + shorter, positions: run(word, q.length) };
  const within = t.indexOf(q);
  if (within !== -1) return { score: 3 * TIER + shorter, positions: run(within, q.length) };
  const plain = lay(q, t, false);
  if (!plain) return null;
  const starts = lay(q, t, true);
  const best = starts && starts.score < plain.score ? starts : plain;
  return { score: 4 * TIER + Math.min(best.score, TIER - 2) + shorter, positions: best.positions };
}

/** `fuzzyMatch`'s score alone. */
export function fuzzyScore(query: string, text: string): number | null {
  return fuzzyMatch(query, text)?.score ?? null;
}

/**
 * The better of a match against what a row shows and one against everything
 * it is found by, so that a row's own name is not outranked by the words
 * listed after it.
 */
export function fuzzyMatchLabel(query: string, label: string, keywords?: string): FuzzyMatch | null {
  const own = fuzzyMatch(query, label);
  if (!keywords) return own;
  const all = fuzzyMatch(query, `${label} ${keywords}`);
  if (!own) return all;
  return all && all.score < own.score ? all : own;
}
