use crate::domain::{AgentId, SubscriptionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RedactedProjection {
    pub sequence: u64,
    pub kind: String,
    pub public_payload: Value,
    pub redacted_fields: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObserverAuthority {
    pub may_suggest: bool,
    pub may_deny: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObserverSubscription {
    pub id: SubscriptionId,
    pub observer: AgentId,
    pub authority: ObserverAuthority,
    pub cost_budget_micros: u64,
    pub cost_used_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum Intervention {
    Suggest(String),
    Deny(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObserverRuleEvent {
    pub subscription: SubscriptionId,
    pub projection_sequence: u64,
    pub intervention: Intervention,
    pub cost_micros: u64,
}

impl ObserverSubscription {
    pub fn intervene(
        &mut self,
        projection: &RedactedProjection,
        intervention: Intervention,
        cost_micros: u64,
    ) -> Result<ObserverRuleEvent, ObserverError> {
        let permitted = match intervention {
            Intervention::Suggest(_) => self.authority.may_suggest,
            Intervention::Deny(_) => self.authority.may_deny,
        };
        if !permitted {
            return Err(ObserverError::Unauthorized);
        }
        let used = self
            .cost_used_micros
            .checked_add(cost_micros)
            .ok_or(ObserverError::CostExceeded)?;
        if used > self.cost_budget_micros {
            return Err(ObserverError::CostExceeded);
        }
        self.cost_used_micros = used;
        Ok(ObserverRuleEvent {
            subscription: self.id,
            projection_sequence: projection.sequence,
            intervention,
            cost_micros,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverError {
    Unauthorized,
    CostExceeded,
}

impl fmt::Display for ObserverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unauthorized => "observer is not authorized for this intervention",
            Self::CostExceeded => "observer cost budget exceeded",
        })
    }
}

impl std::error::Error for ObserverError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observers_only_consume_redacted_bounded_projections_and_denial_is_separate() {
        let projection = RedactedProjection {
            sequence: 7,
            kind: "operation.requested".into(),
            public_payload: json!({"path": "[redacted]"}),
            redacted_fields: 1,
        };
        let mut observer = ObserverSubscription {
            id: SubscriptionId::new(),
            observer: AgentId::new(),
            authority: ObserverAuthority {
                may_suggest: true,
                may_deny: false,
            },
            cost_budget_micros: 5,
            cost_used_micros: 0,
        };
        assert!(observer
            .intervene(&projection, Intervention::Suggest("run tests".into()), 3)
            .is_ok());
        assert_eq!(
            observer.intervene(&projection, Intervention::Deny("unsafe".into()), 1),
            Err(ObserverError::Unauthorized)
        );
        assert_eq!(
            observer.intervene(&projection, Intervention::Suggest("again".into()), 3),
            Err(ObserverError::CostExceeded)
        );
    }
}
