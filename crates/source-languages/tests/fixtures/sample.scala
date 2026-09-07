package com.example.pricing

import scala.collection.mutable
import scala.util.{Failure, Success, Try}

trait PriceSource {
  val currency: String

  def lookup(sku: String): Option[BigDecimal]
}

enum Status {
  case Active, Retired
}

case class LineItem(sku: String, quantity: Int, unitPrice: BigDecimal, status: Status) {
  def total: BigDecimal = unitPrice * quantity
}

class Catalog(private val prices: Map[String, BigDecimal]) extends PriceSource {
  override val currency: String = Catalog.DefaultCurrency

  private val hits = mutable.Map.empty[String, Int]

  override def lookup(sku: String): Option[BigDecimal] = {
    hits.update(sku, hits.getOrElse(sku, 0) + 1)
    prices.get(sku)
  }

  def priceOf(item: LineItem): Try[BigDecimal] =
    lookup(item.sku) match {
      case Some(price) => Success(price * item.quantity)
      case None => Failure(new NoSuchElementException(item.sku))
    }
}

object Catalog {
  val DefaultCurrency: String = "USD"

  type Money = BigDecimal

  def empty: Catalog = new Catalog(Map.empty)

  def subtotal(items: Seq[LineItem]): Money =
    items.filter(_.status == Status.Active).map(_.total).foldLeft(BigDecimal(0))(_ + _)
}
