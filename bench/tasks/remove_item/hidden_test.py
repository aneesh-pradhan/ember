import pytest
from tinyledger import Invoice

def test_remove_existing_item():
    inv = Invoice()
    inv.add("A", 10.0)
    inv.add("B", 5.0)
    inv.remove("A")
    assert [i.description for i in inv.items] == ["B"]
    assert inv.subtotal() == 5.0

def test_remove_missing_item_raises_keyerror():
    inv = Invoice()
    inv.add("A", 10.0)
    with pytest.raises(KeyError):
        inv.remove("Z")
