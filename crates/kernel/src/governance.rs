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
// PermissionKind — discrete action permissions
// ---------------------------------------------------------------------------

/// The kind of action a permission grants or denies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Read files from the filesystem.
    ReadFile,
    /// Write or overwrite files on the filesystem.
    WriteFile,
    /// Execute a shell command or subprocess.
    ExecuteCommand,
    /// Make outbound network requests.
    NetworkAccess,
    /// Publish or deploy an artifact to an environment.
    DeployArtifact,
    /// Create, update, or rotate secrets and credentials.
    ModifySecret,
}

// ---------------------------------------------------------------------------
// BlastRadius — scope of potential impact
// ---------------------------------------------------------------------------

/// How broadly an action can affect the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlastRadius {
    /// Affects only local files or in-process state.
    Local,
    /// Affects the current repository.
    Repo,
    /// Affects a single deployed service.
    Service,
    /// Affects the entire engineering org.
    Org,
    /// Cross-org or production-wide impact.
    Global,
}

// ---------------------------------------------------------------------------
// CredentialPolicy — rules for credential handling
// ---------------------------------------------------------------------------

/// Policy governing how credentials may be created and used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialPolicy {
    /// Which permission kinds are allowed under this policy.
    pub allowed_kinds: Vec<PermissionKind>,
    /// Maximum time-to-live for credentials, in seconds.
    pub max_ttl_secs: u64,
    /// Whether every credential use must produce an audit entry.
    pub require_audit: bool,
}

impl Default for CredentialPolicy {
    fn default() -> Self {
        Self {
            allowed_kinds: vec![PermissionKind::ReadFile],
            max_ttl_secs: 3600,
            require_audit: true,
        }
    }
}

// ---------------------------------------------------------------------------
// AuditEntry — immutable record of a governed action
// ---------------------------------------------------------------------------

/// A single audit log entry recording an action subject to governance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique identifier for this entry.
    pub id: uuid::Uuid,
    /// Human-readable description of the action.
    pub action: String,
    /// Identity of the agent or process that performed the action.
    pub actor: String,
    /// Wall-clock time when the action was recorded.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Which permission kind was exercised.
    pub permission: PermissionKind,
    /// Scope of potential impact.
    pub blast_radius: BlastRadius,
    /// Whether the action was ultimately approved.
    pub approved: bool,
}

impl AuditEntry {
    /// Create a new audit entry stamped with the current UTC time and a fresh UUID.
    pub fn new(
        action: impl Into<String>,
        actor: impl Into<String>,
        permission: PermissionKind,
        blast_radius: BlastRadius,
        approved: bool,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            action: action.into(),
            actor: actor.into(),
            timestamp: chrono::Utc::now(),
            permission,
            blast_radius,
            approved,
        }
    }
}

impl PartialEq for AuditEntry {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for AuditEntry {}

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

    // --- PermissionKind tests ---

    #[test]
    fn permission_kind_variants_are_distinct() {
        assert_ne!(PermissionKind::ReadFile, PermissionKind::WriteFile);
        assert_ne!(
            PermissionKind::ExecuteCommand,
            PermissionKind::NetworkAccess
        );
        assert_ne!(PermissionKind::DeployArtifact, PermissionKind::ModifySecret);
    }

