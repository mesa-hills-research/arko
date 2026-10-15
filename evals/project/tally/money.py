"""Amounts of money. tally keeps every amount as a whole number of cents."""

from decimal import Decimal, InvalidOperation

SYMBOLS = {"USD": "$", "CAD": "$", "EUR": "€", "GBP": "£"}


def parse_amount(text: str) -> int:
    """Cents from an amount as banks write it: "1,234.56", "-12.30",
    "(12.30)" (negative), "$5"."""
    cleaned = text.strip().replace(",", "")
    negative = False
    if cleaned.startswith("(") and cleaned.endswith(")"):
        negative = True
        cleaned = cleaned[1:-1].strip()
    if cleaned.startswith("-"):
        negative = not negative
        cleaned = cleaned[1:].strip()
    for symbol in set(SYMBOLS.values()):
        cleaned = cleaned.replace(symbol, "")
    try:
        value = Decimal(cleaned)
    except InvalidOperation:
        raise ValueError(f"not an amount: {text!r}") from None
    cents = int((value * 100).quantize(Decimal("1")))
    return -cents if negative else cents


def format_amount(cents: int, currency: str = "USD") -> str:
    """An amount for people to read: 123456 is "$1,234.56"."""
    symbol = SYMBOLS.get(currency, currency + " ")
    sign = "-" if cents < 0 else ""
    cents = abs(cents)
    return f"{sign}{symbol}{cents // 100:,}.{cents % 100:02d}"


def split_evenly(cents: int, parts: int) -> list[int]:
    """`cents` shared out in `parts` amounts that differ by at most a cent,
    the larger ones first."""
    if parts < 1:
        raise ValueError("parts must be at least 1")
    share, rest = divmod(cents, parts)
    return [share + 1 if index < rest else share for index in range(parts)]
