"""Categories by payee: a rule gives its category to every payee that
contains its pattern, whatever the case."""

from dataclasses import dataclass

from tally.models import Transaction

UNCATEGORIZED = "uncategorized"


@dataclass(frozen=True)
class Rule:
    pattern: str
    category: str

    def matches(self, payee: str) -> bool:
        return self.pattern.casefold() in payee.casefold()


def load_rules(table: dict[str, str]) -> list[Rule]:
    """Rules from the settings' [rules] table, in the order written there."""
    return [Rule(pattern, category) for pattern, category in table.items()]


def best_rule(rules: list[Rule], payee: str) -> Rule | None:
    best = None
    for rule in rules:
        if rule.matches(payee) and (best is None or len(rule.pattern) > len(best.pattern)):
            best = rule
    return best


def categorize(transaction: Transaction, rules: list[Rule]) -> str:
    """A transaction's category: the one set by hand, else its payee's rule."""
    if transaction.category:
        return transaction.category
    rule = best_rule(rules, transaction.payee)
    return rule.category if rule else UNCATEGORIZED
