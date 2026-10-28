Reject invalid tax rates: creating an `Invoice` with a `tax_rate` that is negative or greater than 1 must raise `ValueError`. Rates of exactly 0 and 1 are allowed.
