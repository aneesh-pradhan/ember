"""Invoices made of line items, with an optional discount and sales tax."""

from __future__ import annotations

from dataclasses import dataclass, field

from .money import round_money


@dataclass
class LineItem:
    description: str
    unit_price: float
    quantity: int = 1

    @property
    def amount(self) -> float:
        return round_money(self.unit_price * self.quantity)


@dataclass
class Invoice:
    tax_rate: float = 0.0
    discount_percent: float = 0.0
    items: list[LineItem] = field(default_factory=list)

    def add(self, description: str, unit_price: float, quantity: int = 1) -> LineItem:
        if quantity <= 0:
            raise ValueError("quantity must be positive")
        if unit_price < 0:
            raise ValueError("unit_price cannot be negative")
        item = LineItem(description, unit_price, quantity)
        self.items.append(item)
        return item

    def apply_discount(self, percent: float) -> None:
        if not 0 <= percent <= 100:
            raise ValueError("discount must be between 0 and 100 percent")
        self.discount_percent = percent

    def subtotal(self) -> float:
        return round_money(sum(item.amount for item in self.items))

    def discount(self) -> float:
        return round_money(self.subtotal() * self.discount_percent / 100)

    def tax(self) -> float:
        return round_money((self.subtotal() - self.discount()) * self.tax_rate)

    def total(self) -> float:
        return round_money(self.subtotal() - self.discount() + self.tax())

    def summary(self) -> dict[str, float]:
        return {
            "subtotal": self.subtotal(),
            "discount": self.discount(),
            "tax": self.tax(),
            "total": self.total(),
        }
