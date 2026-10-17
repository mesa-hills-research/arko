# Agent evals

These scripts run Arko on a set of small coding tasks with several models and score each run from its saved session. They measure how models behave in Arko. The scores say whether the task got done, how long it took, which tools the agent used, and the moves it wasted.

Each run works on a fresh copy of `project/`, a small Python spending tracker (`tally`) with a unittest suite and no dependencies. The tasks are in `tasks.py`, each with a prompt and a check:

| Task | What it asks | Index of the copy |
|---|---|---|
| `settings` | change two values in `settings.toml` | no |
| `failing-test` | find why a test fails and fix the code | yes |
| `question` | which rule wins when several match, and which function decides | no |
| `rename` | rename a function everywhere | yes |
| `feature` | add a `--category` option with a test | no |
| `split` | add `report --json` and an `export` command, with tests | yes |
| `delegate` | the same two features, each given to a worker | yes |

Tasks without an index of the copy still list an unrelated index, so the agent sees "None of them covers the working directory".

## Running

Build the binary to measure and copy it somewhere a later build won't replace it. Make a template `ARKO_HOME` with a `.env` holding `VOYAGE_API_KEY` and an index of an unrelated project. Then run a batch:

```
cp target/release/arko ~/evals/bin/arko
ARKO_HOME=~/evals/template-home arko index create ~/evals/decoy/some-project

python3 evals/run.py --arko ~/evals/bin/arko --template-home ~/evals/template-home \
    --out ~/evals/runs --batch base --runs 3 \
    --models cc:claude-haiku-5-5,cc:opus,luna,astra --arko-arg=--text-replies
```

`--arko-arg` passes an argument to Arko before `-p` (repeat it for more). `--tasks` picks tasks, and `--worker-model` goes through `--arko-arg` for the `delegate` task. Each run gets its own `ARKO_HOME` under the batch folder, so its session and indexes stay apart from yours. A batch run again skips the runs it finished.

## Scoring

```
python3 evals/score.py ~/evals/runs/base                  # tables by model, task and tier
python3 evals/score.py --failures ~/evals/runs/base       # also each run that failed or wasted moves
python3 evals/score.py --show ~/evals/runs/base/rename/luna-2   # one run's transcript
```

The tables count successes per model and task, with the median wall time, tool calls, failed calls, wasted moves (searches of the disk, index searches no index covers, file edits through the shell, files read again, changes nobody asked for), turns that ended before the work was done, and tokens. `score.py` writes `scores.jsonl` in each batch folder with the details of every run.
