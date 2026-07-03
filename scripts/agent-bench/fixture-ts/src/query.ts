import { collapsePaths } from "./dedupe.js";
import { blendScores } from "./rank.js";
import type { Store } from "./store.js";
import { tokenize } from "./tokenize.js";
import type { DocEntry } from "./types.js";

/**
 * Run a query: lexical lookups per token, blended with a (stub) semantic list,
 * deduped by path, top-N as fresh DocEntry hits carrying the fused score.
 */
export function search(store: Store, query: string, top: number): DocEntry[] {
  const lexical: Array<[string, number]> = [];
  for (const token of tokenize(query)) {
    lexical.push(...store.lookup(token));
  }
  // Semantic ranking is a stub: the doc table order stands in for vector scores.
  const semantic: Array<[string, number]> = store.docs.map((d, i) => [String(i), d.score]);

  const fused = blendScores(lexical, semantic);
  const hits: DocEntry[] = [];
  for (const [id, score] of fused.slice(0, top * 2)) {
    const doc = store.docs[Number(id)];
    if (!doc) continue;
    hits.push({
      name: doc.name,
      path: doc.path,
      score,
      kind: "source",
    });
  }
  return collapsePaths(hits).slice(0, top);
}
