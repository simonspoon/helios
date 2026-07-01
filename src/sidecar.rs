//! Roslyn sidecar: locate, probe, and run the `helios-roslyn` helper.
//!
//! Same architectural slot as `git.rs`: a child-process wrapper that degrades,
//! never panics the run. Detection happens once per `helios init` (P3-M1); any
//! failure in the ladder — dotnet absent, helper missing, `ping` fails,
//! `analyze` errors or times out — falls back to the tree-sitter path with a
//! single `warning:` line (P3-M2). The wire contract is NDJSON on stdout, one
//! `type`-discriminated object per line; diagnostics on stderr only.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Version floor: semantic mode requires a .NET runtime major at/above this
/// (architect decision on spec A2: .NET 8.0 LTS).
const DOTNET_MAJOR_FLOOR: u64 = 8;

/// Wall-clock timeout for `analyze` (P3-S1); a hung helper degrades instead of
/// blocking `init` indefinitely.
const ANALYZE_TIMEOUT: Duration = Duration::from_secs(120);

/// Wall-clock timeout for `ping`; a hung probe counts as a failed ping.
const PING_TIMEOUT: Duration = Duration::from_secs(15);

/// Parsed output of one `analyze` invocation.
#[derive(Debug, Default)]
pub struct AnalyzeOutput {
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
}

/// A `definition` record. Only the fields the semantic ingest needs are kept;
/// remaining wire fields (kind, start_col, visibility, scope, …) are ignored
/// per the forward-compat rule.
#[derive(Debug, Deserialize)]
pub struct Definition {
    pub docid: String,
    pub name: String,
    pub file: String,
    pub start_line: i64,
}

/// A `reference` record.
#[derive(Debug, Deserialize)]
pub struct Reference {
    pub docid: String,
    pub file: String,
    pub line: i64,
    pub col: i64,
    pub is_definition: bool,
}

/// A detected, ping-verified helper. Constructed only by `detect()`.
pub struct Sidecar {
    /// Program used to run the helper (`dotnet`; overridable in unit tests).
    program: OsString,
    /// Path to `helios-roslyn.dll`.
    dll: PathBuf,
}

/// Locate + ping + version-floor check. Called once per `helios init` run.
///
/// `None` means syntactic mode. When the fallback is caused by a *failure*
/// (as opposed to a clean not-found with no `HELIOS_ROSLYN` set), exactly one
/// `warning:` line is printed on stderr — the same channel as the indexer's
/// per-file warnings.
pub fn detect() -> Option<Sidecar> {
    let (candidate, probe) = probe();
    match decide(probe) {
        Decision::Semantic => candidate,
        Decision::Syntactic(Some(reason)) => {
            eprintln!("warning: {reason}");
            None
        }
        Decision::Syntactic(None) => None,
    }
}

impl Sidecar {
    /// One-shot `analyze` with a timeout. `Err` => caller falls back to the
    /// tree-sitter path (one warning).
    pub fn analyze(&self, root: &Path) -> Result<AnalyzeOutput> {
        self.analyze_with_timeout(root, ANALYZE_TIMEOUT)
    }

    fn analyze_with_timeout(&self, root: &Path, timeout: Duration) -> Result<AnalyzeOutput> {
        let mut cmd = Command::new(&self.program);
        cmd.arg(&self.dll).arg("analyze").arg("--root").arg(root);
        let out = match run_with_timeout(cmd, timeout) {
            Ok(out) => out,
            Err(RunError::Spawn(e)) => anyhow::bail!("could not run dotnet: {e}"),
            Err(RunError::Wait(e)) => anyhow::bail!("waiting on helper: {e}"),
            Err(RunError::TimedOut) => {
                anyhow::bail!("analyze timed out after {}s", timeout.as_secs())
            }
        };
        if !out.status.success() {
            anyhow::bail!(
                "analyze exited with {}: {}",
                out.status,
                one_line(&out.stderr)
            );
        }
        let (parsed, warnings) = parse_analyze(&out.stdout)?;
        // Non-fatal per-document diagnostics from an otherwise-good run are
        // forwarded; they do not affect mode.
        for w in warnings {
            eprintln!("warning: helios-roslyn: {w}");
        }
        Ok(parsed)
    }

