"""The ledger: every transaction imported, kept in a JSON file."""

import json
from pathlib import Path

from tally.dates import month_key
from tally.models import Transaction

VERSION = 1


class Ledger:
    def __init__(self, path):
        self.path = Path(path)
        self.transactions: list[Transaction] = []

    @classmethod
    def open(cls, path) -> "Ledger":
        """The ledger at `path`, empty when there is no file yet."""
        ledger = cls(path)
        if ledger.path.exists():
            data = json.loads(ledger.path.read_text(encoding="utf-8"))
            if data.get("version") != VERSION:
                raise ValueError(f"{ledger.path}: unknown ledger version {data.get('version')!r}")
            ledger.transactions = [Transaction.from_dict(item) for item in data["transactions"]]
        return ledger

    def add(self, transactions: list[Transaction]) -> int:
        """Adds the transactions the ledger doesn't have yet, and says how
        many that was."""
        known = {transaction.key() for transaction in self.transactions}
        added = 0
        for transaction in transactions:
            if transaction.key() not in known:
                self.transactions.append(transaction)
                known.add(transaction.key())
                added += 1
        self.transactions.sort(key=lambda transaction: (transaction.day, transaction.payee))
        return added

    def months(self) -> list[str]:
        return sorted({month_key(transaction.day) for transaction in self.transactions})

    def save(self) -> None:
        data = {
            "version": VERSION,
            "transactions": [transaction.to_dict() for transaction in self.transactions],
        }
        temporary = self.path.with_suffix(".tmp")
        temporary.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        temporary.replace(self.path)
