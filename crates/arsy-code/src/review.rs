//! What a change is, how much verification it deserves, and what to look at.
//!
//! # Why the depth is derived rather than chosen
//!
//! "Run the tests" is the wrong instruction twice over: for a typo it wastes
//! the turn, and for a migration it is not enough. The depth comes from
//! properties of the change that can be pointed at — how many files, how many
//! lines, whether anything was deleted, whether the tests moved with the code —
//! so a caller can disagree with the answer by disagreeing with a signal,
//! rather than with a number nobody can explain.
//!
//! # Why the findings are not prose
//!
//! A review that reads well and cannot be acted on costs a turn and changes
//! nothing. Every finding here names a file, states one fact about it, and
//! says what would settle it.

use arsy_kernel::domain::ResourceRef;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationClass {
    Read,
    ReversibleWrite,
    Process,
    Destructive,
    Irreversible,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RiskInput {
    pub operation: OperationClass,
    pub affected_resources: u32,
    pub changed_lines: u32,
    /// `None` when nothing measured it. Unknown coverage widens verification
    /// rather than narrowing it: a caller who has not measured is not thereby
    /// entitled to the benefit of the doubt.
    pub relevant_coverage_percent: Option<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDepth {
    None,
    Targeted,
    Workspace,
    FullWithApproval,
}

pub fn verification_depth(input: RiskInput) -> VerificationDepth {
    if matches!(
        input.operation,
        OperationClass::Destructive | OperationClass::Irreversible
    ) {
        return VerificationDepth::FullWithApproval;
    }
    if input.operation == OperationClass::Read {
        return VerificationDepth::None;
    }
    if input.affected_resources > 20
        || input.changed_lines > 1_000
        || input
            .relevant_coverage_percent
            .is_none_or(|percent| percent < 50)
    {
        VerificationDepth::Workspace
    } else {
        VerificationDepth::Targeted
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Correctness,
    Security,
    Compatibility,
    Performance,
    Verification,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewFinding {
    pub kind: FindingKind,
    pub message: String,
    pub evidence: Vec<ResourceRef>,
}

/// One file's part of a change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    /// The file is gone after the change, not merely emptied.
    pub deleted: bool,
    /// Removed lines that declared something public, in a language whose
    /// visibility this can read. The signal a compatibility finding rests on.
    pub removed_public_items: Vec<String>,
}

/// A change, assessed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Review {
    pub risk: RiskInput,
    pub depth: VerificationDepth,
    pub files: Vec<ChangedFile>,
    pub findings: Vec<ReviewFinding>,
}

/// Path segments whose contents decide whether something is allowed.
///
/// A change under one of these is not necessarily wrong; it is necessarily
/// worth a second pair of eyes, which is all the finding claims.
const SENSITIVE: &[&str] = &[
    "auth",
    "credential",
    "crypto",
    "keychain",
    "oauth",
    "permission",
    "policy",
    "sandbox",
    "secret",
    "token",
];

/// Read `git diff` output into per-file counts and the public items it removed.
///
/// Only the parts of the format that carry the signals below are read: a diff
/// parser complete enough to reconstruct the change would be a second patch
/// engine, and `apply_patch` already is one.
pub fn parse_diff(diff: &str) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = Vec::new();
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            // `a/path b/path`; the second half is the post-change name, which
            // is the one a reader is looking for even when the file moved.
            let path = header
                .split_once(" b/")
                .map(|(_, right)| right)
                .unwrap_or(header)
                .to_owned();
            files.push(ChangedFile {
                path,
                added: 0,
                removed: 0,
                deleted: false,
                removed_public_items: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("deleted file mode") {
            file.deleted = true;
            continue;
        }
        // `+++`/`---` are headers, not content.
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            file.added += 1;
        } else if let Some(removed) = line.strip_prefix('-') {
            file.removed += 1;
            if let Some(item) = public_item(removed) {
                file.removed_public_items.push(item);
            }
        }
    }
    files
}

/// The name a removed line declared publicly, if it declared one.
fn public_item(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("pub ")?;
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let rest = rest.strip_prefix("const ").unwrap_or(rest);
    let rest = rest.strip_prefix("unsafe ").unwrap_or(rest);
    let (keyword, tail) = rest.split_once(' ')?;
    if !matches!(
        keyword,
        "fn" | "struct" | "enum" | "trait" | "type" | "mod" | "const" | "static"
    ) {
        return None;
    }
    let name: String = tail
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect();
    (!name.is_empty()).then(|| format!("{keyword} {name}"))
}

/// Assess a parsed change: how much verification it needs, and what to look at.
///
/// `coverage_percent` is whatever measured the tested share of the changed
/// code, and `None` when nothing did.
pub fn assess(files: Vec<ChangedFile>, coverage_percent: Option<u8>) -> Review {
    let changed_lines = files
        .iter()
        .map(|file| file.added.saturating_add(file.removed))
        .fold(0u32, u32::saturating_add);
    let deleted: Vec<&ChangedFile> = files.iter().filter(|file| file.deleted).collect();
    let risk = RiskInput {
        operation: if deleted.is_empty() {
            OperationClass::ReversibleWrite
        } else {
            // A deletion is not recoverable from the working tree, whatever
            // version control could do about it afterwards.
            OperationClass::Destructive
        },
        affected_resources: u32::try_from(files.len()).unwrap_or(u32::MAX),
        changed_lines,
        relevant_coverage_percent: coverage_percent,
    };

    let mut findings = Vec::new();
    let evidence = |path: &str| {
        ResourceRef::new("workspace", path)
            .map(|reference| vec![reference])
            .unwrap_or_default()
    };

    for file in &deleted {
        findings.push(ReviewFinding {
            kind: FindingKind::Correctness,
            message: format!(
                "{} is deleted; confirm nothing still refers to it",
                file.path
            ),
            evidence: evidence(&file.path),
        });
    }

    for file in &files {
        let lowered = file.path.to_ascii_lowercase();
        let matched: BTreeSet<&str> = SENSITIVE
            .iter()
            .filter(|needle| lowered.contains(*needle))
            .copied()
            .collect();
        if !matched.is_empty() {
            findings.push(ReviewFinding {
                kind: FindingKind::Security,
                message: format!(
                    "{} decides {}; a negative-path test is the evidence this needs",
                    file.path,
                    matched.into_iter().collect::<Vec<_>>().join(", ")
                ),
                evidence: evidence(&file.path),
            });
        }
        if !file.removed_public_items.is_empty() {
            findings.push(ReviewFinding {
                kind: FindingKind::Compatibility,
                message: format!(
                    "{} removes or changes {}; check every caller outside this change",
                    file.path,
                    file.removed_public_items.join(", ")
                ),
                evidence: evidence(&file.path),
            });
        }
    }

    if !files.is_empty() && !files.iter().any(|file| is_test(&file.path)) {
        findings.push(ReviewFinding {
            kind: FindingKind::Verification,
            message: format!(
                "{} file(s) changed and no test did; name the check that would have failed before",
                files.len()
            ),
            evidence: Vec::new(),
        });
    }

    Review {
        risk,
        depth: verification_depth(risk),
        files,
        findings,
    }
}

/// Whether a path is a test by this repository's conventions, plus the two
/// every Rust and JavaScript project shares.
fn is_test(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    lowered.contains("/tests/")
        || lowered.starts_with("tests/")
        || lowered.contains("test_")
        || lowered.contains("_test.")
        || lowered.contains(".test.")
        || lowered.contains(".spec.")
}

#[derive(Clone, Debug)]
pub struct CompletionClaim {
    pub required_depth: VerificationDepth,
    pub executed_depth: VerificationDepth,
    pub evidence: Vec<ResourceRef>,
    pub approved: bool,
}

impl CompletionClaim {
    pub fn validate(&self) -> Result<(), ReviewError> {
        if self.executed_depth < self.required_depth {
            return Err(ReviewError::InsufficientDepth);
        }
        if self.required_depth != VerificationDepth::None && self.evidence.is_empty() {
            return Err(ReviewError::MissingEvidence);
        }
        if self.required_depth == VerificationDepth::FullWithApproval && !self.approved {
            return Err(ReviewError::ApprovalRequired);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewError {
    InsufficientDepth,
    MissingEvidence,
    ApprovalRequired,
}

impl fmt::Display for ReviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InsufficientDepth => {
                "recorded verification is shallower than change risk requires"
            }
            Self::MissingEvidence => "completion requires recorded verification evidence",
            Self::ApprovalRequired => "high-risk verification cannot replace approval",
        })
    }
}

