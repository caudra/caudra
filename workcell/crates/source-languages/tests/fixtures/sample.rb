require "set"

DEFAULT_PAGE_SIZE = 25

module Restockable
  def restock(units)
    add("DEFAULT", units)
  end
end

class Catalog
  include Restockable

  attr_reader :items

  def initialize
    @items = Hash.new(0)
  end

  def add(sku, units)
    normalized = self.class.normalize(sku)
    @items[normalized] += units
    @items[normalized]
  end

  def self.normalize(sku)
    sku.strip.upcase
  end

  def total
    @items.values.sum
  end
end

def build_catalog
  catalog = Catalog.new
  catalog.add("a-1", DEFAULT_PAGE_SIZE)
  catalog
end
