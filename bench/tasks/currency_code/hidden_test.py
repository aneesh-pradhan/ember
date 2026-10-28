from tinyledger import format_money

def test_currency_code():
    assert format_money(1234.5, currency_code="EUR") == "1,234.50 EUR"
    assert format_money(-3, currency_code="EUR") == "-3.00 EUR"

def test_symbol_unchanged():
    assert format_money(1234.5) == "$1,234.50"
    assert format_money(2, symbol="£") == "£2.00"