impl std::error::Error for ReviewError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_selects_depth_and_completion_needs_evidence_and_approval() {
        let high = verification_depth(RiskInput {
            operation: OperationClass::Destructive,
            affected_resources: 1,
            changed_lines: 1,
            relevant_coverage_percent: Some(100),
        });
        assert_eq!(high, VerificationDepth::FullWithApproval);
        assert_eq!(
            CompletionClaim {
                required_depth: high,
                executed_depth: high,
                evidence: Vec::new(),
                approved: true
            }
            .validate(),
            Err(ReviewError::MissingEvidence)
        );
        let evidence = ResourceRef::new("artifact", "verification/1").unwrap();
        assert_eq!(
            CompletionClaim {
                required_depth: high,
                executed_depth: high,
                evidence: vec![evidence],
                approved: false
            }
            .validate(),
            Err(ReviewError::ApprovalRequired)
        );
    }

    const DIFF: &str = "\
diff --git a/crates/arsy-kernel/src/policy.rs b/crates/arsy-kernel/src/policy.rs
--- a/crates/arsy-kernel/src/policy.rs
+++ b/crates/arsy-kernel/src/policy.rs
@@ -1,4 +1,4 @@
-pub fn evaluate(query: &Query) -> Decision {
+pub fn evaluate(query: &Query, actor: &Actor) -> Decision {
     let mut decision = Decision::Deny;
+    let _ = actor;
diff --git a/crates/arsy-kernel/src/legacy.rs b/crates/arsy-kernel/src/legacy.rs
deleted file mode 100644
--- a/crates/arsy-kernel/src/legacy.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-pub struct Legacy;
";

    #[test]
    fn a_change_is_read_from_its_diff_and_answered_with_what_to_look_at() {
        let files = parse_diff(DIFF);

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "crates/arsy-kernel/src/policy.rs");
        assert_eq!(files[0].added, 2);
        assert_eq!(files[0].removed, 1);
        assert!(!files[0].deleted);
        assert_eq!(files[0].removed_public_items, ["fn evaluate"]);
        assert!(files[1].deleted);

        let review = assess(files, None);

        // A deletion is destructive, and destructive outranks every other
        // signal in the change.
        assert_eq!(review.risk.operation, OperationClass::Destructive);
        assert_eq!(review.depth, VerificationDepth::FullWithApproval);
        let kinds: Vec<FindingKind> = review.findings.iter().map(|found| found.kind).collect();
        assert!(kinds.contains(&FindingKind::Correctness), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::Security), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::Compatibility), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::Verification), "{kinds:?}");
        // Every finding names the file it is about, so it can be acted on
        // without re-reading the diff.
        assert!(review
            .findings
            .iter()
            .filter(|found| found.kind != FindingKind::Verification)
            .all(|found| found.message.contains(".rs")));
    }

    #[test]
    fn a_small_tested_change_asks_for_the_tests_it_already_has() {
        let review = assess(
            parse_diff(
                "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@
-let x = 1;
+let x = 2;
diff --git a/tests/lib.rs b/tests/lib.rs
--- a/tests/lib.rs
+++ b/tests/lib.rs
@@
+assert_eq!(x, 2);
",
            ),
            Some(90),
        );

        assert_eq!(review.risk.operation, OperationClass::ReversibleWrite);
        assert_eq!(review.depth, VerificationDepth::Targeted);
        assert!(review.findings.is_empty(), "{:?}", review.findings);
    }

    #[test]
    fn unmeasured_coverage_widens_verification_rather_than_narrowing_it() {
        let one_line = parse_diff(
            "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@
+let x = 2;
diff --git a/tests/lib.rs b/tests/lib.rs
--- a/tests/lib.rs
+++ b/tests/lib.rs
@@
+assert!(true);
",
        );

        assert_eq!(
            assess(one_line.clone(), None).depth,
            VerificationDepth::Workspace
        );
        assert_eq!(
            assess(one_line, Some(80)).depth,
            VerificationDepth::Targeted
        );
    }
}
