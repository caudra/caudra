defmodule Inventory.Catalog do
  @moduledoc "Tracks stock levels for a small catalog."

  @behaviour Inventory.Store

  alias Inventory.Item
  require Logger

  @default_page_size 50

  defstruct items: %{}, page_size: @default_page_size

  defmacro __using__(_opts) do
    quote do
      alias Inventory.Catalog
    end
  end

  @doc "Builds an empty catalog."
  def new do
    %__MODULE__{}
  end

  def restock(%__MODULE__{items: items} = catalog, sku, amount) do
    updated = Map.update(items, normalize(sku), amount, &(&1 + amount))
    %{catalog | items: updated}
  end

  def total(%__MODULE__{items: items}) do
    items
    |> Map.values()
    |> Enum.sum()
  end

  def count(catalog), do: total(catalog)

  def label(sku) do
    sku
    |> normalize
    |> Item.render()
  end

  def describe(catalog) do
    case total(catalog) do
      0 -> Logger.info("empty catalog")
      units -> log_units(units)
    end
  end

  defp log_units(units) do
    Logger.info("units: #{units}")
  end

  defp normalize(sku) when is_binary(sku) do
    String.trim(sku)
  end
end

defprotocol Inventory.Sized do
  @doc "Returns the number of tracked units."
  def size(value)
end
