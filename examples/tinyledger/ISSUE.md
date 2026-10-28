Invoice totals are a cent short when tax lands on a half cent

We bill a $2.50 item with 5% sales tax. The tax is $0.125, which should round to $0.13
(half a cent rounds up, which is how our POS and every receipt we compare against do it),
so the total should be $2.63. tinyledger gives tax 0.12 and total 2.62.

```python
from tinyledger import Invoice
inv = Invoice(tax_rate=0.05)
inv.add("Sticker pack", 2.50)
print(inv.summary())  # {'subtotal': 2.5, 'discount': 0.0, 'tax': 0.12, 'total': 2.62}
```

It also happens without tax: a 50% discount on a $0.25 item shows a discount of 0.12 instead
of 0.13, and some prices like $1.005 x 1 come out as 1.0 instead of 1.01. Our accountant
caught a few hundred of these at month end.
