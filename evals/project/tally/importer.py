"""Reading bank statements exported as CSV, with the columns Date, Payee,
Amount and, optionally, Category."""

import csv
from pathlib import Path

from tally.dates import DEFAULT_FORMATS, parse_date
from tally.models import Transaction
from tally.money import parse_amount

REQUIRED = ("Date", "Payee", "Amount")


class StatementError(ValueError):
    """A statement tally can't read, with the line it failed on."""


def read_statement(path, date_format: str | None = None) -> list[Transaction]:
    formats = (date_format,) if date_format else DEFAULT_FORMATS
    with Path(path).open(newline="", encoding="utf-8-sig") as handle:
        reader = csv.DictReader(handle)
        missing = [column for column in REQUIRED if column not in (reader.fieldnames or [])]
        if missing:
            raise StatementError(f"{path}: missing column(s) {', '.join(missing)}")
        transactions = []
        for line, row in enumerate(reader, start=2):
            if not any((value or "").strip() for value in row.values()):
                continue
            try:
                transactions.append(
                    Transaction(
                        day=parse_date(row["Date"], formats),
                        payee=row["Payee"].strip(),
                        amount=parse_amount(row["Amount"]),
                        category=(row.get("Category") or "").strip() or None,
                    )
                )
            except ValueError as error:
                raise StatementError(f"{path}, line {line}: {error}") from None
    return transactions
