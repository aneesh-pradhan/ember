"""Helpers for working with currency amounts."""


def round_money(amount: float) -> float:
    """Round an amount to whole cents."""
    return round(amount, 2)


def format_money(amount: float, symbol: str = "$") -> str:
    sign = "-" if amount < 0 else ""
    return f"{sign}{symbol}{abs(round_money(amount)):,.2f}"
