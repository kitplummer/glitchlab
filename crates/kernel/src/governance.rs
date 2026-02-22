use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// GovernanceReceipt — proof that an action was approved
// ---------------------------------------------------------------------------

/// Opaque proof that a governance check passed.
/// Cannot be constructed outside this module.
#[derive(Debug, Clone)]
pub struct GovernanceReceipt {
    /// What was approved.
    action_desc: String,
    /// When it was approved.
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl GovernanceReceipt {
    /// What action was approved.
    pub fn action_desc(&self) -> &str {
        &self.action_desc
    }

    /// When the approval was issued.
    pub fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.timestamp
    }
}

// ---------------------------------------------------------------------------
// ApprovedAction<T> — type-level governance enforcement
// ---------------------------------------------------------------------------

/// A value that has been approved by the governance layer.
///
/// You cannot construct an `ApprovedAction` without going through
/// a governance gate. This is the type-system enforcement described
/// in the language ADR — the compiler proves every governed action
/// was approved.
#[derive(Debug)]
pub struct ApprovedAction<T> {
    inner: T,
    receipt: GovernanceReceipt,
}

impl<T> ApprovedAction<T> {
    /// Only constructable within the governance module.
    pub(crate) fn new(inner: T, receipt: GovernanceReceipt) -> Self {
        Self { inner, receipt }
    }

    /// Access the approved value.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Consume and return the approved value.
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// Access the governance receipt.
    pub fn receipt(&self) -> &GovernanceReceipt {
        &self.receipt
    }
}

// ---------------------------------------------------------------------------
// BoundaryEnforcer — protected path enforcement
// ---------------------------------------------------------------------------

/// Enforces file-level boundaries. Files under protected paths
/// cannot be modified unless explicitly overridden.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundaryEnforcer {
    protected_paths: Vec<PathBuf>,
}

impl BoundaryEnforcer {
    pub fn new(protected_paths: Vec<PathBuf>) -> Self {
        Self { protected_paths }
    }

    /// Check a list of files against protected paths.
    /// Returns the list of violations (empty = clean).
    pub fn check(&self, files: &[impl AsRef<Path>]) -> Vec<PathBuf> {
        let mut violations = Vec::new();
        for file in files {
            let file = file.as_ref();
            for protected in &self.protected_paths {
                if file.starts_with(protected) {
                    violations.push(file.to_path_buf());
                    break;
                }
            }
        }
        violations
    }

    /// Check files and return an error if violations are found
    /// and `allow_core` is false.
    pub fn enforce(&self, files: &[impl AsRef<Path>], allow_core: bool) -> Result<Vec<PathBuf>> {
        let violations = self.check(files);
        if !violations.is_empty() && !allow_core {
            return Err(Error::BoundaryViolation { paths: violations });
        }
        Ok(violations)
    }
}

// ---------------------------------------------------------------------------
// ZephyrPolicy — placeholder for Zephyr governance integration
// ---------------------------------------------------------------------------

/// Governance policy for an org.
///
/// Today this holds the boundary enforcer and autonomy level.
/// When Rob ships Zephyr, this becomes the integration point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZephyrPolicy {
    /// Protected filesystem paths.
    pub boundaries: BoundaryEnforcer,

    /// Autonomy level for this org (0.0 = fully gated, 1.0 = fully autonomous).
    #[serde(default = "default_autonomy")]
    pub autonomy: f64,
}

fn default_autonomy() -> f64 {
    0.0
}

