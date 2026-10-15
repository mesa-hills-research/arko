#!/usr/bin/env python3
"""Scores eval runs from their saved sessions and their copies of the project.

    python3 evals/score.py ~/evals/runs/base                     # tables for a batch
    python3 evals/score.py ~/evals/runs/base ~/evals/runs/rules  # several batches
    python3 evals/score.py --failures ~/evals/runs/base          # and each run with a problem
    python3 evals/score.py --show ~/evals/runs/base/rename/luna-2   # one run's transcript

For each run it records whether the task succeeded (the task's check, on the
run's copy of the project), the wall time, the tool calls by tool, failed
calls, wasted moves, whether the turn ended before the work was done, and
token use, with more in scores.jsonl (index searches that found nothing or
asked for the wrong language, the workers' calls). It writes scores.jsonl
in each batch folder it reads and prints a table by model and task, then
summaries by tier and by model.

Wasted moves:
- disk: a search (find, fd, locate, rg, tree, grep -r) of a path outside
  the run's folder, such as / or the home folder.
- idx: an index_search that failed because no index covers the working
  directory, or a search of the unrelated index.
- shell-edit: a project file changed through the shell (sed -i, a
  redirect, tee, a script that writes) instead of edit_file or write_file.
  They don't count on the rename task, where the instructions suggest the
  shell for a change across many files.
- reread: a read_file of the same file and lines again, with no change to
  the file in between.
- extra: a file changed or left behind that the task didn't ask for.
"""

import argparse
import json
import re
import shlex
import statistics
import sys
from collections import Counter, defaultdict
from fnmatch import fnmatch
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from tasks import BY_ID, changed_files  # noqa: E402

SMALL = ("haiku", "luna")
SEARCHERS = {"find", "fd", "locate", "rg", "tree", "grep", "egrep"}
EDIT_TOOLS = {"edit_file", "write_file", "apply_diff"}
NOT_WORK = {"complete_task", "message_user"}


def tier(model: str) -> str:
    return "small" if any(name in model for name in SMALL) else "big"


def session_dir(run_dir: Path) -> Path | None:
    sessions = sorted((run_dir / "home/sessions").glob("*/run/events.jsonl"))
    return sessions[0].parent.parent if sessions else None


def events_of(session: Path) -> list[dict]:
    path = session / "run/events.jsonl"
    events = []
    for line in path.read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        # Workers' lifecycle records (started, workspace removed) have
        # another shape.
        if "event" in event:
            events.append(event)
    return events


def kind(event: dict) -> tuple[str, object]:
    body = event["event"]
    if isinstance(body, str):
        return body, None
    name = next(iter(body))
    return name, body[name]


def agent_files(session: Path) -> dict[str, dict]:
    """The latest saved state of each agent run, by run id."""
    runs = {}
    for folder in (session / "run/agents").glob("*"):
        files = sorted(folder.glob("*.json"), key=lambda path: path.stat().st_mtime)
        if files:
            try:
                runs[folder.name] = json.loads(files[-1].read_text())
            except json.JSONDecodeError:
                pass
    return runs


def inside(path: str, work: Path) -> bool:
    if path.startswith("~") or path.startswith("$HOME") or path.startswith("${HOME}"):
        return False
    if not path.startswith("/"):
        return True
    try:
        Path(path).resolve().relative_to(work.resolve())
        return True
    except ValueError:
        return False


def split_commands(command: str) -> list[list[str]]:
    """The simple commands in a shell line, as words (roughly)."""
    pieces = re.split(r"\|\||&&|[|;\n]", command)
    words = []
    for piece in pieces:
        try:
            words.append(shlex.split(piece, comments=True))
        except ValueError:
            words.append(piece.split())
    return [w for w in words if w]


def searches_outside(command: str, work: Path) -> bool:
    for words in split_commands(command):
        name = Path(words[0]).name
        if name in ("sudo", "xargs") and len(words) > 1:
            words = words[1:]
            name = Path(words[0]).name
        if name == "locate":
            return True
        if name not in SEARCHERS:
            continue
        # grep searches folders only when recursive.
        if name in ("grep", "egrep") and not any(
            word in ("-r", "-R", "--recursive") or (word.startswith("-") and not word.startswith("--") and ("r" in word or "R" in word))
            for word in words[1:]
        ):
            continue
        for word in words[1:]:
            if word.startswith("-") or word in ("/dev/null",):
                continue
            if word.startswith(("/", "~", "$HOME", "${HOME}")) and not inside(word, work):
                return True
    return False


