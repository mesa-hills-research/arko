# tally

A small tracker for personal spending. It imports bank statements (CSV), gives each transaction a category by its payee, and shows what each category spent in a month against its budget.

```
python3 -m tally import data/sample.csv
python3 -m tally report --month 2026-09
python3 -m tally budget --month 2026-09
```

The settings in `config/settings.toml` set the currency, the budgets, and the rules that give payees their categories. The ledger (`ledger.json` by default) holds every transaction imported.

## Tests

```
python3 -m unittest
```

tally needs Python 3.11 or later, and nothing else.
