//! Per-command memory cap for shell commands, terminal jobs and monitors.
//!
//! By default each command gets half of system RAM (at least 512 MiB, and never
//! more than total RAM), applied by `TerminalManager` through a transient
//! systemd user scope on Linux and a Job Object on Windows. Setting
//! `ARKO_COMMAND_MEMORY_LIMIT` in the environment or a `.env` file overrides
//! the cap, or disables it with `off`, `none` or `0` while keeping Linux's
//! `oom_score_adj` protection.

use crate::credentials;

/// The environment and `.env` variable that overrides the per-command memory
/// cap.
pub const SETTING: &str = "ARKO_COMMAND_MEMORY_LIMIT";

/// Minimum default per-command memory cap (512 MiB), clamped to total RAM.
const DEFAULT_FLOOR_BYTES: u64 = 512 * 1024 * 1024;

/// Resolves the per-command memory cap for `TerminalConfig::memory_max_bytes`.
///
/// Uses [`SETTING`] from the environment or `.env` when set (`off`, `none` or
/// `0` disables the hard cap and returns `Ok(None)`), otherwise half of total
/// physical RAM with a 512 MiB floor (or `Ok(None)` when total RAM can't be
/// read).
pub(crate) fn limit() -> Result<Option<u64>, String> {
    resolve(credentials::var(SETTING).as_deref(), total_memory_bytes)
}

fn resolve(
    override_value: Option<&str>,
    read_total: impl FnOnce() -> Option<u64>,
) -> Result<Option<u64>, String> {
    if let Some(value) = override_value {
        return parse(value).map_err(|()| {
            format!(
                "{SETTING} is `{value}`. Set it to a size such as 8G, 4096M or 1073741824, or off"
            )
        });
    }
    Ok(read_total().and_then(default_from_total))
}

/// Computes the default per-command cap from total physical RAM, taking half of
/// `total_bytes` with a 512 MiB floor clamped to `total_bytes`.
fn default_from_total(total_bytes: u64) -> Option<u64> {
    if total_bytes == 0 {
        return None;
    }
    Some((total_bytes / 2).max(DEFAULT_FLOOR_BYTES).min(total_bytes))
}

/// Parses a memory limit such as `8G`, `4096M`, `1.5GiB`, `1073741824`, or
/// `off`/`none`/`0` (returning `Ok(None)` to disable the hard cap).
pub(crate) fn parse(text: &str) -> Result<Option<u64>, ()> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(());
    }
    if trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(None);
    }

    let number_end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let number = &trimmed[..number_end];
    let suffix = trimmed[number_end..].trim();
    if number.is_empty() {
        return Err(());
    }

    let multiplier = parse_unit(suffix)?;

    if let Some((whole_str, frac_str)) = number.split_once('.') {
        if whole_str.is_empty() || frac_str.is_empty() || frac_str.contains('.') || multiplier == 1
        {
            return Err(());
        }
        let whole: u128 = whole_str.parse().map_err(|_| ())?;
        let sig_frac = frac_str.trim_end_matches('0');
        if whole == 0 && sig_frac.is_empty() {
            return Ok(None);
        }
        let mult = u128::from(multiplier);
        let whole_bytes = whole.checked_mul(mult).ok_or(())?;
        let frac_bytes = if sig_frac.is_empty() {
            0
        } else {
            // Up to 18 decimal places fit in `u128` alongside `TIB` without
            // overflow, and any digit past the 18th is less than a byte even
            // at TiB scale.
            let kept = &sig_frac[..sig_frac.len().min(18)];
            let numerator: u128 = kept.parse().map_err(|_| ())?;
            let denominator = 10u128.pow(kept.len() as u32);
            numerator.checked_mul(mult).ok_or(())? / denominator
        };
        let total = whole_bytes.checked_add(frac_bytes).ok_or(())?;
        if total == 0 {
            return Err(());
        }
        return u64::try_from(total).map(Some).map_err(|_| ());
    }

    let value: u64 = number.parse().map_err(|_| ())?;
    if value == 0 {
        return Ok(None);
    }
    value.checked_mul(multiplier).map(Some).ok_or(())
}

fn parse_unit(suffix: &str) -> Result<u64, ()> {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;
    match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => Ok(1),
        "k" | "kb" | "kib" => Ok(KIB),
        "m" | "mb" | "mib" => Ok(MIB),
        "g" | "gb" | "gib" => Ok(GIB),
        "t" | "tb" | "tib" => Ok(TIB),
        _ => Err(()),
    }
}

