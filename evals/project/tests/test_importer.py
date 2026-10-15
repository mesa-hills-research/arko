import tempfile
import unittest
from datetime import date
from pathlib import Path

from tally.importer import StatementError, read_statement
from tally.models import Transaction


class ImporterTest(unittest.TestCase):
    def statement(self, text: str) -> Path:
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        path = Path(folder.name) / "statement.csv"
        path.write_text(text, encoding="utf-8")
        return path

    def test_reads_each_line(self):
        path = self.statement(
            "Date,Payee,Amount\n2026-09-02,FRESH MARKET,-12.40\n2026-09-03,ACME PAYROLL,\"1,000.00\"\n"
        )
        self.assertEqual(
            read_statement(path),
            [
                Transaction(date(2026, 9, 2), "FRESH MARKET", -1240),
                Transaction(date(2026, 9, 3), "ACME PAYROLL", 100000),
            ],
        )

    def test_keeps_a_category_set_by_hand(self):
        path = self.statement("Date,Payee,Amount,Category\n2026-09-20,BOOKS,-9.99,fun\n2026-09-21,BOOKS,-1.00,\n")
        first, second = read_statement(path)
        self.assertEqual(first.category, "fun")
        self.assertIsNone(second.category)

    def test_skips_blank_lines(self):
        path = self.statement("Date,Payee,Amount\n\n2026-09-02,CAFE,-3.00\n,,\n")
        self.assertEqual(len(read_statement(path)), 1)

    def test_a_date_format_can_be_given(self):
        path = self.statement("Date,Payee,Amount\n02.09.2026,CAFE,-3.00\n")
        self.assertEqual(read_statement(path, "%d.%m.%Y")[0].day, date(2026, 9, 2))

    def test_says_which_line_is_wrong(self):
        path = self.statement("Date,Payee,Amount\n2026-09-02,CAFE,-3.00\n2026-09-03,CAFE,three\n")
        with self.assertRaisesRegex(StatementError, "line 3"):
            read_statement(path)

    def test_needs_the_columns(self):
        path = self.statement("Day,Payee,Amount\n2026-09-02,CAFE,-3.00\n")
        with self.assertRaisesRegex(StatementError, "Date"):
            read_statement(path)


if __name__ == "__main__":
    unittest.main()
