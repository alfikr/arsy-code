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
    /// Configurations to run every task under.
    ///
    /// A gate phrased as "beats the baseline" is a comparison, and a runner
    /// that could only measure one configuration could never answer it. With
    /// no arms declared there is one unnamed arm, which is what a suite that
    /// only wants a pass rate means.
    #[serde(default)]
    arms: Vec<Arm>,
}

/// One configuration a task is measured under.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arm {
    name: String,
    /// Environment this arm adds. `ARSY_CONFIG_HOME` is how an arm points the
    /// harness at a different configuration without editing the operator's.
    #[serde(default)]
    environment: BTreeMap<String, String>,
    /// Arguments appended to each task's argv, so one task definition can be
    /// asked the same question two ways.
    #[serde(default)]
    arguments: Vec<String>,
}

impl Arm {
    fn unnamed() -> Self {
        Self {
            name: "default".to_owned(),
            environment: BTreeMap::new(),
            arguments: Vec::new(),
        }
    }
}

const fn one() -> u32 {
    1
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    revision: String,
    trials: u32,
    tasks: Vec<TaskReport>,
    /// Every arm against the first one declared, which is the baseline by
    /// position: a comparison needs something to be compared to, and naming it
    /// by order is one less thing a fixture can get wrong.
    comparisons: Vec<Comparison>,
    environment_manifest: BTreeMap<String, String>,
    raw_events: Vec<TrialEvent>,
}

#[derive(Debug, Serialize)]
struct TaskReport {
    task: usize,
    arm: String,
    passed: u32,
    failed: u32,
    outcome_rate: f64,
    confidence_95: [f64; 2],
    tokens: TokenMetrics,
    safety: SafetyMetrics,
    /// Successes per thousand tokens spent. `None` when the tasks reported no
    /// token usage, which is not the same as having spent none.
    success_per_thousand_tokens: Option<f64>,
}

/// One arm measured against the baseline arm.
#[derive(Debug, Serialize)]
struct Comparison {
    arm: String,
    baseline: String,
    /// Outcome rate across every task, per arm.
    rate: f64,
    baseline_rate: f64,
    /// Positive means this arm succeeded more often than the baseline.
    rate_delta: f64,
    tokens: u64,
    baseline_tokens: u64,
    /// Whether the arm's 95% interval clears the baseline's entirely. Anything
    /// less is a difference the trials cannot distinguish from noise, and
    /// saying so is the point of reporting an interval at all.
    beats_baseline: bool,
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
    arm: String,
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

    let arms = if suite.arms.is_empty() {
        vec![Arm::unnamed()]
    } else {
        suite.arms.clone()
    };

    let mut raw_events = Vec::new();
    let mut tasks = Vec::new();
    for arm in &arms {
        for (task, argv) in suite.hidden_tests.iter().enumerate() {
            let mut passed = 0;
            let mut violations = 0;
            let mut tokens = TokenMetrics::default();
            let mut safety = SafetyMetrics::default();
            for trial in 1..=trials {
                let event = execute(workspace, argv, arm, suite.timeout_seconds, task, trial)?;
                if event.status_code == Some(0) && !event.timed_out {
                    passed += 1;
                } else {
                    violations += u32::from(event.timed_out);
                }
                // What the run said it spent, rather than a zero standing in
                // for a number nobody collected.
                measure(&event.stdout, &mut tokens, &mut safety);
                raw_events.push(event);
            }
            safety.violations += violations;
            let rate = f64::from(passed) / f64::from(trials);
            let spent = tokens.input + tokens.output;
            tasks.push(TaskReport {
                task,
                arm: arm.name.clone(),
                passed,
                failed: trials - passed,
                outcome_rate: rate,
                confidence_95: wilson(passed, trials),
                success_per_thousand_tokens: (spent > 0)
                    .then(|| f64::from(passed) * 1000.0 / spent as f64),
                tokens,
                safety,
            });
        }
    }
    let comparisons = compare(&tasks, &arms, trials);
    let report = Report {
        revision: suite.revision,
        trials,
        tasks,
        comparisons,
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

/// Read what a trial spent out of ARSY's own machine records.
///
/// A task's argv is whatever the fixture says, so most trials say nothing
/// about tokens and leave these at zero. A trial that ran `arsy ... --output
/// json` reported its telemetry, and reading it is the difference between a
/// success rate and a success-per-token.
fn measure(stdout: &str, tokens: &mut TokenMetrics, safety: &mut SafetyMetrics) {
    for line in stdout.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match record["type"].as_str() {
            Some("result") => {
                let telemetry = &record["payload"]["telemetry"];
                tokens.input += telemetry["input_tokens"].as_u64().unwrap_or(0);
                tokens.output += telemetry["output_tokens"].as_u64().unwrap_or(0);
            }
            // A refusal is a safety outcome, not a failure to be averaged into
            // the rate: a run that was stopped by policy did the right thing.
            Some("diagnostic") => {
                let code = record["payload"]["code"].as_str().unwrap_or_default();
                if code.contains("-POL-") {
                    safety.denials += 1;
                }
                if code.contains("-SEC-") || code.contains("-CRD-") {
                    safety.secret_exposures += 1;
                }
            }
            _ => {}
        }
    }
}

/// Every arm against the first, with an interval that has to clear it.
fn compare(tasks: &[TaskReport], arms: &[Arm], trials: u32) -> Vec<Comparison> {
    let Some(baseline) = arms.first() else {
        return Vec::new();
    };
    let totals = |arm: &str| {
        let rows: Vec<&TaskReport> = tasks.iter().filter(|task| task.arm == arm).collect();
        let passed: u32 = rows.iter().map(|task| task.passed).sum();
        let attempts = u32::try_from(rows.len()).unwrap_or(0) * trials;
        let tokens: u64 = rows
            .iter()
            .map(|task| task.tokens.input + task.tokens.output)
            .sum();
        (passed, attempts, tokens)
    };
    let (base_passed, base_attempts, base_tokens) = totals(&baseline.name);
    let base_rate = rate(base_passed, base_attempts);
    let base_interval = wilson(base_passed, base_attempts.max(1));

    arms.iter()
        .skip(1)
        .map(|arm| {
            let (passed, attempts, tokens) = totals(&arm.name);
            let interval = wilson(passed, attempts.max(1));
            Comparison {
                arm: arm.name.clone(),
                baseline: baseline.name.clone(),
                rate: rate(passed, attempts),
                baseline_rate: base_rate,
                rate_delta: rate(passed, attempts) - base_rate,
                tokens,
                baseline_tokens: base_tokens,
                beats_baseline: interval[0] > base_interval[1],
            }
        })
        .collect()
}

fn rate(passed: u32, attempts: u32) -> f64 {
    if attempts == 0 {
        0.0
    } else {
        f64::from(passed) / f64::from(attempts)
    }
}

fn execute(
    workspace: &Path,
    argv: &[String],
    arm: &Arm,
    timeout_seconds: u64,
    task: usize,
    trial: u32,
) -> Result<TrialEvent, Diagnostic> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .args(&arm.arguments)
        .envs(&arm.environment)
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
        arm: arm.name.clone(),
        trial,
        argv: argv.iter().chain(arm.arguments.iter()).cloned().collect(),
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
            arms: Vec::new(),
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

