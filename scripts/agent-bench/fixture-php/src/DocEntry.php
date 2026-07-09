<?php

declare(strict_types=1);

namespace Corpus;

class DocEntry
{
    public function __construct(
        public string $name,
        public string $path,
        public float $score,
        public EntryKind $kind,
    ) {
    }

    public function display(): string
    {
        return "{$this->name} ({$this->path})";
    }
}
