"""The eval tasks: what each one asks of Arko, how its copy of the project is
set up, and how its result is checked.

Each task runs on a fresh copy of `project/` (a small Python spending
tracker, `tally`, tested with unittest). `index` says whether the copy gets
a search index of its own: the others run with only an unrelated index in
the list, which is the case of "no index covers the working directory".
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

PROJECT = Path(__file__).resolve().parent / "project"


@dataclass
class Check:
    ok: bool
    notes: list[str] = field(default_factory=list)


@dataclass
class Task:
    id: str
    title: str
    prompt: str
    index: bool
    check: Callable[[Path, str, str], Check]
    # Paths the task may change or add; anything else counts as a change
    # beyond what was asked. A trailing "/" allows a whole folder.
    allowed: tuple[str, ...]
    setup: Callable[[Path], None] | None = None
    timeout: int = 600


def git(work: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(work), *args], capture_output=True, text=True, check=False
    ).stdout


def changed_files(work: Path, base: str) -> list[str]:
    """Files changed since the copy's first commit (committed or not), and
    files added and not ignored."""
    changed = set(git(work, "diff", "--name-only", base).split())
    changed |= set(git(work, "ls-files", "--others", "--exclude-standard").split())
    return sorted(changed)


def added_text(work: Path, base: str, prefix: str) -> str:
    """The lines added under `prefix` since `base`, with new files whole."""
    text = git(work, "diff", base, "--", prefix)
    added = [line[1:] for line in text.splitlines() if line.startswith("+") and not line.startswith("+++")]
    for name in git(work, "ls-files", "--others", "--exclude-standard", "--", prefix).split():
        try:
            added.append((work / name).read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError):
            pass
    return "\n".join(added)


def python(work: Path, *args: str, timeout: int = 60) -> subprocess.CompletedProcess:
    env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
    return subprocess.run(
        [sys.executable, *args], cwd=work, capture_output=True, text=True, timeout=timeout, env=env
    )


def tests_pass(work: Path, notes: list[str]) -> bool:
    try:
        result = python(work, "-m", "unittest")
    except subprocess.TimeoutExpired:
        notes.append("the tests timed out")
        return False
    summary = (result.stderr.strip().splitlines() or [""])[-1]
    ran = re.search(r"Ran (\d+) tests?", result.stderr)
    notes.append(f"tests: {summary} ({ran.group(1) if ran else '?'} ran)")
    return result.returncode == 0


def tally(work: Path, ledger: Path, *args: str) -> subprocess.CompletedProcess:
    return python(work, "-m", "tally", "--ledger", str(ledger), *args)


def imported_ledger(work: Path, folder: Path) -> Path:
    ledger = folder / "ledger.json"
    tally(work, ledger, "import", "data/sample.csv")
    return ledger


# T1: change a named file. --------------------------------------------------


def check_settings(work: Path, base: str, reply: str) -> Check:
    notes: list[str] = []
    path = work / "config/settings.toml"
    try:
        now = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        return Check(False, [f"settings.toml unreadable: {error}"])
    before = tomllib.loads(git(work, "show", f"{base}:config/settings.toml"))
    ok = str(now.get("currency", "")).upper() == "EUR"
    ok &= str(now.get("week_starts_on", "")).lower() == "monday"
    rest_same = {k: v for k, v in now.items() if k not in ("currency", "week_starts_on")} == {
        k: v for k, v in before.items() if k not in ("currency", "week_starts_on")
    }
    if not rest_same:
        notes.append("other settings changed")
    loads = python(work, "-c", "from tally.config import load_settings; load_settings()")
    if loads.returncode != 0:
        notes.append("load_settings() fails")
    numstat = git(work, "diff", "--numstat", base, "--", "config/settings.toml").split()
    notes.append(f"settings.toml diff +{numstat[0] if numstat else 0}/-{numstat[1] if numstat else 0}")
    ok = ok and rest_same and loads.returncode == 0 and tests_pass(work, notes)
    return Check(ok, notes)


# T2: fix a failing test. ---------------------------------------------------


def break_december(work: Path) -> None:
    path = work / "tally/dates.py"
    text = path.read_text(encoding="utf-8")
    broken = text.replace(
        "date(year + month // 12, month % 12 + 1, 1)", "date(year, month % 12 + 1, 1)"
    )
    assert broken != text
    path.write_text(broken, encoding="utf-8")


def check_failing_test(work: Path, base: str, reply: str) -> Check:
    notes: list[str] = []
    tests_changed = git(work, "diff", "--name-only", base, "--", "tests").split()
    if tests_changed:
        notes.append(f"changed the tests: {', '.join(tests_changed)}")
    ok = tests_pass(work, notes) and not tests_changed
    return Check(ok, notes)


# T3: answer a question. ----------------------------------------------------


def check_question(work: Path, base: str, reply: str) -> Check:
    text = reply.lower()
    names = "best_rule" in text
    longest = bool(re.search(r"longest|most specific|longer pattern|length of the pattern|pattern length", text))
    notes = []
    if not names:
        notes.append("doesn't name best_rule")
    if not longest:
        notes.append("doesn't say the longest pattern wins")
    if "first" in text and ("tie" in text or "same length" in text or "equal" in text):
        notes.append("mentions ties")
    return Check(names and longest, notes)


# T4: rename across files. --------------------------------------------------


def check_rename(work: Path, base: str, reply: str) -> Check:
    notes: list[str] = []
    left = git(work, "grep", "-n", "format_amount").strip()
    left_untracked = [
        name
        for name in git(work, "ls-files", "--others", "--exclude-standard").split()
        if "format_amount" in (work / name).read_text(encoding="utf-8", errors="replace")
    ]
    if left or left_untracked:
        notes.append(f"format_amount left: {len(left.splitlines()) + len(left_untracked)}")
    defined = "def format_money(" in (work / "tally/money.py").read_text(encoding="utf-8")
    if not defined:
        notes.append("money.py doesn't define format_money")
    ok = not left and not left_untracked and defined and tests_pass(work, notes)
    return Check(ok, notes)


# T5: a small feature with a test. ------------------------------------------


def check_category_option(work: Path, base: str, reply: str) -> Check:
    notes: list[str] = []
    with tempfile.TemporaryDirectory() as folder:
        ledger = imported_ledger(work, Path(folder))
        only = tally(work, ledger, "report", "--month", "2026-09", "--category", "groceries")
        full = tally(work, ledger, "report", "--month", "2026-09")
    filtered = only.returncode == 0 and "groceries" in only.stdout and not re.search(
        r"dining|transport|utilities|uncategorized", only.stdout
    )
    if not filtered:
        notes.append(f"--category groceries gave: {(only.stdout + only.stderr).strip()[:200]!r}")
    unchanged = full.returncode == 0 and "dining" in full.stdout and "transport" in full.stdout
    if not unchanged:
        notes.append("the report without --category changed")
    tested = "category" in added_text(work, base, "tests").lower()
    if not tested:
        notes.append("no test added")
    ok = filtered and unchanged and tested and tests_pass(work, notes)
    return Check(ok, notes)


# T6: a task a lead could split. ---------------------------------------------

EXPECTED_SEPTEMBER = {
    "groceries": (45000, 42698),
    "dining": (20000, 11190),
    "transport": (12000, 4775),
    "utilities": (18000, 9640),
    "fun": (10000, 6299),
    "uncategorized": (0, 2745),
}


def check_json_and_export(work: Path, base: str, reply: str) -> Check:
    notes: list[str] = []
    with tempfile.TemporaryDirectory() as folder:
        folder = Path(folder)
        ledger = imported_ledger(work, folder)
        report = tally(work, ledger, "report", "--month", "2026-09", "--json")
        json_ok = False
        try:
            items = json.loads(report.stdout)
            by_category = {
                item["category"]: item
                for item in items
                if str(item.get("category", "")).lower() != "total"
            }
            json_ok = set(by_category) == set(EXPECTED_SEPTEMBER) and all(
                by_category[name] == {"category": name, "budget": budget, "spent": spent, "left": budget - spent}
                for name, (budget, spent) in EXPECTED_SEPTEMBER.items()
            )
            if not json_ok:
                notes.append(f"report --json: {report.stdout.strip()[:200]!r}")
        except (ValueError, TypeError, KeyError, AttributeError):
            notes.append(f"report --json isn't the list asked for: {(report.stdout + report.stderr).strip()[:200]!r}")
        exported = folder / "out.csv"
        export = tally(work, ledger, "export", str(exported))
        again = folder / "again.json"
        export_ok = False
        if export.returncode == 0 and exported.exists():
            tally(work, again, "import", str(exported))
            if again.exists():
                first = json.loads(ledger.read_text())["transactions"]
                second = json.loads(again.read_text())["transactions"]
                export_ok = first == second
                if not export_ok:
                    notes.append("export, then import, gives other transactions")
            else:
                notes.append("the export couldn't be imported")
        else:
            notes.append(f"export failed: {(export.stdout + export.stderr).strip()[:200]!r}")
    added = added_text(work, base, "tests").lower()
    tested = "json" in added and "export" in added
    if not tested:
        notes.append("tests for both not added")
    ok = json_ok and export_ok and tested and tests_pass(work, notes)
    return Check(ok, notes)


TASKS = [
    Task(
        "settings",
        "change a named file",
        "In settings.toml, change the currency to EUR and make weeks start on Monday.",
        index=False,
        check=check_settings,
        allowed=("config/settings.toml",),
    ),
    Task(
        "failing-test",
        "fix a failing test",
        "A test is failing. Find out why and fix the code, not the test.",
        index=True,
        check=check_failing_test,
        allowed=("tally/dates.py",),
        setup=break_december,
    ),
    Task(
        "question",
        "answer a question",
        "When a transaction's payee matches more than one categorization rule, which category does it get? Which function decides that?",
        index=False,
        check=check_question,
        allowed=(),
    ),
    Task(
        "rename",
        "rename across files",
        "Rename format_amount to format_money everywhere.",
        index=True,
        check=check_rename,
        allowed=("tally/money.py", "tally/report.py", "tally/cli.py", "tests/test_money.py"),
    ),
    Task(
        "feature",
        "a small feature with a test",
        "Add a --category option to the report command that shows only that category. Add a test for it.",
        index=False,
        check=check_category_option,
        allowed=("tally/cli.py", "tally/report.py", "tally/budget.py", "tests/", "README.md"),
    ),
    Task(
        "split",
        "a task a lead could split",
        "Add two things to the command line, each with tests:\n"
        "1. `report --json`, which prints the report as a JSON list with one object per category, with the keys category, budget, spent and left (amounts in cents, as integers).\n"
        "2. An `export PATH` command that writes all the ledger's transactions to a CSV file that `import` reads back as the same transactions.",
        index=True,
        check=check_json_and_export,
        allowed=("tally/", "tests/", "README.md"),
        timeout=900,
    ),
]

# T7: the split task, handed to workers. Run it with --worker-model to
# measure a small model as a worker under a large lead.
TASKS.append(
    Task(
        "delegate",
        "a task handed to workers",
        "Add two things to the command line, each with tests. Give each one to its own worker, in a worktree of its own, and merge their work when they're done.\n"
        "1. `report --json`, which prints the report as a JSON list with one object per category, with the keys category, budget, spent and left (amounts in cents, as integers).\n"
        "2. An `export PATH` command that writes all the ledger's transactions to a CSV file that `import` reads back as the same transactions.",
        index=True,
        check=check_json_and_export,
        allowed=("tally/", "tests/", "README.md"),
        timeout=1200,
    )
)

BY_ID = {task.id: task for task in TASKS}
