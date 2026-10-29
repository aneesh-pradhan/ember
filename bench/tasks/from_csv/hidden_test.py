import pytest

from tinyledger import Invoice

def test_rows_header_blank_lines_and_default_quantity():
    inv = Invoice.from_csv("description,unit_price,quantity\nWidget,2.50,2\n\nGadget,5\n", tax_rate=0.1)
    assert [(i.description, i.unit_price, i.quantity) for i in inv.items] == [("Widget", 2.5, 2), ("Gadget", 5.0, 1)]
    assert inv.tax_rate == 0.1 and inv.subtotal() == 10.0

def test_quoted_descriptions_with_commas():
    inv = Invoice.from_csv('"Cable, 2m",3.00,1\n')
    assert inv.items[0].description == "Cable, 2m"

def test_invalid_rows_name_the_line():
    with pytest.raises(ValueError, match="3"):
        Invoice.from_csv("Widget,2.50\nGadget,5\nBroken,abc\n")
    with pytest.raises(ValueError, match="2"):
        Invoice.from_csv("Widget,2.50\nFree,1.00,0\n")