impl ZephyrPolicy {
    /// Approve an action if governance allows it.
    /// Returns `ApprovedAction<T>` on success, `Error::GovernanceDenied` on failure.
    pub fn approve<T>(&self, action: T, description: &str) -> Result<ApprovedAction<T>> {
        // For now, approval is unconditional — the boundary enforcer
        // handles file-level checks separately. When Zephyr lands,
        // this method will evaluate the full policy.
        let receipt = GovernanceReceipt {
            action_desc: description.to_string(),
            timestamp: chrono::Utc::now(),
        };
        Ok(ApprovedAction::new(action, receipt))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_enforcer_detects_violations() {
        let enforcer = BoundaryEnforcer::new(vec![
            PathBuf::from("crates/zephyr-core"),
            PathBuf::from("secrets/"),
        ]);

        let files = vec![
            "crates/zephyr-core/src/lib.rs",
            "crates/kernel/src/lib.rs",
            "secrets/api_key.txt",
        ];

        let violations = enforcer.check(&files);
        assert_eq!(violations.len(), 2);
        assert!(violations.contains(&PathBuf::from("crates/zephyr-core/src/lib.rs")));
        assert!(violations.contains(&PathBuf::from("secrets/api_key.txt")));
    }

    #[test]
    fn boundary_enforcer_clean_when_no_violations() {
        let enforcer = BoundaryEnforcer::new(vec![PathBuf::from("crates/zephyr-core")]);
        let files = vec!["crates/kernel/src/lib.rs", "src/main.rs"];
        assert!(enforcer.check(&files).is_empty());
    }

    #[test]
    fn enforce_returns_error_without_allow_core() {
        let enforcer = BoundaryEnforcer::new(vec![PathBuf::from("crates/zephyr-core")]);
        let files = vec!["crates/zephyr-core/src/lib.rs"];
        assert!(enforcer.enforce(&files, false).is_err());
    }

    #[test]
    fn enforce_returns_violations_with_allow_core() {
        let enforcer = BoundaryEnforcer::new(vec![PathBuf::from("crates/zephyr-core")]);
        let files = vec!["crates/zephyr-core/src/lib.rs"];
        let result = enforcer.enforce(&files, true).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn approved_action_requires_governance_gate() {
        let policy = ZephyrPolicy {
            boundaries: BoundaryEnforcer::new(vec![]),
            autonomy: 0.5,
        };

        let approved = policy.approve("deploy to prod", "deploy action").unwrap();
        assert_eq!(*approved.inner(), "deploy to prod");
        assert_eq!(approved.receipt().action_desc(), "deploy action");
    }

    #[test]
    fn governance_receipt_accessors() {
        let policy = ZephyrPolicy {
            boundaries: BoundaryEnforcer::new(vec![]),
            autonomy: 0.5,
        };
        let approved = policy.approve(42, "test action").unwrap();
        assert_eq!(approved.receipt().action_desc(), "test action");
        assert!(approved.receipt().timestamp() <= chrono::Utc::now());
    }

    #[test]
    fn approved_action_into_inner() {
        let policy = ZephyrPolicy {
            boundaries: BoundaryEnforcer::new(vec![]),
            autonomy: 1.0,
        };
        let approved = policy.approve(String::from("value"), "test").unwrap();
        let val = approved.into_inner();
        assert_eq!(val, "value");
    }

    #[test]
    fn zephyr_policy_default_autonomy_serde() {
        let yaml = "boundaries:\n  protected_paths: []\n";
        let policy: ZephyrPolicy = serde_yaml::from_str(yaml).unwrap();
        assert!((policy.autonomy - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn boundary_enforcer_empty_files_list() {
        let enforcer = BoundaryEnforcer::new(vec![PathBuf::from("src")]);
        let files: Vec<&str> = vec![];
        assert!(enforcer.check(&files).is_empty());
    }

    #[test]
    fn boundary_enforcer_empty_protected() {
        let enforcer = BoundaryEnforcer::new(vec![]);
        let files = vec!["anything/here.rs"];
        assert!(enforcer.check(&files).is_empty());
    }

    #[test]
    fn boundary_enforcer_serde_roundtrip() {
        let enforcer = BoundaryEnforcer::new(vec![PathBuf::from("core/src")]);
        let json = serde_json::to_string(&enforcer).unwrap();
        let parsed: BoundaryEnforcer = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.protected_paths.len(), 1);
    }
}
