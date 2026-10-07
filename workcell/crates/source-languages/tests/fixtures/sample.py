"""Catalog inventory tracking."""
import collections

DEFAULT_PAGE_SIZE = 25


class Catalog:
    """Tracks stock levels per SKU."""

    def __init__(self):
        self.items = collections.defaultdict(int)

    def add(self, sku, units):
        normalized = self.normalize(sku)
        self.items[normalized] += units
        return self.items[normalized]

    @staticmethod
    def normalize(sku):
        return sku.strip().upper()

    def total(self):
        return sum(self.items.values())


def build_catalog():
    catalog = Catalog()
    catalog.add("a-1", DEFAULT_PAGE_SIZE)
    return catalog
