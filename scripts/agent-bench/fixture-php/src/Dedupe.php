<?php

declare(strict_types=1);

namespace Corpus;

class Dedupe
{
    /**
     * Collapse hits by path, keeping the first occurrence of each path.
     *
     * @param list<DocEntry> $hits
     * @return list<DocEntry>
     */
    public static function collapsePaths(array $hits): array
    {
        $seen = [];
        $out = [];
        foreach ($hits as $hit) {
            if (isset($seen[$hit->path])) {
                continue;
            }
            $seen[$hit->path] = true;
            $out[] = $hit;
        }

        return $out;
    }
}
