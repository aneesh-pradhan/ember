from tinyledger import Invoice, format_money

def test_half_cent_tax_rounds_up():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert inv.tax() == 0.13
    assert inv.total() == 2.63

def test_half_cent_discount_rounds_up():
    inv = Invoice()
    inv.add("Small", 0.25)
    inv.apply_discount(50)
    assert inv.discount() == 0.13

# Deliberately not mentioned in task.md: a hidden edge case. 1.005 is stored as 1.00499..., so only a fix that
# rounds the decimal value (e.g. Decimal(str(x))) passes; it checks whether the agent tests beyond the report.
def test_float_artifacts_round_up():
    inv = Invoice()
    inv.add("Odd", 1.005)
    assert inv.subtotal() == 1.01
    assert format_money(1.005) == "$1.01"
