"""Dates and months. A month is named by its key, such as "2026-09"."""

from datetime import date, datetime, timedelta

DEFAULT_FORMATS = ("%Y-%m-%d", "%m/%d/%Y", "%d.%m.%Y")
WEEKDAYS = ("monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday")


def parse_date(text: str, formats=DEFAULT_FORMATS) -> date:
    text = text.strip()
    for fmt in formats:
        try:
            return datetime.strptime(text, fmt).date()
        except ValueError:
            continue
    raise ValueError(f"not a date: {text!r}")


def month_key(day: date) -> str:
    return f"{day.year:04d}-{day.month:02d}"


def parse_month(key: str) -> tuple[int, int]:
    try:
        year, month = (int(part) for part in key.split("-"))
    except ValueError:
        raise ValueError(f"not a month: {key!r} (use YYYY-MM)") from None
    if not 1 <= month <= 12:
        raise ValueError(f"not a month: {key!r} (use YYYY-MM)")
    return year, month


def month_range(key: str) -> tuple[date, date]:
    """The first and the last day of the month `key`."""
    year, month = parse_month(key)
    first = date(year, month, 1)
    next_first = date(year + month // 12, month % 12 + 1, 1)
    return first, next_first - timedelta(days=1)


def in_month(day: date, key: str) -> bool:
    first, last = month_range(key)
    return first <= day <= last


def previous_month(key: str) -> str:
    year, month = parse_month(key)
    if month == 1:
        return f"{year - 1:04d}-12"
    return f"{year:04d}-{month - 1:02d}"


def week_start(day: date, starts_on: str = "sunday") -> date:
    """The first day of the week `day` is in, for weeks that start on
    `starts_on`."""
    offset = (day.weekday() - WEEKDAYS.index(starts_on.lower())) % 7
    return day - timedelta(days=offset)
