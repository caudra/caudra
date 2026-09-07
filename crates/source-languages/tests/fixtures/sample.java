package com.example.catalog;

import java.util.HashMap;
import java.util.Map;

public class Catalog implements Restockable {
    public static final int DEFAULT_PAGE_SIZE = 25;

    private final Map<String, Integer> items = new HashMap<>();

    public int add(String sku, int units) {
        String normalized = normalize(sku);
        int current = items.getOrDefault(normalized, 0);
        items.put(normalized, current + units);
        return current + units;
    }

    private static String normalize(String sku) {
        return sku.trim().toUpperCase();
    }

    @Override
    public void restock(int units) {
        add("DEFAULT", units);
    }

    public static Catalog buildCatalog() {
        Catalog catalog = new Catalog();
        catalog.add("a-1", DEFAULT_PAGE_SIZE);
        return catalog;
    }
}
