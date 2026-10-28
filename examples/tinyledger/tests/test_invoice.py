import pytest

from tinyledger import Invoice, format_money


def test_empty_invoice_totals_zero():
    assert Invoice().total() == 0


def test_line_items_sum_into_subtotal():
    inv = Invoice()
    inv.add("Widget", 19.99, 3)
    inv.add("Gadget", 5.00)
    assert inv.subtotal() == 64.97


def test_discount_is_taken_before_tax():
    inv = Invoice(tax_rate=0.10)
    inv.add("Service", 200.00)
    inv.apply_discount(25)
    assert inv.summary() == {"subtotal": 200.0, "discount": 50.0, "tax": 15.0, "total": 165.0}


def test_rejects_invalid_quantity_and_discount():
    inv = Invoice()
    with pytest.raises(ValueError):
        inv.add("Nothing", 1.00, 0)
    with pytest.raises(ValueError):
        inv.apply_discount(120)


def test_format_money():
    assert format_money(1234.5) == "$1,234.50"
    assert format_money(-3) == "-$3.00"
