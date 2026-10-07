<?php

namespace Example\Catalog;

use ArrayObject;

const DEFAULT_PAGE_SIZE = 25;

interface Restockable
{
    public function restock(int $units): void;
}

class Catalog implements Restockable
{
    private array $items = [];

    public function add(string $sku, int $units): int
    {
        $normalized = self::normalize($sku);
        $current = $this->items[$normalized] ?? 0;
        $this->items[$normalized] = $current + $units;
        return $current + $units;
    }

    private static function normalize(string $sku): string
    {
        return strtoupper(trim($sku));
    }

    public function restock(int $units): void
    {
        $this->add('DEFAULT', $units);
    }
}

function buildCatalog(): Catalog
{
    $catalog = new Catalog();
    $catalog->add('a-1', DEFAULT_PAGE_SIZE);
    return $catalog;
}
