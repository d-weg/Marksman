<?php

declare(strict_types=1);

namespace Corpus;

class Rank
{
    public const RRF_K = 60.0;

    /**
     * Reciprocal-rank fusion of two ranked (id, score) lists.
     *
     * Each list contributes 1 / (RRF_K + rank + 1) to a document's fused
     * score, where rank is the document's zero-based position in that list.
     * The result is sorted by fused score, descending.
     *
     * @param list<array{0: int, 1: float}> $lexical
     * @param list<array{0: int, 1: float}> $semantic
     * @return list<array{0: int, 1: float}>
     */
    public static function blendScores(array $lexical, array $semantic): array
    {
        /** @var array<int, float> $fused */
        $fused = [];

        foreach ([$lexical, $semantic] as $ranking) {
            foreach ($ranking as $rank => $pair) {
                $id = $pair[0];
                $contribution = 1.0 / (self::RRF_K + $rank + 1);
                $fused[$id] = ($fused[$id] ?? 0.0) + $contribution;
            }
        }

        $results = [];
        foreach ($fused as $id => $score) {
            $results[] = [$id, $score];
        }

        usort($results, static fn (array $a, array $b): int => $b[1] <=> $a[1]);

        return $results;
    }
}
