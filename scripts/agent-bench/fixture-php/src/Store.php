<?php

declare(strict_types=1);

namespace Corpus;

class Store
{
    /**
     * @var list<DocEntry>
     */
    public array $docs = [];

    /**
     * Inverted index: token => list of document ids that contain it.
     *
     * @var array<string, list<int>>
     */
    private array $postings = [];

    /**
     * Index a document and return its assigned id.
     */
    public function add(string $name, string $path, EntryKind $kind, string $body): int
    {
        $id = count($this->docs);
        $tokens = Tokenize::tokenize($body);

        $this->docs[] = new DocEntry($name, $path, (float) count($tokens), $kind);

        foreach ($tokens as $token) {
            if (!isset($this->postings[$token])) {
                $this->postings[$token] = [];
            }
            $this->postings[$token][] = $id;
        }

        return $id;
    }

    /**
     * Look up a token and return (id, score) pairs, where score is the
     * term frequency of the token within each matching document.
     *
     * @return list<array{0: int, 1: float}>
     */
    public function lookup(string $token): array
    {
        $needle = Tokenize::normalize($token);
        $ids = $this->postings[$needle] ?? [];

        $counts = [];
        foreach ($ids as $id) {
            $counts[$id] = ($counts[$id] ?? 0) + 1;
        }

        $results = [];
        foreach ($counts as $id => $count) {
            $results[] = [$id, (float) $count];
        }

        return $results;
    }
}
