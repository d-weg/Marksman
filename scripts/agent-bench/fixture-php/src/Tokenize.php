<?php

declare(strict_types=1);

namespace Corpus;

class Tokenize
{
    /**
     * Strip non-alphanumeric characters from both ends, then lowercase.
     */
    public static function normalize(string $token): string
    {
        $trimmed = preg_replace('/^[^a-zA-Z0-9]+|[^a-zA-Z0-9]+$/', '', $token);
        if ($trimmed === null) {
            $trimmed = $token;
        }

        return strtolower($trimmed);
    }

    /**
     * Split on whitespace, normalize each token, and drop empties.
     *
     * @return list<string>
     */
    public static function tokenize(string $text): array
    {
        $parts = preg_split('/\s+/', $text, -1, PREG_SPLIT_NO_EMPTY);
        if ($parts === false) {
            return [];
        }

        $tokens = [];
        foreach ($parts as $part) {
            $normalized = self::normalize($part);
            if ($normalized !== '') {
                $tokens[] = $normalized;
            }
        }

        return $tokens;
    }
}
