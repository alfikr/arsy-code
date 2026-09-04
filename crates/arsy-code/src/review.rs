use arsy_kernel::domain::ResourceRef;
use serde::{Deserialize, Serialize};
use std::fmt;

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
    pub relevant_coverage_percent: u8,
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
        || input.relevant_coverage_percent < 50
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
            relevant_coverage_percent: 100,
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
}
