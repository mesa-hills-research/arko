"""The command line: python -m tally import|report|budget."""

import argparse
import sys
from datetime import date

from tally.budget import budget_lines, over_budget
from tally.config import DEFAULT_PATH, load_settings
from tally.dates import month_key, parse_month
from tally.importer import StatementError, read_statement
from tally.money import format_amount
from tally.report import monthly_report
from tally.rules import load_rules
from tally.storage import Ledger


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="tally", description="Track spending against monthly budgets.")
    parser.add_argument("--settings", default=str(DEFAULT_PATH), help="the settings file (default: %(default)s)")
    parser.add_argument("--ledger", help="the ledger file (default: the one the settings name)")
    commands = parser.add_subparsers(dest="command", required=True)

    importing = commands.add_parser("import", help="add a bank statement (CSV) to the ledger")
    importing.add_argument("statement", help="the CSV file")

    report = commands.add_parser("report", help="what each category spent in a month")
    report.add_argument("--month", help="YYYY-MM (default: this month)")

    budget = commands.add_parser("budget", help="the categories over budget in a month")
    budget.add_argument("--month", help="YYYY-MM (default: this month)")
    return parser


def main(argv=None, today: date | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        settings = load_settings(args.settings)
        ledger = Ledger.open(args.ledger or settings.ledger)
    except (OSError, ValueError) as error:
        print(f"tally: {error}", file=sys.stderr)
        return 1

    if args.command == "import":
        try:
            transactions = read_statement(args.statement, settings.date_format)
        except (OSError, StatementError) as error:
            print(f"tally: {error}", file=sys.stderr)
            return 1
        added = ledger.add(transactions)
        ledger.save()
        print(f"Added {added} of {len(transactions)} transactions to {ledger.path}.")
        return 0

    month = args.month or month_key(today or date.today())
    try:
        parse_month(month)
    except ValueError as error:
        print(f"tally: {error}", file=sys.stderr)
        return 2
    lines = budget_lines(settings.budgets, ledger.transactions, month, load_rules(settings.rules))

    if args.command == "report":
        print(monthly_report(lines, month, settings.currency))
        return 0

    over = over_budget(lines)
    if not over:
        print(f"No category is over budget in {month}.")
        return 0
    for line in over:
        print(
            f"{line.category}: {format_amount(-line.left, settings.currency)} over"
            f" ({format_amount(line.spent, settings.currency)} of {format_amount(line.budget, settings.currency)})"
        )
    return 0
