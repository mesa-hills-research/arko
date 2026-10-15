import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from tally.cli import main

SETTINGS = """
currency = "USD"
ledger = "ledger.json"

[budgets]
groceries = "100.00"
dining = "50.00"

[rules]
"FRESH MARKET" = "groceries"
"CAFE" = "dining"
"""

STATEMENT = """Date,Payee,Amount
2026-09-02,FRESH MARKET,-40.00
2026-09-03,CAFE LUNA,-62.50
2026-09-04,HARDWARE DEPOT,-10.00
2026-10-01,CAFE LUNA,-5.00
"""


class CliTest(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.folder = Path(folder.name)
        self.settings = self.folder / "settings.toml"
        self.settings.write_text(SETTINGS)
        self.ledger = self.folder / "ledger.json"
        (self.folder / "statement.csv").write_text(STATEMENT)

    def tally(self, *argv: str) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = main(["--settings", str(self.settings), "--ledger", str(self.ledger), *argv])
        return code, output.getvalue()

    def test_import_adds_the_statement_once(self):
        statement = str(self.folder / "statement.csv")
        self.assertEqual(self.tally("import", statement), (0, f"Added 4 of 4 transactions to {self.ledger}.\n"))
        self.assertEqual(self.tally("import", statement)[1], f"Added 0 of 4 transactions to {self.ledger}.\n")

    def test_report(self):
        self.tally("import", str(self.folder / "statement.csv"))
        code, text = self.tally("report", "--month", "2026-09")
        self.assertEqual(code, 0)
        self.assertIn("groceries      $100.00   $40.00   $60.00", text)
        self.assertIn("dining          $50.00   $62.50  -$12.50", text)

    def test_budget_names_the_categories_over(self):
        self.tally("import", str(self.folder / "statement.csv"))
        self.assertEqual(self.tally("budget", "--month", "2026-09"), (0, "dining: $12.50 over ($62.50 of $50.00)\n"))
        self.assertEqual(self.tally("budget", "--month", "2026-10"), (0, "No category is over budget in 2026-10.\n"))

    def test_a_bad_month_is_refused(self):
        code, text = self.tally("report", "--month", "2026-9-1")
        self.assertEqual(code, 2)
        self.assertIn("not a month", text)

    def test_a_missing_statement_is_an_error(self):
        code, text = self.tally("import", str(self.folder / "missing.csv"))
        self.assertEqual(code, 1)
        self.assertIn("tally:", text)


if __name__ == "__main__":
    unittest.main()
