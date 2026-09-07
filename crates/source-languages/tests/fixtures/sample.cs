using System.Collections.Generic;

namespace Example.Catalog
{
    public interface IRestockable
    {
        void Restock(int units);
    }

    public class Catalog : IRestockable
    {
        public const int DefaultPageSize = 25;

        private readonly Dictionary<string, int> items = new Dictionary<string, int>();

        public int Add(string sku, int units)
        {
            var normalized = Normalize(sku);
            items.TryGetValue(normalized, out var current);
            items[normalized] = current + units;
            return current + units;
        }

        private static string Normalize(string sku)
        {
            return sku.Trim().ToUpperInvariant();
        }

        public void Restock(int units)
        {
            Add("DEFAULT", units);
        }

        public static Catalog BuildCatalog()
        {
            var catalog = new Catalog();
            catalog.Add("a-1", DefaultPageSize);
            return catalog;
        }
    }
}