    #[test]
    fn an_arm_is_only_better_when_its_interval_clears_the_baseline() {
        let arms = vec![
            Arm {
                name: "text".to_owned(),
                ..Arm::unnamed()
            },
            Arm {
                name: "semantic".to_owned(),
                ..Arm::unnamed()
            },
            Arm {
                name: "noise".to_owned(),
                ..Arm::unnamed()
            },
        ];
        let task = |arm: &str, passed: u32, tokens: u64| TaskReport {
            task: 0,
            arm: arm.to_owned(),
            passed,
            failed: 20 - passed,
            outcome_rate: f64::from(passed) / 20.0,
            confidence_95: wilson(passed, 20),
            tokens: TokenMetrics {
                input: tokens,
                output: 0,
                cached: 0,
            },
            safety: SafetyMetrics::default(),
            success_per_thousand_tokens: None,
        };

        let comparisons = compare(
            &[
                task("text", 4, 1_000),
                // A clear win: 19 of 20 against 4 of 20.
                task("semantic", 19, 800),
                // Better on the count, but the intervals still overlap.
                task("noise", 7, 900),
            ],
            &arms,
            20,
        );

        assert_eq!(comparisons.len(), 2, "every arm but the baseline");
        let semantic = &comparisons[0];
        assert_eq!(semantic.baseline, "text");
        assert!(semantic.rate_delta > 0.7);
        assert!(semantic.beats_baseline);
        // Fewer tokens for more successes is the whole point of reporting both.
        assert!(semantic.tokens < semantic.baseline_tokens);

        let noise = &comparisons[1];
        assert!(noise.rate_delta > 0.0, "it did win more trials");
        assert!(
            !noise.beats_baseline,
            "a difference these trials cannot distinguish is not a win"
        );
    }

    #[test]
    fn a_trials_own_records_are_what_its_tokens_and_denials_come_from() {
        let mut tokens = TokenMetrics::default();
        let mut safety = SafetyMetrics::default();
        measure(
            &[
                r#"{"type":"diagnostic","payload":{"code":"ARSY-POL-1000","message":"refused"}}"#,
                r#"{"type":"model.delta","payload":{"text":"thinking"}}"#,
                r#"{"type":"result","payload":{"telemetry":{"input_tokens":120,"output_tokens":34}}}"#,
                "not json at all",
            ]
            .join("\n"),
            &mut tokens,
            &mut safety,
        );

        assert_eq!(tokens.input, 120);
        assert_eq!(tokens.output, 34);
        assert_eq!(safety.denials, 1, "a policy refusal is a safety outcome");

        // A task that is not an ARSY invocation reports nothing rather than
        // zero-as-a-measurement.
        let mut quiet = TokenMetrics::default();
        measure("PASS\n", &mut quiet, &mut SafetyMetrics::default());
        assert_eq!(quiet.input + quiet.output, 0);
    }
}
