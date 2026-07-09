<?php

declare(strict_types=1);

namespace Corpus;

class Query
{
    /**
     * Search the store for a query string and return the top-N hits.
     *
     * Lexical scores come from token lookups; the semantic channel is a
     * stub that simply enumerates the stored documents. Both channels are
     * fused with reciprocal-rank fusion, truncated, materialized as
     * DocEntry values, de-duplicated by path, and finally trimmed to $top.
     *
     * @return list<DocEntry>
     */
    public static function search(Store $store, string $query, int $top): array
    {
        // Lexical channel: accumulate (id, score) pairs across query tokens.
        $lexical = [];
        foreach (Tokenize::tokenize($query) as $token) {
            foreach ($store->lookup($token) as $pair) {
                $lexical[] = $pair;
            }
        }

        // Semantic channel (stub): enumerate the stored documents.
        $semantic = [];
        foreach ($store->docs as $id => $doc) {
            $semantic[] = [$id, $doc->score];
        }

        $fused = Rank::blendScores($lexical, $semantic);

        $limit = $top * 2;
        $truncated = array_slice($fused, 0, $limit);

        $hits = [];
        foreach ($truncated as $pair) {
            $id = $pair[0];
            $score = $pair[1];
            $doc = $store->docs[$id] ?? null;
            if ($doc === null) {
                continue;
            }
            $hits[] = new DocEntry($doc->name, $doc->path, $score, EntryKind::Source);
        }

        $collapsed = Dedupe::collapsePaths($hits);

        return array_slice($collapsed, 0, $top);
    }
}