/// Parses `MemTotal` from Linux's `/proc/meminfo` text into bytes.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_meminfo(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let value: u64 = parts.next()?.parse().ok()?;
        if value == 0 {
            return None;
        }
        let unit = parts.next().unwrap_or("");
        let multiplier = match unit.to_ascii_lowercase().as_str() {
            "kb" | "kib" => 1024u64,
            "mb" | "mib" => 1024u64 * 1024,
            "gb" | "gib" => 1024u64 * 1024 * 1024,
            "b" | "" => 1,
            _ => return None,
        };
        return value.checked_mul(multiplier);
    }
    None
}

/// Total physical RAM in bytes, or `None` on platforms that don't enforce
/// `memory_max_bytes` or when it can't be read.
#[cfg(target_os = "linux")]
fn total_memory_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_proc_meminfo(&text)
}

#[cfg(windows)]
fn total_memory_bytes() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        dw_length: u32,
        dw_memory_load: u32,
        ull_total_phys: u64,
        ull_avail_phys: u64,
        ull_total_page_file: u64,
        ull_avail_page_file: u64,
        ull_total_virtual: u64,
        ull_avail_virtual: u64,
        ull_avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: `status` is initialized with its exact byte size as required by
    // `GlobalMemoryStatusEx` and lives for the duration of the call.
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    (ok != 0 && status.ull_total_phys > 0).then_some(status.ull_total_phys)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn total_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    #[test]
    fn parse_accepts_human_sizes_and_disable_words() {
        for (input, expected) in [
            ("off", None),
            ("OFF", None),
            ("  Off  ", None),
            ("none", None),
            ("None", None),
            ("0", None),
            ("0B", None),
            ("0m", None),
            ("0GiB", None),
            ("0.0G", None),
            ("1", Some(1)),
            ("1073741824", Some(GIB)),
            ("1073741824b", Some(GIB)),
            ("1073741824 B", Some(GIB)),
            ("512K", Some(512 * KIB)),
            ("512k", Some(512 * KIB)),
            ("512KB", Some(512 * KIB)),
            ("512KiB", Some(512 * KIB)),
            ("4096M", Some(4096 * MIB)),
            ("4096mb", Some(4096 * MIB)),
            ("4096 MiB", Some(4096 * MIB)),
            ("8G", Some(8 * GIB)),
            ("8g", Some(8 * GIB)),
            ("8GB", Some(8 * GIB)),
            (" 8 GiB ", Some(8 * GIB)),
            ("1T", Some(TIB)),
            ("1tb", Some(TIB)),
            ("2TiB", Some(2 * TIB)),
            ("1.5G", Some(GIB + 512 * MIB)),
            ("0.5M", Some(512 * KIB)),
            ("2.25K", Some(2 * KIB + 256)),
            ("1.50000000000000000000G", Some(GIB + 512 * MIB)),
        ] {
            assert_eq!(parse(input), Ok(expected), "input: {input:?}");
        }
    }

    #[test]
    fn parse_rejects_invalid_and_overflowing_values() {
        for input in [
            "",
            "   ",
            "-8G",
            "+8G",
            "8X",
            "8Gi",
            "8GBs",
            "8 G B",
            "1.5",
            "1.5B",
            "0.00001K",
            ".5G",
            "1.G",
            "1..5G",
            "1e6",
            "abc",
            "16777216T",
            "18446744073709551616",
        ] {
            assert_eq!(parse(input), Err(()), "input: {input:?}");
        }
    }

    #[test]
    fn default_from_total_takes_half_with_512_mib_floor() {
        assert_eq!(default_from_total(0), None);
        assert_eq!(default_from_total(256 * MIB), Some(256 * MIB));
        assert_eq!(default_from_total(512 * MIB), Some(512 * MIB));
        assert_eq!(default_from_total(768 * MIB), Some(512 * MIB));
        assert_eq!(default_from_total(GIB), Some(512 * MIB));
        assert_eq!(default_from_total(8 * GIB), Some(4 * GIB));
        assert_eq!(default_from_total(32 * GIB), Some(16 * GIB));
    }

    #[test]
    fn parse_proc_meminfo_extracts_memtotal_in_bytes() {
        let sample = "\
MemTotal:       24031764 kB
MemFree:         6226252 kB
MemAvailable:   18251856 kB
";
        assert_eq!(parse_proc_meminfo(sample), Some(24_031_764 * KIB));
        assert_eq!(parse_proc_meminfo("MemFree: 1000 kB\n"), None);
        assert_eq!(parse_proc_meminfo("MemTotal: 0 kB\n"), None);
        assert_eq!(parse_proc_meminfo("MemTotal: nope kB\n"), None);
        assert_eq!(parse_proc_meminfo("MemTotal: 100 widgets\n"), None);
    }

    #[test]
    fn resolve_prefers_override_and_falls_back_to_default() {
        assert_eq!(resolve(Some("4G"), || Some(32 * GIB)), Ok(Some(4 * GIB)));
        assert_eq!(resolve(Some("off"), || Some(32 * GIB)), Ok(None));
        assert_eq!(resolve(Some("none"), || Some(32 * GIB)), Ok(None));
        assert_eq!(resolve(Some("0"), || Some(32 * GIB)), Ok(None));
        assert_eq!(resolve(None, || Some(32 * GIB)), Ok(Some(16 * GIB)));
        assert_eq!(resolve(None, || None), Ok(None));
        let err = resolve(Some("bogus"), || Some(32 * GIB)).unwrap_err();
        assert!(err.contains(SETTING) && err.contains("`bogus`"), "{err}");
    }

    #[test]
    fn dotenv_reads_command_memory_limit() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".env"), "ARKO_COMMAND_MEMORY_LIMIT=2G\n").unwrap();
        let env = credentials::Env::for_folder_in(Some(temp.path()), false, None);
        if std::env::var_os(SETTING).is_none() {
            assert_eq!(env.var(SETTING).as_deref(), Some("2G"));
            let resolved = crate::context::with_env(env, limit);
            assert_eq!(resolved, Ok(Some(2 * GIB)));
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managers_open_applies_oom_score_adj_and_memory_limit() {
        use std::time::Duration;

        use arko_agents::TaskContext;
        use arko_agents::terminal::{JobState, StartOptions};
        use uuid::Uuid;

        use crate::harness::Managers;
        use crate::session::Session;
        use crate::shell::{Shell, WindowsShell};

        let temp = tempfile::tempdir().unwrap();
        let session = Session::create(&temp.path().join("arko")).unwrap();
        let shell = Shell::detect(WindowsShell::Cmd);

        // When ARKO_COMMAND_MEMORY_LIMIT is not set in the test process, test
        // both default and `.env` overrides through `context::with_env`.
        let env_unset = std::env::var_os(SETTING).is_none();
        if env_unset {
            // Invalid override fails cleanly at open.
            let bad_dir = temp.path().join("bad");
            std::fs::create_dir_all(&bad_dir).unwrap();
            std::fs::write(bad_dir.join(".env"), "ARKO_COMMAND_MEMORY_LIMIT=bogus\n").unwrap();
            let bad_env = credentials::Env::for_folder_in(Some(&bad_dir), false, None);
            let err = crate::context::with_env(bad_env, || Managers::open(&session, &shell))
                .err()
                .expect("invalid ARKO_COMMAND_MEMORY_LIMIT should fail");
            assert!(err.contains(SETTING) && err.contains("`bogus`"), "{err}");
        }

        // Default open applies `oom_score_adj = 500` and (when systemd user
        // scopes are available) the configured `MemoryMax` with `MemorySwapMax=0`.
        let expected_limit = limit().unwrap();
        assert!(
            expected_limit.is_some(),
            "/proc/meminfo should yield a limit on Linux"
        );
        let managers = Managers::open(&session, &shell).unwrap();
        let owner = Uuid::new_v4();
        let context = TaskContext::new(owner, "main", "test", temp.path());
        let command = r#"printf "oom=%s\n" "$(cat /proc/self/oom_score_adj)"; cg=$(awk -F: '$1==0{print $3}' /proc/self/cgroup); if [ -r "/sys/fs/cgroup$cg/memory.max" ]; then printf "cg=%s\nmax=%s\nswap=%s\n" "$cg" "$(cat "/sys/fs/cgroup$cg/memory.max")" "$(cat "/sys/fs/cgroup$cg/memory.swap.max")"; fi"#;
        let started = managers
            .jobs
            .start(
                &context,
                command,
                StartOptions {
                    terminal: false,
                    detect_prompts: false,
                    timeout: Some(Duration::from_secs(10)),
                    held: true,
                    shell_args: Some(shell.args.clone()),
                    label: None,
                },
            )
            .unwrap();
        let mut rev = started.revision;
        let done = loop {
            let snap = managers
                .jobs
                .wait(owner, started.job_id, rev, Duration::from_secs(10))
                .await
                .unwrap();
            if snap.state.is_terminal() {
                break snap;
            }
            rev = snap.revision;
        };
        assert_eq!(done.state, JobState::Succeeded, "{:?}", done.output.text);
        let output = std::fs::read_to_string(&done.log_path).unwrap();
        assert!(
            output.lines().any(|line| line == "oom=500"),
            "expected oom=500, got:\n{output}"
        );
        if output.lines().any(|line| {
            line.starts_with("cg=") && line.ends_with(".scope") && line.contains("/run-")
        }) {
            // Linux cgroup v2 `memory.max` stores whole pages (rounding down
            // to the system page size).
            // SAFETY: `sysconf(_SC_PAGESIZE)` reads a constant of the system.
            let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
            let expected = expected_limit.unwrap();
            let expected_aligned = expected - (expected % page);
            let actual: u64 = output
                .lines()
                .find_map(|line| line.strip_prefix("max="))
                .expect("max= line missing")
                .parse()
                .unwrap();
            assert_eq!(
                actual, expected_aligned,
                "expected page-aligned memory.max={expected_aligned} (from {expected}), got:\n{output}"
            );
            assert!(
                output.lines().any(|line| line == "swap=0"),
                "expected swap=0, got:\n{output}"
            );
        }
        managers.shutdown().await;

        // Explicit override applies that exact limit and keeps `oom_score_adj = 500`.
        if env_unset {
            let cap_dir = temp.path().join("cap");
            std::fs::create_dir_all(&cap_dir).unwrap();
            std::fs::write(cap_dir.join(".env"), "ARKO_COMMAND_MEMORY_LIMIT=256M\n").unwrap();
            let cap_env = credentials::Env::for_folder_in(Some(&cap_dir), false, None);
            let session_cap = Session::create(&temp.path().join("arko-cap")).unwrap();
            let managers =
                crate::context::with_env(cap_env, || Managers::open(&session_cap, &shell)).unwrap();
            let started = managers
                .jobs
                .start(
                    &context,
                    command,
                    StartOptions {
                        terminal: false,
                        detect_prompts: false,
                        timeout: Some(Duration::from_secs(10)),
                        held: true,
                        shell_args: Some(shell.args.clone()),
                        label: None,
                    },
                )
                .unwrap();
            let mut rev = started.revision;
            let done = loop {
                let snap = managers
                    .jobs
                    .wait(owner, started.job_id, rev, Duration::from_secs(10))
                    .await
                    .unwrap();
                if snap.state.is_terminal() {
                    break snap;
                }
                rev = snap.revision;
            };
            assert_eq!(done.state, JobState::Succeeded);
            let output = std::fs::read_to_string(&done.log_path).unwrap();
            assert!(
                output.lines().any(|line| line == "oom=500"),
                "expected oom=500 with override, got:\n{output}"
            );
            if output.lines().any(|line| {
                line.starts_with("cg=") && line.ends_with(".scope") && line.contains("/run-")
            }) {
                let expected = (256 * MIB).to_string();
                assert!(
                    output.lines().any(|line| line == format!("max={expected}")),
                    "expected max={expected} with 256M override, got:\n{output}"
                );
                assert!(
                    output.lines().any(|line| line == "swap=0"),
                    "expected swap=0 with 256M override, got:\n{output}"
                );
            }
            managers.shutdown().await;
        }

        // Disabling the hard cap with `off` keeps `oom_score_adj = 500`.
        if env_unset {
            let off_dir = temp.path().join("off");
            std::fs::create_dir_all(&off_dir).unwrap();
            std::fs::write(off_dir.join(".env"), "ARKO_COMMAND_MEMORY_LIMIT=off\n").unwrap();
            let off_env = credentials::Env::for_folder_in(Some(&off_dir), false, None);
            let session_off = Session::create(&temp.path().join("arko-off")).unwrap();
            let managers =
                crate::context::with_env(off_env, || Managers::open(&session_off, &shell)).unwrap();
            let started = managers
                .jobs
                .start(
                    &context,
                    "cat /proc/self/oom_score_adj",
                    StartOptions {
                        terminal: false,
                        detect_prompts: false,
                        timeout: Some(Duration::from_secs(10)),
                        held: true,
                        shell_args: Some(shell.args.clone()),
                        label: None,
                    },
                )
                .unwrap();
            let mut rev = started.revision;
            let done = loop {
                let snap = managers
                    .jobs
                    .wait(owner, started.job_id, rev, Duration::from_secs(10))
                    .await
                    .unwrap();
                if snap.state.is_terminal() {
                    break snap;
                }
                rev = snap.revision;
            };
            assert_eq!(done.state, JobState::Succeeded);
            let output = std::fs::read_to_string(&done.log_path).unwrap();
            assert_eq!(output.trim(), "500");
            managers.shutdown().await;
        }
    }
}
