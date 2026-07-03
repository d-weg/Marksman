/**
 * Lowercase a raw token and strip surrounding punctuation. The index and the
 * query side must agree on this, or nothing matches.
 */
export function normalize(token: string): string {
  return token.replace(/^[^a-zA-Z0-9]+|[^a-zA-Z0-9]+$/g, "").toLowerCase();
}

/** Split source text into normalized tokens (empty tokens dropped). */
export function tokenize(text: string): string[] {
  return text
    .split(/\s+/)
    .map(normalize)
    .filter((t) => t.length > 0);
}
