/** What kind of document a hit points at. */
export type EntryKind = "source" | "doc" | "config";

/** One indexed document: the unit every stage passes around. */
export interface DocEntry {
  name: string;
  path: string;
  score: number;
  kind: EntryKind;
}

export function display(entry: DocEntry): string {
  return `${entry.name} (${entry.path})`;
}
