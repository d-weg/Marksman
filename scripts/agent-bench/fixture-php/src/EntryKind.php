<?php

declare(strict_types=1);

namespace Corpus;

enum EntryKind
{
    case Source;
    case Doc;
    case Config;
}
