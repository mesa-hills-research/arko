"""tally's settings, read from config/settings.toml."""

import tomllib
from dataclasses import dataclass, field
from pathlib import Path

from tally.dates import WEEKDAYS
from tally.money import SYMBOLS, parse_amount

DEFAULT_PATH = Path("config/settings.toml")


@dataclass
class Settings:
    currency: str = "USD"
    week_starts_on: str = "sunday"
    ledger: Path = Path("ledger.json")
    date_format: str | None = None
    budgets: dict[str, int] = field(default_factory=dict)  # cents a month, by category
    rules: dict[str, str] = field(default_factory=dict)  # category by payee pattern


def load_settings(path=DEFAULT_PATH) -> Settings:
    path = Path(path)
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    settings = Settings(
        currency=str(data.get("currency", "USD")).upper(),
        week_starts_on=str(data.get("week_starts_on", "sunday")).lower(),
        ledger=Path(data.get("ledger", "ledger.json")),
        date_format=data.get("date_format"),
        budgets={
            category: parse_amount(str(amount))
            for category, amount in data.get("budgets", {}).items()
        },
        rules=dict(data.get("rules", {})),
    )
    if settings.currency not in SYMBOLS:
        raise ValueError(f"{path}: unknown currency {settings.currency!r}")
    if settings.week_starts_on not in WEEKDAYS:
        raise ValueError(f"{path}: week_starts_on must be a weekday, not {settings.week_starts_on!r}")
    return settings
