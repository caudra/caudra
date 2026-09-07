#include <string.h>

#define DEFAULT_PAGE_SIZE 25

typedef struct {
    char sku[32];
    unsigned int units;
} entry_t;

struct catalog {
    entry_t entries[64];
    unsigned int count;
};

static unsigned int normalize_units(unsigned int units) {
    return units > 0u ? units : 1u;
}

unsigned int catalog_add(struct catalog *catalog, const char *sku, unsigned int units) {
    unsigned int normalized = normalize_units(units);
    strncpy(catalog->entries[catalog->count].sku, sku, sizeof(catalog->entries[0].sku) - 1);
    catalog->entries[catalog->count].units = normalized;
    catalog->count += 1u;
    return normalized;
}

unsigned int catalog_build(struct catalog *catalog) {
    return catalog_add(catalog, "a-1", DEFAULT_PAGE_SIZE);
}