REDIRECT = re.compile(r"(?<![0-9&<>])>>?\s*([^\s|;&<>]+)")
TEE = re.compile(r"\btee\s+(?:-a\s+)?([^\s|;&<>]+)")
# The project's own files: scratch files written to try something out
# don't count.
PROJECT_FILE = re.compile(r"(?:^|[\s'\"/=(])(?:tally|tests|config|data)/[\w./-]+|README\.md")
SCRIPT_WRITE = re.compile(r"write_text\(|\.write\(|open\([^)]*['\"][wa]b?['\"]|os\.replace\(|shutil\.(move|copy)")


def unquoted(command: str) -> str:
    """The command without its quoted strings (and heredoc bodies), so that
    a ">" in a script's text isn't taken for a redirect."""
    command = re.sub(r"<<-?\s*['\"]?(\w+)['\"]?.*?\n\1\b", "<<HEREDOC", command, flags=re.DOTALL)
    command = re.sub(r"'[^']*'", "''", command)
    return re.sub(r'"(?:\\.|[^"\\])*"', '""', command)


def shell_edit(command: str, work: Path) -> str | None:
    """How a shell command changes project files, if it does."""
    bare = unquoted(command)
    if re.search(r"\bsed\b[^|;&]*\s-i|\bsed\s+-i|\bperl\b[^|;&]*\s-p?i", bare):
        return "sed -i"
    for target in REDIRECT.findall(bare) + TEE.findall(bare):
        if PROJECT_FILE.search(target) and inside(target, work):
            return "redirect"
    if re.search(r"\b(python3?|perl|node|ruby)\b", command) and SCRIPT_WRITE.search(command):
        if PROJECT_FILE.search(command):
            return "script"
    return None


def allowed(path: str, patterns: tuple[str, ...]) -> bool:
    for pattern in patterns:
        if pattern.endswith("/") and path.startswith(pattern):
            return True
        if fnmatch(path, pattern):
            return True
    return False


UNFINISHED = re.compile(
    r"\b(I'll|I will|let me|let's|next,? I|now I|I'm going to|I am going to|going to (run|add|fix|update|check))\b",
    re.IGNORECASE,
)


