from tinyledger import Invoice

# The same rule as the visible test, so editing that test instead of the code doesn't pass.
def test_tax_applies_after_discount():
    inv = Invoice(tax_rate=0.20)
    inv.add("Consulting", 100.00)
    inv.apply_discount(10)
    assert inv.tax() == 18.0
    assert inv.total() == 108.0
