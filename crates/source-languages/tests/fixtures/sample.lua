local DEFAULT_PAGE_SIZE = 25

local Catalog = {}
Catalog.__index = Catalog

function Catalog.new()
	return setmetatable({ items = {} }, Catalog)
end

local function normalize(sku)
	return string.upper(sku)
end

function Catalog:add(sku, units)
	local normalized = normalize(sku)
	self.items[normalized] = (self.items[normalized] or 0) + units
	return self.items[normalized]
end

function Catalog:restock(units)
	return self:add("DEFAULT", units)
end

local function build_catalog()
	local catalog = Catalog.new()
	catalog:add("a-1", DEFAULT_PAGE_SIZE)
	return catalog
end

return { Catalog = Catalog, build_catalog = build_catalog }
