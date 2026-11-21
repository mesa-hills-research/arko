# Command memory limits

Commands the agents run through `shell`, `terminal_start` and monitors are capped so one runaway
process dies on its own instead of taking down Arko or your terminal.

By default, each command gets half of system RAM (at least 512 MiB, and never more than total RAM).

| Platform | How it is enforced |
|---|---|
| Linux | Each command runs with `oom_score_adj` set to `500` and inside a transient `systemd-run --user --scope` with `MemoryMax` set to the cap and `MemorySwapMax=0`. When user scopes aren't available, commands run without a hard cap and keep `oom_score_adj` at `500`. |
| Windows | Each command's Job Object sets `JOB_OBJECT_LIMIT_JOB_MEMORY` across the command and its descendants. |
| macOS | No hard physical-memory cap is applied. |

## Changing or disabling the cap

Set `ARKO_COMMAND_MEMORY_LIMIT` in the environment or a `.env` file.

| Value | Effect |
|---|---|
| `8G`, `8GB`, `8GiB` | Cap each command at 8 GiB (`K`, `M`, `G` and `T` are powers of 1024, case-insensitive, with optional `B` or `iB`) |
| `4096M`, `1.5G` | Cap in mebibytes or fractional gibibytes |
| `1073741824` | Exact cap in bytes |
| `off`, `none`, `0` | Turn off the hard memory cap (on Linux, `oom_score_adj` stays `500`) |
