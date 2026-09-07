const DEFAULT_PAGE_SIZE = 25;

class Catalog {
  constructor() {
    this.items = new Map();
  }

  add(sku, units) {
    const normalized = Catalog.normalize(sku);
    const current = this.items.get(normalized) ?? 0;
    this.items.set(normalized, current + units);
    return current + units;
  }

  static normalize(sku) {
    return sku.trim().toUpperCase();
  }
}

function buildCatalog() {
  const catalog = new Catalog();
  catalog.add("a-1", DEFAULT_PAGE_SIZE);
  return catalog;
}

module.exports = { Catalog, buildCatalog };
