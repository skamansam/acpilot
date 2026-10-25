//! The error envelope every endpoint returns on failure.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Machine-readable error payload.
///
/// `code` is a stable, documented identifier (`invalid_request`, `not_found`,
/// `conflict`, `permission_denied`, `timeout`, `host_unavailable`,
/// `model_unavailable`, `agent_failed`, `protocol`, `io`). Clients switch on
/// `code`; `message` is for humans and may be reworded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    /// Stable identifier for the failure class.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Field-level detail, when the failure is attributable to specific inputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ErrorDetail>,
}

impl ErrorBody {
    /// Build an envelope with no field-level detail.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Vec::new(),
        }
    }
}

/// One field-level entry in an [`ErrorBody`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ErrorDetail {
    /// Dotted path of the offending field, for example `preferences.wrap`.
    pub field: String,
    /// What is wrong with it.
    pub message: String,
}
