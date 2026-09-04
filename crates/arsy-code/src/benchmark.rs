//! Stable benchmark dimensions and regression math shared by CI and benches.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Temperature {
    Cold,
    Warm,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositorySize {
    Small,
    Medium,
    Large,
}

impl RepositorySize {
    pub const fn files(self) -> usize {
        match self {
            Self::Small => 16,
            Self::Medium => 128,
            Self::Large => 1_024,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BenchmarkSample {
    pub operation: String,
    pub repository_size: RepositorySize,
    pub temperature: Temperature,
    pub location: Location,
    pub elapsed_ns: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegressionThreshold {
    pub baseline_ns: u64,
    pub max_regression_percent: u16,
}

impl RegressionThreshold {
    pub fn accepts(self, elapsed_ns: u64) -> bool {
        let permitted = u128::from(self.baseline_ns)
            .saturating_mul(u128::from(100 + u32::from(self.max_regression_percent)))
            / 100;
        u128::from(elapsed_ns) <= permitted
    }

    pub fn for_sample(sample: &BenchmarkSample) -> Self {
        let files = sample.repository_size.files() as u64;
        let baseline_ns = match sample.operation.as_str() {
            "search" => 5_000_000 + files * 50_000,
            "edit" => 5_000_000 + files * 150_000,
            "retrieval" => 1_000_000 + files * 20_000,
            _ => 0,
        } * if sample.temperature == Temperature::Cold {
            2
        } else {
            1
        };
        Self {
            baseline_ns,
            max_regression_percent: 25,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_are_explicit_and_regression_math_does_not_overflow() {
        assert!(RepositorySize::Small.files() < RepositorySize::Large.files());
        let threshold = RegressionThreshold {
            baseline_ns: u64::MAX,
            max_regression_percent: 10,
        };
        assert!(threshold.accepts(u64::MAX));
        assert!(RegressionThreshold {
            baseline_ns: 100,
            max_regression_percent: 5,
        }
        .accepts(105));
        assert!(!RegressionThreshold {
            baseline_ns: 100,
            max_regression_percent: 5,
        }
        .accepts(106));
        let sample = BenchmarkSample {
            operation: "search".into(),
            repository_size: RepositorySize::Large,
            temperature: Temperature::Cold,
            location: Location::Remote,
            elapsed_ns: 1,
        };
        assert_eq!(
            RegressionThreshold::for_sample(&sample).baseline_ns,
            112_400_000
        );
    }
}
