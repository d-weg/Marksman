/** Reciprocal-rank fusion constant: rank dampening for the tail. */
export const RRF_K = 60;

/**
 * Blend two ranked lists [id, score] into one, reciprocal-rank style: each list
 * contributes 1/(RRF_K + rank), summed per id, highest first.
 */
export function blendScores(
  lexical: Array<[string, number]>,
  semantic: Array<[string, number]>,
): Array<[string, number]> {
  const fused = new Map<string, number>();
  lexical.forEach(([id], rank) => {
    fused.set(id, (fused.get(id) ?? 0) + 1 / (RRF_K + rank + 1));
  });
  semantic.forEach(([id], rank) => {
    fused.set(id, (fused.get(id) ?? 0) + 1 / (RRF_K + rank + 1));
  });
  return [...fused.entries()].sort((a, b) => b[1] - a[1]);
}
