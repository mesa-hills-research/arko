import unittest
from datetime import date

from tally.budget import BudgetLine, budget_lines, over_budget, spent_by_category
from tally.models import Transaction
from tally.rules import load_rules

RULES = load_rules({"FRESH MARKET": "groceries", "CAFE": "dining"})
BUDGETS = {"groceries": 20000, "dining": 5000}


def spend(day: date, payee: str, cents: int) -> Transaction:
    return Transaction(day, payee, -cents)


class BudgetTest(unittest.TestCase):
    def test_counts_only_the_month_s_spending(self):
        transactions = [
            spend(date(2026, 8, 31), "FRESH MARKET", 1000),
            spend(date(2026, 9, 1), "FRESH MARKET", 2000),
            spend(date(2026, 9, 30), "CAFE LUNA", 450),
            Transaction(date(2026, 9, 15), "ACME PAYROLL", 300000),
        ]
        self.assertEqual(
            spent_by_category(transactions, "2026-09", RULES),
            {"groceries": 2000, "dining": 450},
        )

    def test_lines_list_budgeted_categories_first(self):
        transactions = [
            spend(date(2026, 9, 3), "HARDWARE DEPOT", 2745),
            spend(date(2026, 9, 4), "CAFE LUNA", 6000),
        ]
        self.assertEqual(
            budget_lines(BUDGETS, transactions, "2026-09", RULES),
            [
                BudgetLine("groceries", 20000, 0),
                BudgetLine("dining", 5000, 6000),
                BudgetLine("uncategorized", 0, 2745),
            ],
        )

    def test_over_budget(self):
        lines = [BudgetLine("dining", 5000, 6000), BudgetLine("groceries", 20000, 100), BudgetLine("other", 0, 900)]
        self.assertEqual(over_budget(lines), [BudgetLine("dining", 5000, 6000)])
        self.assertEqual(lines[0].left, -1000)

    def test_december_spending_counts(self):
        transactions = [
            spend(date(2026, 12, 1), "FRESH MARKET", 4200),
            spend(date(2026, 12, 24), "CAFE LUNA", 1250),
            spend(date(2027, 1, 2), "CAFE LUNA", 800),
        ]
        lines = budget_lines(BUDGETS, transactions, "2026-12", RULES)
        self.assertEqual([line.spent for line in lines], [4200, 1250])


if __name__ == "__main__":
    unittest.main()
