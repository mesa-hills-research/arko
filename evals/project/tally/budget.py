"""Monthly budgets: what each category may spend in a month, and what it
spent."""

from dataclasses import dataclass

from tally.dates import in_month
from tally.models import Transaction
from tally.rules import Rule, categorize


@dataclass(frozen=True)
class BudgetLine:
    category: str
    budget: int  # cents; 0 when the category has no budget
    spent: int  # cents, as a positive number

    @property
    def left(self) -> int:
        return self.budget - self.spent

    @property
    def over(self) -> bool:
        return self.budget > 0 and self.spent > self.budget


def spent_by_category(
    transactions: list[Transaction], month: str, rules: list[Rule]
) -> dict[str, int]:
    """What each category spent in `month`, in cents. Money received
    doesn't count."""
    spent: dict[str, int] = {}
    for transaction in transactions:
        if transaction.is_expense and in_month(transaction.day, month):
            category = categorize(transaction, rules)
            spent[category] = spent.get(category, 0) - transaction.amount
    return spent


def budget_lines(
    budgets: dict[str, int], transactions: list[Transaction], month: str, rules: list[Rule]
) -> list[BudgetLine]:
    """A line for each category with a budget or with spending in `month`:
    those with a budget in the settings' order, then the others by name."""
    spent = spent_by_category(transactions, month, rules)
    lines = [BudgetLine(category, budget, spent.get(category, 0)) for category, budget in budgets.items()]
    for category in sorted(set(spent) - set(budgets)):
        lines.append(BudgetLine(category, 0, spent[category]))
    return lines


def over_budget(lines: list[BudgetLine]) -> list[BudgetLine]:
    return [line for line in lines if line.over]
