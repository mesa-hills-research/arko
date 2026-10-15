import unittest

from tally.budget import BudgetLine
from tally.report import monthly_report, report_rows


class ReportTest(unittest.TestCase):
    def setUp(self):
        self.lines = [
            BudgetLine("groceries", 45000, 42698),
            BudgetLine("dining", 20000, 11030),
            BudgetLine("uncategorized", 0, 2745),
        ]

    def test_rows_end_with_the_total(self):
        rows = report_rows(self.lines, "USD")
        self.assertEqual(rows[0], ("groceries", "$450.00", "$426.98", "$23.02"))
        self.assertEqual(rows[2], ("uncategorized", "-", "$27.45", "-"))
        self.assertEqual(rows[-1], ("Total", "$650.00", "$564.73", "$85.27"))

    def test_report_is_a_table(self):
        text = monthly_report(self.lines, "2026-09", "EUR")
        expected = "\n".join(
            [
                "Spending in 2026-09",
                "",
                "Category        Budget    Spent    Left",
                "-------------  -------  -------  ------",
                "groceries      €450.00  €426.98  €23.02",
                "dining         €200.00  €110.30  €89.70",
                "uncategorized        -   €27.45       -",
                "-------------  -------  -------  ------",
                "Total          €650.00  €564.73  €85.27",
            ]
        )
        self.assertEqual(text, expected)


if __name__ == "__main__":
    unittest.main()