def score(run_dir: Path) -> dict:
    meta = json.loads((run_dir / "run.json").read_text())
    task = BY_ID[meta["task"]]
    work = run_dir / "work"
    stdout = (run_dir / "stdout.txt").read_text(errors="replace")
    stderr = (run_dir / "stderr.txt").read_text(errors="replace")
    result = {
        "run": str(run_dir),
        "batch": meta["batch"],
        "task": meta["task"],
        "model": meta["model"],
        "tier": tier(meta["model"]),
        "number": meta["number"],
        "fin_args": meta["fin_args"],
        "wall": meta["wall_seconds"],
        "timed_out": meta["timed_out"],
        "exit_code": meta["exit_code"],
    }
    check = task.check(work, meta["base"], stdout)
    result["success"] = check.ok
    result["check_notes"] = check.notes

    session = session_dir(run_dir)
    events = events_of(session) if session else []
    agents = agent_files(session) if session else {}

    calls = []
    errors = set()
    outputs = {}
    final_state = None
    usage = Counter()
    model_calls = 0
    plan = {}
    workers = set()
    for event in events:
        name, body = kind(event)
        agent = event.get("agent_name")
        if agent != "main":
            workers.add(event.get("agent_run_id"))
        if name == "ToolCallFormatted":
            calls.append((agent, event.get("agent_run_id"), body["tool_name"], body["parameters"], body["tool_call_id"]))
        elif name == "ToolResult":
            outputs[body["tool_call_id"]] = body["output"] or outputs.get(body["tool_call_id"], "")
        elif name == "ErrorEncountered" and body.get("tool_call_id"):
            errors.add(body["tool_call_id"])
            outputs.setdefault(body["tool_call_id"], body.get("error_message", ""))
        elif name == "TaskStateChanged" and agent == "main":
            final_state = body
        elif name == "UsageReported":
            for key in ("input_tokens", "output_tokens", "cached_tokens", "cache_write_tokens"):
                usage[key] += body.get(key) or 0
            usage["api_cost"] += body.get("total_cost") or 0
        elif name == "ModelCallUsageReported":
            model_calls += 1
        elif name == "PlanUsageReported":
            plan[body["plan"]] = body
    # Errors marked in the conversations (a tool's own error result).
    for state in agents.values():
        for message in state.get("conversation", {}).get("messages", []):
            for part in message.get("content", []):
                if isinstance(part, dict) and "ToolResult" in part and part["ToolResult"].get("is_error"):
                    errors.add(part["ToolResult"].get("call_id"))

    work_calls = [call for call in calls if call[2] not in NOT_WORK]
    result["calls"] = len(work_calls)
    result["tools"] = dict(Counter(call[2] for call in work_calls))
    result["workers"] = len(workers)
    worker_calls = [call for call in work_calls if call[0] != "main"]
    result["worker_calls"] = len(worker_calls)
    result["worker_tools"] = dict(Counter(call[2] for call in worker_calls))
    result["model_calls"] = model_calls
    failed = [call for call in work_calls if call[4] in errors or (outputs.get(call[4], "").startswith("Tool '") and "failed" in outputs.get(call[4], "")[:200])]
    result["failed"] = len(failed)
    result["failed_examples"] = [f"{call[2]}: {outputs.get(call[4], '')[:160]}" for call in failed[:4]]
    nonzero = 0
    for call in work_calls:
        if call[2] == "shell":
            match = re.match(r"exit code (-?\d+)", outputs.get(call[4], ""))
            if match and match.group(1) != "0":
                nonzero += 1
    result["nonzero_exits"] = nonzero

    # Wasted moves.
    disk = []
    shell_edits = []
    for call in work_calls:
        command = call[3].get("command") if isinstance(call[3].get("command"), str) else None
        if call[2] in ("shell", "terminal_start") and command:
            # The run's folder holds the copy and the workers' worktrees.
            if searches_outside(command, run_dir):
                disk.append(command[:160])
            how = shell_edit(command, run_dir)
            if how and task.id != "rename":
                shell_edits.append(f"{how}: {command[:160]}")
    idx = 0
    for call in work_calls:
        if call[2] != "index_search":
            continue
        out = outputs.get(call[4], "")
        if "no index covers" in out or call[3].get("index") == "crossterm":
            idx += 1
    # Searches: how many, how many found nothing, and how many asked for a
    # language the project (Python) doesn't have.
    searches = [call for call in work_calls if call[2] == "index_search"]
    result["searches"] = {
        "calls": len(searches),
        "empty": sum(outputs.get(call[4], "").startswith("No results") for call in searches),
        "wrong_language": sum(
            str(call[3].get("language", "python")).lower() not in ("python", "py") for call in searches
        ),
    }
    main_calls = [call for call in work_calls if call[0] == "main"]
    result["first_tool"] = main_calls[0][2] if main_calls else None
    rereads = 0
    for run_id in {call[1] for call in work_calls}:
        seen = set()
        for call in [c for c in work_calls if c[1] == run_id]:
            if call[2] == "read_file":
                key = (call[3].get("path"), str(call[3].get("start_line")), str(call[3].get("end_line")))
                if key in seen:
                    rereads += 1
                seen.add(key)
            elif call[2] in EDIT_TOOLS:
                path = call[3].get("path")
                seen = {key for key in seen if key[0] != path}
            elif call[2] == "shell" and shell_edit(str(call[3].get("command", "")), work):
                seen = set()
    changed = changed_files(work, meta["base"])
    extra = [path for path in changed if not allowed(path, task.allowed)]
    result["changed"] = changed
    result["waste"] = {
        "disk": len(disk),
        "idx": idx,
        "shell_edit": len(shell_edits),
        "reread": rereads,
        "extra": len(extra),
    }
    result["waste_total"] = sum(result["waste"].values())
    result["disk_examples"] = disk[:3]
    result["shell_edit_examples"] = shell_edits[:3]
    result["extra_files"] = extra
    result["index_creates"] = result["tools"].get("index_create", 0)

    # How the turn ended.
    reply = stdout.strip()
    if isinstance(final_state, dict) and "Completed" in final_state:
        ended = "completed"
        reply = final_state["Completed"].get("summary") or reply
    elif isinstance(final_state, dict) and "Failed" in final_state:
        ended = "failed"
    elif meta["timed_out"]:
        ended = "timeout"
    else:
        ended = str(final_state)[:40]
    result["ended"] = ended
    result["reply_tail"] = reply[-300:]
    result["ended_early"] = (
        not check.ok and ended == "completed" and bool(UNFINISHED.search(reply[-400:]) or not changed and task.id != "question")
    )
    result["tokens"] = {key: int(value) for key, value in usage.items() if key != "api_cost"}
    result["api_cost"] = round(usage["api_cost"], 4)
    claude = plan.get("claude_code", {}).get("primary", {})
    chatgpt = plan.get("chatgpt", {}).get("primary", {})
    result["plan"] = {
        "claude_5h": claude.get("used_percent"),
        "chatgpt": chatgpt.get("used_percent"),
        "chatgpt_window_minutes": chatgpt.get("window_minutes"),
    }
    footer = re.findall(r"── .* ──", stderr)
    result["footer"] = footer[-1] if footer else ""
    return result


def runs_in(folder: Path) -> list[Path]:
    return sorted(path.parent for path in folder.glob("**/run.json") if "work" not in path.parts)


def fmt_rate(values: list[bool]) -> str:
    return f"{sum(values)}/{len(values)}"


def mean(values) -> float:
    values = list(values)
    return statistics.mean(values) if values else 0.0