    #[test]
    fn permission_kind_serde_roundtrip() {
        let kinds = vec![
            PermissionKind::ReadFile,
            PermissionKind::WriteFile,
            PermissionKind::ExecuteCommand,
            PermissionKind::NetworkAccess,
            PermissionKind::DeployArtifact,
            PermissionKind::ModifySecret,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let parsed: PermissionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, kind);
        }
    }

    #[test]
    fn permission_kind_clone_and_debug() {
        let k = PermissionKind::WriteFile;
        let cloned = k.clone();
        assert_eq!(k, cloned);
        assert!(!format!("{k:?}").is_empty());
    }

    // --- BlastRadius tests ---

    #[test]
    fn blast_radius_variants_are_distinct() {
        assert_ne!(BlastRadius::Local, BlastRadius::Repo);
        assert_ne!(BlastRadius::Service, BlastRadius::Org);
        assert_ne!(BlastRadius::Org, BlastRadius::Global);
    }

    #[test]
    fn blast_radius_serde_roundtrip() {
        let radii = vec![
            BlastRadius::Local,
            BlastRadius::Repo,
            BlastRadius::Service,
            BlastRadius::Org,
            BlastRadius::Global,
        ];
        for r in &radii {
            let json = serde_json::to_string(r).unwrap();
            let parsed: BlastRadius = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, r);
        }
    }

    #[test]
    fn blast_radius_clone_and_debug() {
        let r = BlastRadius::Global;
        let cloned = r.clone();
        assert_eq!(r, cloned);
        assert!(!format!("{r:?}").is_empty());
    }

    // --- CredentialPolicy tests ---

    #[test]
    fn credential_policy_default_fields() {
        let policy = CredentialPolicy::default();
        // Default should allow at least ReadFile and require audit
        assert!(!policy.allowed_kinds.is_empty());
        assert!(policy.require_audit);
        assert!(policy.max_ttl_secs > 0);
    }

    #[test]
    fn credential_policy_serde_roundtrip() {
        let policy = CredentialPolicy {
            allowed_kinds: vec![PermissionKind::ReadFile, PermissionKind::ExecuteCommand],
            max_ttl_secs: 7200,
            require_audit: false,
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: CredentialPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn credential_policy_clone_and_debug() {
        let policy = CredentialPolicy::default();
        let cloned = policy.clone();
        assert_eq!(policy, cloned);
        assert!(!format!("{policy:?}").is_empty());
    }

    // --- AuditEntry tests ---

    #[test]
    fn audit_entry_new_populates_fields() {
        let entry = AuditEntry::new(
            "write config",
            "planner-agent",
            PermissionKind::WriteFile,
            BlastRadius::Repo,
            true,
        );
        assert_eq!(entry.action, "write config");
        assert_eq!(entry.actor, "planner-agent");
        assert_eq!(entry.permission, PermissionKind::WriteFile);
        assert_eq!(entry.blast_radius, BlastRadius::Repo);
        assert!(entry.approved);
        assert!(entry.timestamp <= chrono::Utc::now());
    }

    #[test]
    fn audit_entry_new_generates_unique_ids() {
        let e1 = AuditEntry::new(
            "a",
            "agent",
            PermissionKind::ReadFile,
            BlastRadius::Local,
            true,
        );
        let e2 = AuditEntry::new(
            "b",
            "agent",
            PermissionKind::ReadFile,
            BlastRadius::Local,
            true,
        );
        assert_ne!(e1.id, e2.id);
    }

    #[test]
    fn audit_entry_serde_roundtrip() {
        let entry = AuditEntry::new(
            "deploy",
            "release-agent",
            PermissionKind::DeployArtifact,
            BlastRadius::Service,
            false,
        );
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: AuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, entry.id);
        assert_eq!(parsed.action, entry.action);
        assert_eq!(parsed.actor, entry.actor);
        assert_eq!(parsed.permission, entry.permission);
        assert_eq!(parsed.blast_radius, entry.blast_radius);
        assert_eq!(parsed.approved, entry.approved);
    }

    #[test]
    fn audit_entry_clone_and_debug() {
        let entry = AuditEntry::new(
            "x",
            "y",
            PermissionKind::NetworkAccess,
            BlastRadius::Org,
            true,
        );
        let cloned = entry.clone();
        assert_eq!(entry.id, cloned.id);
        assert!(!format!("{entry:?}").is_empty());
    }

    #[test]
    fn audit_entry_denied_action() {
        let entry = AuditEntry::new(
            "modify secrets",
            "unknown-agent",
            PermissionKind::ModifySecret,
            BlastRadius::Global,
            false,
        );
        assert!(!entry.approved);
        assert_eq!(entry.permission, PermissionKind::ModifySecret);
        assert_eq!(entry.blast_radius, BlastRadius::Global);
    }
}
