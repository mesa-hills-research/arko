"""The transactions tally keeps."""

from dataclasses import dataclass, replace
from datetime import date


@dataclass(frozen=True)
class Transaction:
    """One line of a bank statement. `amount` is in cents: negative for money
    spent, positive for money received. `category`, when set by hand, wins
    over the rules."""

    day: date
    payee: str
    amount: int
    category: str | None = None

    @property
    def is_expense(self) -> bool:
        return self.amount < 0

    def key(self) -> tuple:
        """What makes two imported lines the same transaction."""
        return (self.day, self.payee.strip().upper(), self.amount)

    def with_category(self, category: str | None) -> "Transaction":
        return replace(self, category=category)

    def to_dict(self) -> dict:
        data = {"day": self.day.isoformat(), "payee": self.payee, "amount": self.amount}
        if self.category:
            data["category"] = self.category
        return data

    @classmethod
    def from_dict(cls, data: dict) -> "Transaction":
        return cls(
            day=date.fromisoformat(data["day"]),
            payee=data["payee"],
            amount=int(data["amount"]),
            category=data.get("category"),
        )
