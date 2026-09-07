import Foundation

let defaultPageSize = 25

protocol Restockable {
    func restock(units: Int)
}

struct Entry {
    let sku: String
    var units: Int
}

class Catalog: Restockable {
    var items: [String: Int] = [:]

    func add(sku: String, units: Int) -> Int {
        let normalized = Catalog.normalize(sku: sku)
        let current = items[normalized] ?? 0
        items[normalized] = current + units
        return current + units
    }

    static func normalize(sku: String) -> String {
        return sku.trimmingCharacters(in: .whitespaces).uppercased()
    }

    func restock(units: Int) {
        _ = add(sku: "DEFAULT", units: units)
    }
}

func buildCatalog() -> Catalog {
    let catalog = Catalog()
    _ = catalog.add(sku: "a-1", units: defaultPageSize)
    return catalog
}
