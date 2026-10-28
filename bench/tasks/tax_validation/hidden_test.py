import pytest
from tinyledger import Invoice

@pytest.mark.parametrize("rate", [-0.01, 1.01, 5])
def test_rejects_invalid_rates(rate):
    with pytest.raises(ValueError):
        Invoice(tax_rate=rate)

@pytest.mark.parametrize("rate", [0, 0.2, 1])
def test_accepts_valid_rates(rate):
    assert Invoice(tax_rate=rate).tax_rate == rate
