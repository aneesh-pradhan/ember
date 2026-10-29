import pathlib
import warnings

import pytest

from tinyledger import Invoice

ROOT = pathlib.Path(__file__).resolve().parent.parent

def test_add_item_adds_and_validates():
    inv = Invoice()
    item = inv.add_item("Widget", 2.50, 2)
    assert item.amount == 5.0 and inv.subtotal() == 5.0
    with pytest.raises(ValueError):
        inv.add_item("Nothing", 1.00, 0)

def test_add_is_a_deprecated_alias():
    inv = Invoice()
    with pytest.warns(DeprecationWarning):
        inv.add("Widget", 2.50, 2)
    assert inv.subtotal() == 5.0

def test_renamed_everywhere():
    with warnings.catch_warnings():
        warnings.simplefilter("error", DeprecationWarning)
        inv = Invoice()
        inv.add_item("Widget", 1.00)
    for path in [*ROOT.glob("src/**/*.py"), ROOT / "tests" / "test_invoice.py", ROOT / "README.md"]:
        text = path.read_text()
        calls = [line for line in text.splitlines() if ".add(" in line]
        assert not calls, f"{path.name} still calls .add(: {calls}"
