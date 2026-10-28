from tinyledger import Invoice

def test_to_dict():
    inv = Invoice(tax_rate=0.10)
    inv.add("Widget", 19.99, 3)
    inv.apply_discount(10)
    d = inv.to_dict()
    assert d["items"] == [{"description": "Widget", "unit_price": 19.99, "quantity": 3, "amount": 59.97}]
    for key in ("subtotal", "discount", "tax", "total"):
        assert d[key] == getattr(inv, key)()
