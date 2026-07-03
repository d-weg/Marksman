import type { DocEntry } from "./types.js";

/** Keep the best-scoring hit per path (input must already be sorted best-first). */
export function collapsePaths(hits: DocEntry[]): DocEntry[] {
  const seen = new Set<string>();
  return hits.filter((h) => {
    if (seen.has(h.path)) return false;
    seen.add(h.path);
    return true;
  });
}