    fn ping_probe(&self) -> ProbeResult {
        let mut cmd = Command::new(&self.program);
        cmd.arg(&self.dll).arg("ping");
        match run_with_timeout(cmd, PING_TIMEOUT) {
            Err(RunError::Spawn(e)) => ProbeResult::PingSpawnFailed(e.to_string()),
            Err(RunError::Wait(e)) => ProbeResult::PingFailed(e.to_string()),
            Err(RunError::TimedOut) => ProbeResult::PingFailed(format!(
                "timed out after {}s",
                PING_TIMEOUT.as_secs()
            )),
            Ok(out) if !out.status.success() => ProbeResult::PingFailed(format!(
                "exited with {}: {}",
                out.status,
                one_line(&out.stderr)
            )),
            Ok(out) => match parse_ping(&out.stdout) {
                Some(record) => ProbeResult::Ping(record),
                None => ProbeResult::PingFailed("no parseable ping record on stdout".into()),
            },
        }
    }
}

/// The capability record `ping` emits.
#[derive(Debug, Deserialize)]
struct PingRecord {
    #[serde(rename = "type")]
    record_type: String,
    available: bool,
    dotnet_version: String,
}

/// Everything the decision function needs to know about the environment.
#[derive(Debug)]
enum ProbeResult {
    /// No `HELIOS_ROSLYN`, no DLL next to the binary — the common case on
    /// machines without the helper; falls back silently.
    DllNotFound,
    /// `HELIOS_ROSLYN` is set (authoritative) but the path does not exist.
    EnvDllMissing(String),
    /// `dotnet` itself could not be spawned (dotnet absent).
    PingSpawnFailed(String),
    /// `ping` ran but failed: non-zero exit, timeout, or unparseable output.
    PingFailed(String),
    /// `ping` exited 0 with a parseable record.
    Ping(PingRecord),
}

/// The semantic/syntactic choice. `Syntactic(Some(reason))` means the caller
/// prints exactly one `warning: <reason>` line.
#[derive(Debug, PartialEq)]
enum Decision {
    Semantic,
    Syntactic(Option<String>),
}

/// Pure decision function (P3-M1): semantic mode iff the DLL was found, `ping`
/// exited 0 with a parseable record, `available == true`, and the dotnet major
/// version is at/above the floor. Anything else → syntactic mode.
fn decide(probe: ProbeResult) -> Decision {
    match probe {
        ProbeResult::DllNotFound => Decision::Syntactic(None),
        ProbeResult::EnvDllMissing(path) => Decision::Syntactic(Some(format!(
            "HELIOS_ROSLYN is set but no helper exists at {path}; resolving C# references with tree-sitter"
        ))),
        ProbeResult::PingSpawnFailed(e) => Decision::Syntactic(Some(format!(
            "could not run dotnet for the helios-roslyn helper ({e}); resolving C# references with tree-sitter"
        ))),
        ProbeResult::PingFailed(detail) => Decision::Syntactic(Some(format!(
            "helios-roslyn ping failed ({detail}); resolving C# references with tree-sitter"
        ))),
        ProbeResult::Ping(record) => {
            if !record.available {
                return Decision::Syntactic(Some(
                    "helios-roslyn reports available=false; resolving C# references with tree-sitter".into(),
                ));
            }
            match dotnet_major(&record.dotnet_version) {
                Some(major) if major >= DOTNET_MAJOR_FLOOR => Decision::Semantic,
                _ => Decision::Syntactic(Some(format!(
                    ".NET runtime {} is below the required .NET {}; resolving C# references with tree-sitter",
                    record.dotnet_version, DOTNET_MAJOR_FLOOR
                ))),
            }
        }
    }
}

/// Locate the DLL and, if found, ping it. Returns the candidate sidecar (when
/// a DLL exists) alongside the probe result for `decide`.
fn probe() -> (Option<Sidecar>, ProbeResult) {
    let dll = match locate_dll() {
        Ok(Some(path)) => path,
        Ok(None) => return (None, ProbeResult::DllNotFound),
        Err(missing) => return (None, ProbeResult::EnvDllMissing(missing)),
    };
    let sidecar = Sidecar {
        program: OsString::from("dotnet"),
        dll,
    };
    let probe = sidecar.ping_probe();
    (Some(sidecar), probe)
}

