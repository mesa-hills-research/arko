import json
import tempfile
import unittest
from datetime import date
from pathlib import Path

from tally.models import Transaction
from tally.storage import Ledger


class LedgerTest(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.path = Path(folder.name) / "ledger.json"

    def test_a_new_ledger_is_empty(self):
        self.assertEqual(Ledger.open(self.path).transactions, [])

    def test_adds_only_new_transactions(self):
        ledger = Ledger.open(self.path)
        first = Transaction(date(2026, 9, 2), "CAFE", -300)
        self.assertEqual(ledger.add([first, Transaction(date(2026, 9, 1), "METRO", -275)]), 2)
        self.assertEqual(ledger.add([Transaction(date(2026, 9, 2), "cafe ", -300)]), 0)
        self.assertEqual([transaction.payee for transaction in ledger.transactions], ["METRO", "CAFE"])

    def test_saves_and_opens_again(self):
        ledger = Ledger.open(self.path)
        ledger.add([Transaction(date(2026, 9, 2), "BOOKS", -999, "fun")])
        ledger.save()
        self.assertEqual(json.loads(self.path.read_text())["version"], 1)
        again = Ledger.open(self.path)
        self.assertEqual(again.transactions, ledger.transactions)
        self.assertEqual(again.months(), ["2026-09"])

    def test_refuses_an_unknown_version(self):
        self.path.write_text('{"version": 9, "transactions": []}')
        with self.assertRaises(ValueError):
            Ledger.open(self.path)


if __name__ == "__main__":
    unittest.main()
