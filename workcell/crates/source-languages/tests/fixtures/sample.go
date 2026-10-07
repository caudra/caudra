package catalog

import "strings"

const DefaultPageSize = 25

type Restockable interface {
	Restock(units uint32)
}

type Catalog struct {
	Items map[string]uint32
}

func NewCatalog() *Catalog {
	return &Catalog{Items: make(map[string]uint32)}
}

func (c *Catalog) Add(sku string, units uint32) uint32 {
	normalized := normalize(sku)
	c.Items[normalized] += units
	return c.Items[normalized]
}

func (c *Catalog) Restock(units uint32) {
	c.Add("DEFAULT", units)
}

func normalize(sku string) string {
	return strings.ToUpper(strings.TrimSpace(sku))
}

func BuildCatalog() *Catalog {
	catalog := NewCatalog()
	catalog.Add("a-1", DefaultPageSize)
	return catalog
}
