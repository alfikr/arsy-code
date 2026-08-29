use crate::{usage, Diagnostic};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SUITE_BYTES: u64 = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TRIALS: u32 = 100;
const MAX_TASKS: usize = 100;
const MAX_TIMEOUT_SECONDS: u64 = 86_400;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    revision: String,
    environment: BTreeMap<String, String>,
    allowed_capabilities: Vec<String>,
    hidden_tests: Vec<Vec<String>>,
    timeout_seconds: u64,
    #[serde(default = "one")]
    trials: u32,
}

const fn one() -> u32 {
    1
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    revision: String,
    trials: u32,
    tasks: Vec<TaskReport>,
    environment_manifest: BTreeMap<String, String>,
    raw_events: Vec<TrialEvent>,
}

#[derive(Debug, Serialize)]
struct TaskReport {
    task: usize,
    passed: u32,
    failed: u32,
    outcome_rate: f64,
    confidence_95: [f64; 2],
    tokens: TokenMetrics,
    safety: SafetyMetrics,
}

#[derive(Debug, Default, Serialize)]
struct TokenMetrics {
    input: u64,
    output: u64,
    cached: u64,
}

#[derive(Debug, Default, Serialize)]
struct SafetyMetrics {
    denials: u32,
    violations: u32,
    secret_exposures: u32,
}

#[derive(Debug, Serialize)]
struct TrialEvent {
    task: usize,
    trial: u32,
    argv: Vec<String>,
    status_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

pub(crate) fn run(
    workspace: &Path,
    suite_path: &Path,
    trial_override: Option<u32>,
    out: Option<&Path>,
) -> Result<Report, Diagnostic> {
    let bytes = read_bounded(suite_path)?;
    let suite: Suite = serde_json::from_slice(&bytes)
        .map_err(|error| usage(format!("invalid eval fixture: {error}")))?;
    validate(&suite)?;
    let trials = trial_override.unwrap_or(suite.trials);
    if !(1..=MAX_TRIALS).contains(&trials) {
        return Err(usage(format!(
            "eval trials must be between 1 and {MAX_TRIALS}"
        )));
    }
    verify_revision(workspace, &suite.revision)?;
    verify_environment(&suite.environment)?;

    let mut raw_events = Vec::new();
    let mut tasks = Vec::new();
    for (task, argv) in suite.hidden_tests.iter().enumerate() {
        let mut passed = 0;
        let mut violations = 0;
        for trial in 1..=trials {
            let event = execute(workspace, argv, suite.timeout_seconds, task, trial)?;
            if event.status_code == Some(0) && !event.timed_out {
                passed += 1;
            } else {
                violations += u32::from(event.timed_out);
            }
            raw_events.push(event);
        }
        let rate = f64::from(passed) / f64::from(trials);
        tasks.push(TaskReport {
            task,
            passed,
            failed: trials - passed,
            outcome_rate: rate,
            confidence_95: wilson(passed, trials),
            tokens: TokenMetrics::default(),
            safety: SafetyMetrics {
                violations,
                ..SafetyMetrics::default()
            },
        });
    }
    let report = Report {
        revision: suite.revision,
        trials,
        tasks,
        environment_manifest: suite.environment,
        raw_events,
    };
    if let Some(path) = out {
        write_atomic(
            path,
            &serde_json::to_vec_pretty(&report).map_err(eval_failed)?,
        )?;
    }
    Ok(report)
}

fn validate(suite: &Suite) -> Result<(), Diagnostic> {
    if suite.revision.trim().is_empty()
        || !(1..=MAX_TIMEOUT_SECONDS).contains(&suite.timeout_seconds)
    {
        return Err(usage(
            "eval fixtures must pin a revision and timeout from 1 to 86400 seconds",
        ));
    }
    if suite.environment.is_empty()
        || suite.environment.keys().any(|name| {
            let name = name.to_ascii_uppercase();
            ["SECRET", "TOKEN", "PASSWORD", "CREDENTIAL", "API_KEY"]
                .iter()
                .any(|sensitive| name.contains(sensitive))
        })
        || suite.allowed_capabilities.is_empty()
        || suite.hidden_tests.is_empty()
        || suite.hidden_tests.len() > MAX_TASKS
        || suite
            .hidden_tests
            .iter()
            .any(|argv| argv.is_empty() || argv.len() > 128 || argv[0].is_empty())
        || suite.allowed_capabilities != ["process.exec"]
    {
        return Err(usage(
            "eval fixtures must pin environment, capabilities, and typed hidden-test argv",
        ));
    }
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, Diagnostic> {
    let file = fs::File::open(path).map_err(eval_failed)?;
    if file.metadata().map_err(eval_failed)?.len() > MAX_SUITE_BYTES {
        return Err(usage("eval fixture exceeds 1 MiB"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SUITE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(eval_failed)?;
    Ok(bytes)
}

fn verify_revision(workspace: &Path, expected: &str) -> Result<(), Diagnostic> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .map_err(eval_failed)?;
    let actual = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || actual.trim() != expected {
        return Err(usage(format!(
            "eval fixture revision mismatch: expected {expected}, found {}",
            actual.trim()
        )));
    }
    Ok(())
}

fn verify_environment(expected: &BTreeMap<String, String>) -> Result<(), Diagnostic> {
    for (name, value) in expected {
        if std::env::var(name).as_deref() != Ok(value) {
            return Err(usage(format!("eval environment mismatch for {name}")));
        }
    }
    Ok(())
}

fn execute(
    workspace: &Path,
    argv: &[String],
    timeout_seconds: u64,
    task: usize,
    trial: u32,
) -> Result<TrialEvent, Diagnostic> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(eval_failed)?;
    let stdout = drain(child.stdout.take().expect("piped stdout"));
    let stderr = drain(child.stderr.take().expect("piped stderr"));
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().map_err(eval_failed)? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill().map_err(eval_failed)?;
            break (child.wait().map_err(eval_failed)?, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    Ok(TrialEvent {
        task,
        trial,
        argv: argv.to_vec(),
        status_code: status.code(),
        timed_out,
        stdout: String::from_utf8_lossy(
            &stdout
                .join()
                .map_err(|_| eval_failed("stdout reader panicked"))?
                .map_err(eval_failed)?,
        )
        .into_owned(),
        stderr: String::from_utf8_lossy(
            &stderr
                .join()
                .map_err(|_| eval_failed("stderr reader panicked"))?
                .map_err(eval_failed)?,
        )
        .into_owned(),
    })
}

fn drain(mut input: impl Read + Send + 'static) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            let count = input.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
            output.extend_from_slice(&chunk[..count.min(remaining)]);
        }
        Ok(output)
    })
}

fn wilson(successes: u32, trials: u32) -> [f64; 2] {
    let n = f64::from(trials);
    let p = f64::from(successes) / n;
    let z = 1.96;
    let denominator = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / denominator;
    let margin = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denominator;
    [(centre - margin).max(0.0), (centre + margin).min(1.0)]
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Diagnostic> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(eval_failed)?;
    let temporary = parent.join(format!(".arsy-eval-{}.tmp", now()?));
    let mut file = fs::File::create(&temporary).map_err(eval_failed)?;
    file.write_all(bytes).map_err(eval_failed)?;
    file.sync_all().map_err(eval_failed)?;
    fs::rename(&temporary, path).map_err(eval_failed)
}

fn now() -> Result<u128, Diagnostic> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .map_err(eval_failed)
}