/// Helper location (spec A1): `HELIOS_ROSLYN` env var is authoritative when
/// set (Err(path) if it points at nothing); otherwise `helios-roslyn.dll`
/// next to the helios binary; otherwise a clean not-found (Ok(None)).
fn locate_dll() -> Result<Option<PathBuf>, String> {
    if let Ok(path) = std::env::var("HELIOS_ROSLYN")
        && !path.is_empty()
    {
        let p = PathBuf::from(&path);
        if p.is_file() {
            return Ok(Some(p));
        }
        return Err(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join("helios-roslyn.dll");
        if p.is_file() {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// First line of stdout that parses as a `ping` record.
fn parse_ping(stdout: &str) -> Option<PingRecord> {
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<PingRecord>(line)
            && record.record_type == "ping"
        {
            return Some(record);
        }
    }
    None
}

/// Collapse child diagnostics to a single line so the degradation warning is
/// always exactly one `warning:` line on stderr.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Leading major version from a string like `"8.0.11"`.
fn dotnet_major(version: &str) -> Option<u64> {
    version.split('.').next()?.trim().parse().ok()
}

/// Parse an `analyze` NDJSON stream. Unknown `type` values are skipped
/// (forward compat); an unparseable line fails the whole invocation — there is
/// no partial-success protocol. `warning` records are returned for the caller
/// to forward to stderr.
fn parse_analyze(stdout: &str) -> Result<(AnalyzeOutput, Vec<String>)> {
    let mut output = AnalyzeOutput::default();
    let mut warnings = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("unparseable NDJSON line from helios-roslyn: {line}"))?;
        match value.get("type").and_then(|t| t.as_str()) {
            Some("definition") => output.definitions.push(
                serde_json::from_value(value).context("malformed definition record")?,
            ),
            Some("reference") => output.references.push(
                serde_json::from_value(value).context("malformed reference record")?,
            ),
            Some("warning") => {
                if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
                    warnings.push(message.to_string());
                }
            }
            _ => {} // unknown type: skip
        }
    }
    Ok((output, warnings))
}

struct RunOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

enum RunError {
    Spawn(std::io::Error),
    Wait(std::io::Error),
    TimedOut,
}

