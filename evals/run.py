#!/usr/bin/env python3
"""Runs Arko on the eval tasks, each run on a fresh copy of the project.

    python3 evals/run.py --out ~/evals --batch base \\
        --models cc:claude-haiku-5-5,cc:opus,luna,astra --runs 3

Every run gets a folder, <out>/<batch>/<task>/<model>-<n>/, with:
- work/: the copy of evals/project, a git repository whose first commit is
  where the run started (with the task's setup applied).
- home/: the run's ARKO_HOME. It starts as a copy of --template-home, which
  holds a .env with VOYAGE_API_KEY and an index of an unrelated project, so
  the agent sees an index list as the owner does. Tasks marked `index` also
  get an index of work/. The session is saved here too, apart from the
  owner's own sessions and indexes.
- stdout.txt, stderr.txt and run.json (the command, wall time, exit code).

A run whose run.json exists is skipped, so an interrupted batch can be run
again with the same arguments. Score the runs with score.py.
"""

import argparse
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from tasks import BY_ID, PROJECT, TASKS  # noqa: E402


def slug(model: str) -> str:
    return model.replace(":", "-").replace("/", "-")


def provider(model: str) -> str:
    """Which plan a model uses: runs on one plan share its limit."""
    return "claude" if model.startswith(("cc:", "claude-code:")) else "chatgpt"


def clean_env(home: Path) -> dict:
    """The environment of a run: this one, without the variables of a Claude
    Code session Arko may be started from (they would reach the `claude` that
    runs cc: models), and with ARKO_HOME set to the run's own."""
    env = {
        key: value
        for key, value in os.environ.items()
        if not (key == "CLAUDECODE" or key.startswith("CLAUDE_"))
    }
    env["ARKO_HOME"] = str(home)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return env


def prepare(run_dir: Path, task, arko: str, template_home: Path) -> tuple[Path, Path, str]:
    work = run_dir / "work"
    shutil.copytree(PROJECT, work, ignore=shutil.ignore_patterns("__pycache__", "ledger.json"))
    if task.setup:
        task.setup(work)
    quiet = {"capture_output": True, "check": True}
    subprocess.run(["git", "init", "-q", "-b", "main", str(work)], **quiet)
    subprocess.run(["git", "-C", str(work), "add", "-A"], **quiet)
    subprocess.run(
        ["git", "-C", str(work), "-c", "user.name=tally", "-c", "user.email=tally@example.com",
         "commit", "-qm", "tally 0.4.0"],
        **quiet,
    )
    base = subprocess.run(
        ["git", "-C", str(work), "rev-parse", "HEAD"], capture_output=True, text=True, check=True
    ).stdout.strip()
    home = run_dir / "home"
    shutil.copytree(template_home, home, ignore=shutil.ignore_patterns("sessions", "history.txt", "last-model.json"))
    if task.index:
        with open(run_dir / "index.log", "w") as log:
            subprocess.run(
                [arko, "index", "create", str(work)],
                cwd=work, env=clean_env(home), stdin=subprocess.DEVNULL, stdout=log, stderr=log, check=True,
            )
    return work, home, base


def run_one(args, task, model: str, number: int, limits: dict, printing: threading.Lock) -> None:
    run_dir = Path(args.out) / args.batch / task.id / f"{slug(model)}-{number}"
    if (run_dir / "run.json").exists():
        return
    if run_dir.exists():
        shutil.rmtree(run_dir)
    run_dir.mkdir(parents=True)
    with limits[provider(model)]:
        work, home, base = prepare(run_dir, task, args.arko, Path(args.template_home))
        command = [args.arko, "-m", model, *args.arko_args, "-p", task.prompt]
        timeout = args.timeout or task.timeout
        started = time.time()
        timed_out = False
        with open(run_dir / "stdout.txt", "w") as out, open(run_dir / "stderr.txt", "w") as err:
            process = subprocess.Popen(
                command, cwd=work, env=clean_env(home), stdin=subprocess.DEVNULL,
                stdout=out, stderr=err, start_new_session=True,
            )
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    code = process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    code = process.wait()
        wall = time.time() - started
    record = {
        "task": task.id,
        "model": model,
        "number": number,
        "batch": args.batch,
        "arko_args": args.arko_args,
        "arko": args.arko,
        "arko_sha256": args.arko_sha256,
        "command": command,
        "base": base,
        "index": task.index,
        "started": started,
        "wall_seconds": round(wall, 1),
        "exit_code": code,
        "timed_out": timed_out,
        "timeout": timeout,
    }
    (run_dir / "run.json").write_text(json.dumps(record, indent=2) + "\n")
    with printing:
        state = "timed out" if timed_out else f"exit {code}"
        print(f"{args.batch} {task.id} {model} #{number}: {state}, {wall:.0f} s", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True, help="where the batches' folders go (outside the repository)")
    parser.add_argument("--batch", required=True, help="this batch's name, such as base")
    parser.add_argument("--models", required=True, help="comma-separated Arko model names")
    parser.add_argument("--tasks", default=",".join(task.id for task in TASKS), help="comma-separated task ids")
    parser.add_argument("--runs", type=int, default=3, help="runs per task and model")
    parser.add_argument("--first", type=int, default=1, help="the number of the first run")
    parser.add_argument("--arko", required=True, help="the arko binary, best a copy that no build replaces")
    parser.add_argument("--template-home", required=True, help="the ARKO_HOME each run's home starts from")
    parser.add_argument("--timeout", type=int, default=0, help="seconds per run (default: the task's)")
    parser.add_argument("--claude-jobs", type=int, default=3, help="cc: runs at once")
    parser.add_argument("--chatgpt-jobs", type=int, default=3, help="runs at once on the ChatGPT login")
    parser.add_argument(
        "--arko-arg", dest="arko_args", action="append", default=[],
        help="an argument for Arko, before -p (repeatable), such as --text-replies",
    )
    args = parser.parse_args()
    # The binary each run used: build the one to measure apart from the
    # one you work on, since a build replaces target/release/arko.
    args.arko_sha256 = hashlib.sha256(Path(args.arko).read_bytes()).hexdigest()
    tasks = [BY_ID[name] for name in args.tasks.split(",")]
    models = args.models.split(",")
    limits = {
        "claude": threading.Semaphore(args.claude_jobs),
        "chatgpt": threading.Semaphore(args.chatgpt_jobs),
    }
    printing = threading.Lock()
    # Run numbers first, so a batch cut short has every task and model.
    jobs = [
        (task, model, number)
        for number in range(args.first, args.first + args.runs)
        for task in tasks
        for model in models
    ]
    # The semaphores keep to the limits, so every job can have a thread.
    with ThreadPoolExecutor(max_workers=max(1, len(jobs))) as pool:
        futures = [pool.submit(run_one, args, task, model, number, limits, printing) for task, model, number in jobs]
        for future in futures:
            future.result()


if __name__ == "__main__":
    main()
