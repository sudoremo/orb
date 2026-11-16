import { expect, test } from "@playwright/test";
import { fuzzyMatch, fuzzyMatchLabel } from "../src/fuzzy";

/** The texts that match `query`, best first. */
function ranked(query: string, texts: string[]): string[] {
  return texts
    .map((text) => ({ text, match: fuzzyMatch(query, text) }))
    .filter((m) => m.match !== null)
    .sort((a, b) => a.match!.score - b.match!.score)
    .map((m) => m.text);
}

test("the text that is what was typed comes before the one whose words start with its letters", () => {
  expect(ranked("GSI", ["Garden Seedling Inventory", "GSI"])).toEqual(["GSI", "Garden Seedling Inventory"]);
  expect(ranked("gsi", ["Garden Seedling Inventory", "GSI"])).toEqual(["GSI", "Garden Seedling Inventory"]);
});

test("whole, then at the start, then at a word's start, then inside, then letters in order", () => {
  const texts = ["Seedling Inventory", "Inventory list", "Inventory", "Preinventory", "Ignore No Vents"];
  expect(ranked("inv", texts)).toEqual(["Inventory", "Inventory list", "Seedling Inventory", "Preinventory", "Ignore No Vents"]);
});

test("among letters in order, word starts and runs beat letters scattered about", () => {
  expect(ranked("wrk", ["Network", "Work"])).toEqual(["Work", "Network"]);
  expect(ranked("ar", ["Mailing Lists/Hardware", "Archive"])).toEqual(["Archive", "Mailing Lists/Hardware"]);
  expect(ranked("clsl", ["close-all", "clear-selection"])).toEqual(["clear-selection", "close-all"]);
  expect(ranked("cs", ["clear-selection", "chat-search", "clips"])).toEqual(["chat-search", "clear-selection", "clips"]);
});

test("the positions are the letters that matched", () => {
  expect(fuzzyMatch("inv", "Seedling Inventory")!.positions).toEqual([9, 10, 11]);
  expect(fuzzyMatch("gsi", "Garden Seedling Inventory")!.positions).toEqual([0, 7, 16]);
  expect(fuzzyMatch("xyz", "Spring")).toBeNull();
  expect(fuzzyMatch("", "anything")!.positions).toEqual([]);
});

test("a row's own name is not outranked by the words it is found by", () => {
  const own = fuzzyMatchLabel("gsi", "GSI", "Garden Seedling Inventory")!;
  const other = fuzzyMatchLabel("gsi", "Garden Seedling Inventory")!;
  expect(own.score).toBeLessThan(other.score);
  expect(fuzzyMatchLabel("mark", "Archive", "mark")).not.toBeNull();
});