/// Run a command to completion under a wall-clock timeout, capturing stdout
/// and stderr. On expiry the child is killed and `TimedOut` returned.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<RunOutput, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;

    // Drain both pipes on threads so a chatty child can't deadlock the poll loop.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(RunError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Wait(e));
            }
        }
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(RunOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping(available: bool, dotnet_version: &str) -> ProbeResult {
        ProbeResult::Ping(PingRecord {
            record_type: "ping".into(),
            available,
            dotnet_version: dotnet_version.into(),
        })
    }

    fn assert_warns(decision: Decision) {
        match decision {
            Decision::Syntactic(Some(_)) => {}
            other => panic!("expected Syntactic with warning, got {other:?}"),
        }
    }

    // --- decision function, one test per input class (P3-M1) ---

    #[test]
    fn decide_dll_missing_is_silent_syntactic() {
        assert_eq!(decide(ProbeResult::DllNotFound), Decision::Syntactic(None));
    }

    #[test]
    fn decide_env_dll_missing_warns() {
        assert_warns(decide(ProbeResult::EnvDllMissing("/no/such.dll".into())));
    }

    #[test]
    fn decide_dotnet_absent_warns() {
        assert_warns(decide(ProbeResult::PingSpawnFailed(
            "No such file or directory".into(),
        )));
    }

    #[test]
    fn decide_ping_failure_warns() {
        assert_warns(decide(ProbeResult::PingFailed("exited with 1".into())));
    }

    #[test]
    fn decide_available_false_warns() {
        assert_warns(decide(ping(false, "9.0.0")));
    }

    #[test]
    fn decide_runtime_below_floor_warns() {
        assert_warns(decide(ping(true, "7.0.20")));
    }

    #[test]
    fn decide_unparseable_version_warns() {
        assert_warns(decide(ping(true, "not-a-version")));
    }

    #[test]
    fn decide_all_good_is_semantic() {
        assert_eq!(decide(ping(true, "8.0.11")), Decision::Semantic);
        assert_eq!(decide(ping(true, "10.0.8")), Decision::Semantic);
    }

    // --- wire parsing ---

    #[test]
    fn parse_ping_reads_capability_record() {
        let record = parse_ping(
            "{\"type\":\"ping\",\"available\":true,\"dotnet_version\":\"8.0.11\",\"roslyn_version\":\"4.11.0\"}\n",
        )
        .expect("ping record");
        assert!(record.available);
        assert_eq!(record.dotnet_version, "8.0.11");
    }

    #[test]
    fn parse_ping_rejects_garbage() {
        assert!(parse_ping("").is_none());
        assert!(parse_ping("not json\n").is_none());
        assert!(parse_ping("{\"type\":\"other\"}\n").is_none());
    }

    #[test]
    fn parse_analyze_reads_records_skips_unknown_collects_warnings() {
        let stdout = concat!(
            "{\"type\":\"definition\",\"docid\":\"M:App.Person.Greet\",\"name\":\"Greet\",\"kind\":\"fn\",\"file\":\"Person.cs\",\"start_line\":3,\"start_col\":21,\"end_line\":3,\"visibility\":\"pub\",\"scope\":\"Person\"}\n",
            "{\"type\":\"reference\",\"docid\":\"M:App.Person.Greet\",\"file\":\"Program.cs\",\"line\":5,\"col\":13,\"is_definition\":false}\n",
            "{\"type\":\"warning\",\"message\":\"could not load obj/Gen.cs\"}\n",
            "{\"type\":\"future-record\",\"anything\":1}\n",
        );
        let (output, warnings) = parse_analyze(stdout).expect("parse");
        assert_eq!(output.definitions.len(), 1);
        assert_eq!(output.definitions[0].docid, "M:App.Person.Greet");
        assert_eq!(output.definitions[0].start_line, 3);
        assert_eq!(output.references.len(), 1);
        assert!(!output.references[0].is_definition);
        assert_eq!(warnings, vec!["could not load obj/Gen.cs".to_string()]);
    }

    #[test]
    fn parse_analyze_fails_on_unparseable_line() {
        assert!(parse_analyze("this is not json\n").is_err());
    }

    // --- analyze invocation: timeout + degradation (P3-S1), no dotnet needed ---

    #[cfg(unix)]
    fn script_sidecar(dir: &Path, body: &str) -> Sidecar {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-helper.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Sidecar {
            program: script.into_os_string(),
            dll: PathBuf::from("unused.dll"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn analyze_hung_helper_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = script_sidecar(dir.path(), "sleep 30");
        let start = Instant::now();
        let err = sidecar
            .analyze_with_timeout(dir.path(), Duration::from_millis(250))
            .expect_err("hung helper must degrade");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timeout did not fire promptly"
        );
        assert!(
            err.to_string().contains("timed out"),
            "unexpected error: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn analyze_nonzero_exit_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = script_sidecar(dir.path(), "echo 'load failure' >&2; exit 3");
        let err = sidecar
            .analyze_with_timeout(dir.path(), Duration::from_secs(10))
            .expect_err("non-zero exit must degrade");
        assert!(
            err.to_string().contains("load failure"),
            "unexpected error: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn analyze_success_parses_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = script_sidecar(
            dir.path(),
            "echo '{\"type\":\"definition\",\"docid\":\"T:App.Person\",\"name\":\"Person\",\"file\":\"Person.cs\",\"start_line\":2}'\n\
             echo '{\"type\":\"reference\",\"docid\":\"T:App.Person\",\"file\":\"Program.cs\",\"line\":4,\"col\":21,\"is_definition\":false}'",
        );
        let output = sidecar
            .analyze_with_timeout(dir.path(), Duration::from_secs(10))
            .expect("analyze");
        assert_eq!(output.definitions.len(), 1);
        assert_eq!(output.references.len(), 1);
    }
}
