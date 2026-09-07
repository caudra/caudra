package com.example.inventory

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

const val DEFAULT_PAGE_SIZE = 50

typealias ItemId = String

interface ItemSource {
    fun load(id: ItemId): Item?
}

data class Item(val id: ItemId, val name: String, val quantity: Int) : Comparable<Item> {
    override fun compareTo(other: Item): Int = quantity.compareTo(other.quantity)
}

enum class Status {
    ACTIVE,
    RETIRED,
}

open class BaseRepository(protected val pageSize: Int) {
    open fun describe(): String = "repository(" + pageSize + ")"
}

class InventoryRepository(private val source: ItemSource) :
    BaseRepository(DEFAULT_PAGE_SIZE), ItemSource {
    private val cache = HashMap<ItemId, Item>()
    var status: Status = Status.ACTIVE

    override fun load(id: ItemId): Item? {
        val hit = cache[id]
        if (hit != null) {
            return hit
        }
        val loaded = source.load(id) ?: return null
        cache.put(id, loaded)
        return loaded
    }

    fun restock(id: ItemId, amount: Int): Item? {
        val current = load(id) ?: return null
        val updated = current.copy(quantity = current.quantity + amount)
        cache.put(id, updated)
        return updated
    }

    companion object Factory {
        fun empty(): InventoryRepository = InventoryRepository(EmptySource)
    }
}

object EmptySource : ItemSource {
    override fun load(id: ItemId): Item? = null
}

fun summarize(items: List<Item>): String =
    items.filter { it.quantity > 0 }.joinToString(", ") { it.name }

fun observe(source: Flow<Item>): Flow<String> = source.map { it.name }
