"""The monthly report, as a text table."""

from tally.budget import BudgetLine
from tally.money import format_amount

HEADER = ("Category", "Budget", "Spent", "Left")


def report_rows(lines: list[BudgetLine], currency: str) -> list[tuple[str, str, str, str]]:
    """The table's rows: one per line, then the total. A category without a
    budget shows "-" for its budget and what it has left."""
    rows = []
    for line in lines:
        budgeted = line.budget > 0
        rows.append(
            (
                line.category,
                format_amount(line.budget, currency) if budgeted else "-",
                format_amount(line.spent, currency),
                format_amount(line.left, currency) if budgeted else "-",
            )
        )
    budget = sum(line.budget for line in lines)
    spent = sum(line.spent for line in lines)
    rows.append(
        (
            "Total",
            format_amount(budget, currency),
            format_amount(spent, currency),
            format_amount(budget - spent, currency),
        )
    )
    return rows


def monthly_report(lines: list[BudgetLine], month: str, currency: str) -> str:
    rows = report_rows(lines, currency)
    widths = [max(len(row[column]) for row in [HEADER, *rows]) for column in range(len(HEADER))]

    def render(row) -> str:
        cells = [row[0].ljust(widths[0])]
        cells.extend(cell.rjust(width) for cell, width in zip(row[1:], widths[1:]))
        return "  ".join(cells).rstrip()

    rule = "  ".join("-" * width for width in widths)
    text = [f"Spending in {month}", "", render(HEADER), rule]
    text.extend(render(row) for row in rows[:-1])
    text.extend([rule, render(rows[-1])])
    return "\n".join(text)
