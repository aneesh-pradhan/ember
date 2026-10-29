from tinyledger import Customer, Invoice
from tinyledger.customer import Customer as FromModule

def test_customer_module_and_export():
    assert Customer is FromModule
    c = Customer("Acme")
    assert c.name == "Acme" and c.tax_exempt is False

def test_tax_exempt_customer_pays_no_tax():
    inv = Invoice(tax_rate=0.10, customer=Customer("Charity", tax_exempt=True))
    inv.add("Books", 40.00)
    assert inv.tax() == 0
    assert inv.total() == 40.0

def test_regular_customer_and_no_customer_pay_tax():
    for customer in (Customer("Acme"), None):
        inv = Invoice(tax_rate=0.10, customer=customer)
        inv.add("Books", 40.00)
        assert inv.tax() == 4.0
        assert inv.total() == 44.0
