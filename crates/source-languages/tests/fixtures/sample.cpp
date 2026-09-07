#include <map>
#include <string>

namespace catalog {

constexpr int kDefaultPageSize = 25;

class Restockable {
public:
    virtual ~Restockable() = default;
    virtual void restock(int units) = 0;
};

class Catalog : public Restockable {
public:
    int add(const std::string &sku, int units);
    void restock(int units) override;

private:
    static std::string normalize(const std::string &sku);
    std::map<std::string, int> items_;
};

std::string Catalog::normalize(const std::string &sku) {
    std::string copy = sku;
    return copy;
}

int Catalog::add(const std::string &sku, int units) {
    const std::string normalized = Catalog::normalize(sku);
    items_[normalized] += units;
    return items_[normalized];
}

void Catalog::restock(int units) {
    add("DEFAULT", units);
}

Catalog buildCatalog() {
    Catalog catalog;
    catalog.add("a-1", kDefaultPageSize);
    return catalog;
}

}  // namespace catalog
