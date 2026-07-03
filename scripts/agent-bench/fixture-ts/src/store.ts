import { tokenize } from "./tokenize.js";
import type { DocEntry, EntryKind } from "./types.js";

/** The in-memory index: token -> doc ids, plus the doc table itself. */
export class Store {
  docs: DocEntry[] = [];
  private postings = new Map<string, number[]>();

  /** Index one document. The id is its position in the doc table. */
  add(name: string, path: string, kind: EntryKind, body: string): number {
    const id = this.docs.length;
    this.docs.push({
      name,
      path,
      score: 0,
      kind,
    });
    for (const token of tokenize(body)) {
      const ids = this.postings.get(token) ?? [];
      ids.push(id);
      this.postings.set(token, ids);
    }
    return id;
  }

  /** Doc ids whose body contained `token`, lexical hit count as the score. */
  lookup(token: string): Array<[string, number]> {
    const ids = this.postings.get(token);
    if (!ids) return [];
    const counts = new Map<string, number>();
    for (const id of ids) {
      const key = String(id);
      counts.set(key, (counts.get(key) ?? 0) + 1);
    }
    return [...counts.entries()];
  }
}