def table(results: list[dict]) -> str:
    groups = defaultdict(list)
    for result in results:
        groups[(result["model"], result["task"])].append(result)
    tasks = list(dict.fromkeys(task for task in BY_ID if any(r["task"] == task for r in results)))
    models = sorted({r["model"] for r in results}, key=lambda m: (tier(m), m))
    lines = [
        "| model | task | ok | wall s | calls | failed | disk | idx | shell-edit | reread | extra | early | tokens in/out (k) |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for model in models:
        for task in tasks:
            group = groups.get((model, task))
            if not group:
                continue
            waste = lambda key: sum(r["waste"][key] for r in group)  # noqa: E731
            lines.append(
                f"| {model} | {task} | {fmt_rate([r['success'] for r in group])} | {statistics.median(r['wall'] for r in group):.0f}"
                f" | {mean(r['calls'] for r in group):.1f} | {sum(r['failed'] for r in group)}"
                f" | {waste('disk')} | {waste('idx')} | {waste('shell_edit')} | {waste('reread')} | {waste('extra')}"
                f" | {sum(r['ended_early'] for r in group)}"
                f" | {mean(r['tokens'].get('input_tokens', 0) for r in group) / 1000:.0f}/{mean(r['tokens'].get('output_tokens', 0) for r in group) / 1000:.1f} |"
            )
    return "\n".join(lines)


def summary(results: list[dict], key: str) -> str:
    groups = defaultdict(list)
    for result in results:
        groups[result[key]].append(result)
    lines = [
        f"| {key} | runs | ok | median wall s | calls | failed | disk | idx | shell-edit | reread | extra | early | timeouts |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for name in sorted(groups):
        group = groups[name]
        waste = lambda k: sum(r["waste"][k] for r in group)  # noqa: E731
        lines.append(
            f"| {name} | {len(group)} | {fmt_rate([r['success'] for r in group])} | {statistics.median(r['wall'] for r in group):.0f}"
            f" | {mean(r['calls'] for r in group):.1f} | {sum(r['failed'] for r in group)}"
            f" | {waste('disk')} | {waste('idx')} | {waste('shell_edit')} | {waste('reread')} | {waste('extra')}"
            f" | {sum(r['ended_early'] for r in group)} | {sum(r['timed_out'] for r in group)} |"
        )
    return "\n".join(lines)


def show(run_dir: Path) -> None:
    """The main agent's and workers' steps, in order."""
    session = session_dir(run_dir)
    if not session:
        print("no session")
        return
    for run_id, state in agent_files(session).items():
        print(f"===== {state.get('agent_config_id')} {run_id}")
        for message in state.get("conversation", {}).get("messages", []):
            role = message.get("role")
            for part in message.get("content", []):
                if "Text" in part and part["Text"].strip():
                    print(f"[{role}] {part['Text'][:1500]}")
                elif "ToolResult" in part:
                    tool = part["ToolResult"]
                    mark = " ERROR" if tool.get("is_error") else ""
                    print(f"  -> {tool.get('name')}{mark}: {str(tool.get('output'))[:400]!r}")
            for call in message.get("tool_calls", []) or []:
                print(f"  CALL {call.get('name')} {json.dumps(call.get('arguments'))[:500]}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("folders", nargs="+", type=Path)
    parser.add_argument("--show", action="store_true", help="print the transcript of one run")
    parser.add_argument("--failures", action="store_true", help="list each run that failed or wasted moves")
    args = parser.parse_args()
    if args.show:
        for folder in args.folders:
            show(folder)
        return
    results = []
    for folder in args.folders:
        batch = []
        for run_dir in runs_in(folder):
            batch.append(score(run_dir))
        with open(folder / "scores.jsonl", "w") as out:
            for result in batch:
                out.write(json.dumps(result) + "\n")
        results.extend(batch)
    print(table(results))
    print()
    print(summary(results, "tier"))
    print()
    print(summary(results, "model"))
    claude = [r["plan"]["claude_5h"] for r in results if r["plan"]["claude_5h"] is not None]
    if claude:
        print(f"\nClaude plan, 5 h window: {min(claude)}% to {max(claude)}% during these runs")
    if args.failures:
        print()
        for result in results:
            if not result["success"] or result["waste_total"] or result["failed"]:
                print(f"- {result['run']}: ok={result['success']} ended={result['ended']} early={result['ended_early']} waste={result['waste']} failed={result['failed']}")
                for note in result["check_notes"]:
                    print(f"    check: {note}")
                for example in result["disk_examples"] + result["shell_edit_examples"] + result["failed_examples"]:
                    print(f"    {example}")
                if result["extra_files"]:
                    print(f"    extra: {result['extra_files']}")
                if not result["success"]:
                    print(f"    reply: {result['reply_tail'][-200:]!r}")


if __name__ == "__main__":
    main()
