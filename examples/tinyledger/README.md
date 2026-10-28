# tinyledger

Small invoicing helpers used by FailFirst's demo. Line items, percentage discounts and
sales tax, with amounts reported in dollars rounded to cents.

```python
from tinyledger import Invoice

inv = Invoice(tax_rate=0.08)
inv.add("Widget", unit_price=19.99, quantity=3)
inv.apply_discount(10)  # percent
print(inv.total())
```
