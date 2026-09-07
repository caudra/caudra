import { readFile } from "node:fs/promises";

export const DEFAULT_PAGE_SIZE = 25;

export interface Restockable {
  restock(units: number): void;
}

export class Catalog implements Restockable {
  items: Map<string, number> = new Map();

  add(sku: string, units: number): number {
    const normalized = Catalog.normalize(sku);
    const current = this.items.get(normalized) ?? 0;
    this.items.set(normalized, current + units);
    return current + units;
  }

  static normalize(sku: string): string {
    return sku.trim().toUpperCase();
  }

  restock(units: number): void {
    this.add("DEFAULT", units);
  }
}

export function buildCatalog(): Catalog {
  const catalog = new Catalog();
  catalog.add("a-1", DEFAULT_PAGE_SIZE);
  return catalog;
}