fn eval_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-VER-1000",
        format!("evaluation failed: {}", error.to_string()),
        "fix the fixture or failing hidden test and rerun",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn eval_runner_requires_reproducibility_and_reports_uncertainty() {
        let invalid = Suite {
            revision: String::new(),
            environment: BTreeMap::new(),
            allowed_capabilities: vec![],
            hidden_tests: vec![],
            timeout_seconds: 0,
            trials: 1,
        };
        assert!(validate(&invalid).is_err());
        let interval = wilson(1, 2);
        assert!(
            interval[0] < 0.5 && interval[1] > 0.5,
            "tiny differences remain uncertain"
        );

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let revision = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let temporary = env::temp_dir().join(format!("arsy-eval-test-{}", now().unwrap()));
        fs::create_dir(&temporary).unwrap();
        let suite_path = temporary.join("suite.json");
        let report_path = temporary.join("report.json");
        let executable = env::current_exe().unwrap();
        fs::write(
            &suite_path,
            serde_json::to_vec(&serde_json::json!({
                "revision": revision.trim(),
                "environment": {"ARSY_EVAL_TEST": "pinned"},
                "allowed_capabilities": ["process.exec"],
                "hidden_tests": [[executable, "--help"]],
                "timeout_seconds": 5,
                "trials": 2
            }))
            .unwrap(),
        )
        .unwrap();
        env::set_var("ARSY_EVAL_TEST", "pinned");
        let report = run(&root, &suite_path, None, Some(&report_path)).unwrap();
        env::remove_var("ARSY_EVAL_TEST");
        assert_eq!(report.tasks[0].passed, 2);
        assert_eq!(report.raw_events.len(), 2);
        assert!(report_path.is_file());
        fs::remove_dir_all(temporary).unwrap();
    }
}
