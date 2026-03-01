use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Stdio;

use glitchlab_kernel::error::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SKIP_DIRS: &[&str] = &[
    ".git",
    ".glitchlab",
    ".context",
    ".venv",
    "venv",
    "env",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    "coverage",
    ".cargo",
    "vendor",
    ".idea",
    ".vscode",
    "out",
    "bin",
    "obj",
];

const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "cpp", "h", "hpp", "cs", "rb",
    "swift", "kt", "toml", "yaml", "yml", "json", "md", "txt", "sql", "graphql", "proto", "sh",
    "bash", "css", "scss", "html", "svelte", "vue", "ex", "exs", "erl",
];

const KEY_FILES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "Makefile",
    "Dockerfile",
    "docker-compose.yml",
    "compose.yaml",
    "fly.toml",
    "README.md",
    "CHANGELOG.md",
    ".gitignore",
    "mix.exs",
];

// ---------------------------------------------------------------------------
// RepoIndex
// ---------------------------------------------------------------------------

/// Index of a repository's structure, used for agent context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub root: String,
    pub total_files: usize,
    pub languages: HashMap<String, usize>,
    pub files: Vec<String>,
    pub directories: Vec<String>,
    pub key_files: Vec<String>,
    pub test_files: Vec<String>,
}

impl RepoIndex {
    /// Format the index as markdown suitable for agent context injection.
    pub fn to_agent_context(&self, max_files: usize) -> String {
        let mut ctx = String::new();
        ctx.push_str("## Repository Structure\n\n");

        // Languages.
        if !self.languages.is_empty() {
            ctx.push_str("**Languages:** ");
            let mut langs: Vec<_> = self.languages.iter().collect();
            langs.sort_by(|a, b| b.1.cmp(a.1));
            let lang_strs: Vec<_> = langs
                .iter()
                .take(8)
                .map(|(ext, count)| format!("{ext} ({count})"))
                .collect();
            ctx.push_str(&lang_strs.join(", "));
            ctx.push_str("\n\n");
        }

        // Key files.
        if !self.key_files.is_empty() {
            ctx.push_str("**Key files:** ");
            ctx.push_str(&self.key_files.join(", "));
            ctx.push_str("\n\n");
        }

        // Top-level directories.
        if !self.directories.is_empty() {
            ctx.push_str("**Directories:** ");
            ctx.push_str(&self.directories.join(", "));
            ctx.push_str("\n\n");
        }

        // File listing (truncated).
        ctx.push_str(&format!(
            "**Files ({} total, showing up to {max_files}):**\n",
            self.total_files
        ));
        for file in self.files.iter().take(max_files) {
            ctx.push_str(&format!("- {file}\n"));
        }

        // Test files.
        if !self.test_files.is_empty() {
            ctx.push_str(&format!("\n**Test files ({}):**\n", self.test_files.len()));
            for tf in self.test_files.iter().take(20) {
                ctx.push_str(&format!("- {tf}\n"));
            }
        }

        ctx
    }
}

// ---------------------------------------------------------------------------
// build_index
// ---------------------------------------------------------------------------

/// Build a repo index using `git ls-files`.
pub async fn build_index(repo_path: &Path) -> Result<RepoIndex> {
    let root = repo_path.to_str().unwrap_or(".").to_string();

    // Use git ls-files for accurate, .gitignore-respecting file list.
    let output = Command::new("git")
        .args(["ls-files"])
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| Error::Workspace(format!("git ls-files failed: {e}")))?;

    let file_list = String::from_utf8_lossy(&output.stdout);

    let mut files = Vec::new();
    let mut languages: HashMap<String, usize> = HashMap::new();
    let mut key_files = Vec::new();
    let mut test_files = Vec::new();
    let mut dir_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in file_list.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }

        // Skip known junk directories.
        if SKIP_DIRS
            .iter()
            .any(|d| path.starts_with(d) || path.contains(&format!("/{d}/")))
        {
            continue;
        }

        files.push(path.to_string());

        // Top-level directory.
        if let Some(first_slash) = path.find('/') {
            dir_set.insert(path[..first_slash].to_string());
        }

        // Language detection by extension.
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str())
            && CODE_EXTENSIONS.contains(&ext)
        {
            *languages.entry(ext.to_string()).or_default() += 1;
        }

        // Key file detection.
        let filename = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if KEY_FILES.contains(&filename) {
            key_files.push(path.to_string());
        }

        // Test file detection.
        if is_test_file(path) {
            test_files.push(path.to_string());
        }
    }

    let total_files = files.len();
    let mut directories: Vec<_> = dir_set.into_iter().collect();
    directories.sort();

    Ok(RepoIndex {
        root,
        total_files,
        languages,
        files,
        directories,
        key_files,
        test_files,
    })
}

pub(crate) fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();

    // Path-based: tests/, test/, __tests__/, spec/
    if lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
        || lower.contains("/spec/")
    {
        return true;
    }

    // Filename-based.
    let filename = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let lower_name = filename.to_lowercase();

    lower_name.starts_with("test_")
        || lower_name.ends_with("_test.rs")
        || lower_name.ends_with("_test.go")
        || lower_name.ends_with("_test.py")
        || lower_name.ends_with(".test.ts")
        || lower_name.ends_with(".test.tsx")
        || lower_name.ends_with(".test.js")
        || lower_name.ends_with(".spec.ts")
        || lower_name.ends_with(".spec.js")
}

// ---------------------------------------------------------------------------
// Codebase Knowledge
// ---------------------------------------------------------------------------

/// Maximum bytes for the codebase knowledge segment.
const KNOWLEDGE_MAX_BYTES: usize = 6 * 1024;

/// Build a structured codebase knowledge summary from the repo index.
///
/// This produces a persistent, compact overview of the project that agents
/// can consume without re-exploring the codebase. Think of it as a
/// CLAUDE.md for automated agents — it answers "what is this project and
/// where are things?" so the implementer doesn't burn tool turns on
/// `list_files` and `read_file` calls to orient itself.
pub fn build_codebase_knowledge(index: &RepoIndex, repo_path: &Path) -> String {
    let mut kb = String::with_capacity(KNOWLEDGE_MAX_BYTES);

    kb.push_str("## Codebase Knowledge\n\n");
    kb.push_str("Use this context to orient yourself. Do NOT re-explore files described here.\n\n");

    // --- Project type and layout ---
    let is_rust = index.key_files.iter().any(|f| f.ends_with("Cargo.toml"));
    let is_node = index.key_files.iter().any(|f| f.ends_with("package.json"));
    let is_python = index
        .key_files
        .iter()
        .any(|f| f.ends_with("pyproject.toml"));
    let is_go = index.key_files.iter().any(|f| f.ends_with("go.mod"));

    if is_rust {
        kb.push_str("**Project type:** Rust workspace\n");
        append_rust_workspace_info(&mut kb, index, repo_path);
    } else if is_node {
        kb.push_str("**Project type:** Node.js\n");
    } else if is_python {
        kb.push_str("**Project type:** Python\n");
    } else if is_go {
        kb.push_str("**Project type:** Go\n");
    }

    // --- Directory layout ---
    if !index.directories.is_empty() {
        kb.push_str(&format!(
            "**Top-level dirs:** {}\n",
            index.directories.join(", ")
        ));
    }

    // --- File statistics ---
    if !index.languages.is_empty() {
        let mut langs: Vec<_> = index.languages.iter().collect();
        langs.sort_by(|a, b| b.1.cmp(a.1));
        let lang_summary: Vec<_> = langs
            .iter()
            .take(5)
            .map(|(ext, count)| format!("{ext}({count})"))
            .collect();
        kb.push_str(&format!(
            "**Files:** {} total — {}\n",
            index.total_files,
            lang_summary.join(", ")
        ));
    }

    // --- Test commands ---
    if is_rust {
        kb.push_str("**Test command:** `cargo test --workspace`\n");
        kb.push_str("**Lint command:** `cargo clippy --all-targets -- -D warnings`\n");
        kb.push_str("**Format command:** `cargo fmt --check`\n");
    } else if is_node {
        kb.push_str("**Test command:** `npm test`\n");
    } else if is_python {
        kb.push_str("**Test command:** `python -m pytest`\n");
    } else if is_go {
        kb.push_str("**Test command:** `go test ./...`\n");
    }

    kb.push('\n');

    // --- Key source files (grouped by directory) ---
    let source_files: Vec<&str> = index
        .files
        .iter()
        .filter(|f| {
            let ext = Path::new(f)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            matches!(ext, "rs" | "py" | "ts" | "tsx" | "js" | "go" | "java")
        })
        .map(String::as_str)
        .collect();

    if !source_files.is_empty() {
        kb.push_str("### Source files\n\n");
        // Group by first two path components for readability.
        let mut groups: std::collections::BTreeMap<String, Vec<&str>> =
            std::collections::BTreeMap::new();
        for file in &source_files {
            let parts: Vec<&str> = file.split('/').collect();
            let group = if parts.len() >= 2 {
                format!("{}/{}", parts[0], parts[1])
            } else {
                parts[0].to_string()
            };
            groups.entry(group).or_default().push(file);
        }

        for (group, files) in &groups {
            if kb.len() > KNOWLEDGE_MAX_BYTES - 200 {
                kb.push_str("\n(truncated)\n");
                break;
            }
            kb.push_str(&format!("**{group}/**: "));
            let names: Vec<&str> = files
                .iter()
                .filter_map(|f| Path::new(f).file_name().and_then(|n| n.to_str()))
                .collect();
            kb.push_str(&names.join(", "));
            kb.push('\n');
        }
    }

    // Enforce size limit.
    if kb.len() > KNOWLEDGE_MAX_BYTES {
        kb.truncate(KNOWLEDGE_MAX_BYTES - 20);
        kb.push_str("\n(truncated)\n");
    }

    kb
}

/// Append Rust workspace details — crate names, purposes, and key modules.
fn append_rust_workspace_info(kb: &mut String, index: &RepoIndex, repo_path: &Path) {
    // Find all Cargo.toml files to identify workspace members.
    let cargo_tomls: Vec<&str> = index
        .key_files
        .iter()
        .filter(|f| f.ends_with("Cargo.toml"))
        .map(String::as_str)
        .collect();

    if cargo_tomls.len() <= 1 {
        return;
    }

    kb.push_str("**Workspace crates:**\n");
    for toml_path in &cargo_tomls {
        // Skip root Cargo.toml.
        if *toml_path == "Cargo.toml" {
            continue;
        }

        let crate_dir = Path::new(toml_path).parent().unwrap_or(Path::new(""));
        let crate_name = crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Try to read the first line of lib.rs or main.rs for a doc comment.
        let purpose = read_crate_purpose(repo_path, crate_dir);
        if let Some(desc) = purpose {
            kb.push_str(&format!(
                "- **{crate_name}** ({crate_dir}) — {desc}\n",
                crate_dir = crate_dir.display()
            ));
        } else {
            kb.push_str(&format!("- **{crate_name}** ({})\n", crate_dir.display()));
        }

        // List source files in this crate's src/.
        let src_prefix = format!("{}/src/", crate_dir.display());
        let crate_files: Vec<&str> = index
            .files
            .iter()
            .filter(|f| f.starts_with(&src_prefix) && f.ends_with(".rs"))
            .filter_map(|f| f.strip_prefix(&src_prefix))
            .collect();

        if !crate_files.is_empty() && crate_files.len() <= 20 {
            kb.push_str(&format!("  modules: {}\n", crate_files.join(", ")));
        }
    }
    kb.push('\n');
}

/// Read the first doc comment from a crate's lib.rs or main.rs.
fn read_crate_purpose(repo_path: &Path, crate_dir: &Path) -> Option<String> {
    for entry in &["src/lib.rs", "src/main.rs"] {
        let path = repo_path.join(crate_dir).join(entry);
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines().take(10) {
                let trimmed = line.trim();
                if let Some(doc) = trimmed.strip_prefix("//!") {
                    let doc = doc.trim();
                    if !doc.is_empty() {
                        return Some(doc.to_string());
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// ADR Indexer
// ---------------------------------------------------------------------------

/// Maximum total size for ADR context injected into agents.
const ADR_CONTEXT_MAX_BYTES: usize = 8 * 1024;

/// Maximum bytes of the Decision section excerpt per ADR.
const ADR_DECISION_EXCERPT_MAX: usize = 500;

/// Structured summary of an Architecture Decision Record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdrSummary {
    pub filename: String,
    pub title: String,
    pub status: String,
    pub decision_excerpt: String,
}

/// Scan `docs/adr-*.md` files in a repo and extract structured summaries.
pub async fn build_adr_index(repo_path: &Path) -> Result<Vec<AdrSummary>> {
    let docs_dir = repo_path.join("docs");
    if !docs_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();

    let entries = std::fs::read_dir(&docs_dir)
        .map_err(|e| Error::Workspace(format!("failed to read docs/: {e}")))?;

    let mut filenames: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("adr-") && name.ends_with(".md") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    filenames.sort();

    for filename in filenames {
        let path = docs_dir.join(&filename);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let title = extract_adr_title(&content).unwrap_or_default();
        let status = extract_adr_status(&content).unwrap_or_default();
        let decision_excerpt = extract_adr_decision(&content).unwrap_or_default();

        summaries.push(AdrSummary {
            filename,
            title,
            status,
            decision_excerpt,
        });
    }

    Ok(summaries)
}

/// Format ADR summaries as markdown context for agent injection.
pub fn adr_summaries_to_context(summaries: &[AdrSummary]) -> String {
    if summaries.is_empty() {
        return String::new();
    }

    let mut ctx = String::with_capacity(ADR_CONTEXT_MAX_BYTES);
    ctx.push_str("## Architecture Decision Records\n\n");

    for s in summaries {
        if ctx.len() > ADR_CONTEXT_MAX_BYTES - 200 {
            ctx.push_str("\n(ADR context truncated)\n");
            break;
        }
        ctx.push_str(&format!("### {} — {}\n", s.filename, s.title));
        if !s.status.is_empty() {
            ctx.push_str(&format!("**Status:** {}\n", s.status));
        }
        if !s.decision_excerpt.is_empty() {
            ctx.push_str(&format!("{}\n", s.decision_excerpt));
        }
        ctx.push('\n');
    }

    if ctx.len() > ADR_CONTEXT_MAX_BYTES {
        let mut end = ADR_CONTEXT_MAX_BYTES - 20;
        while !ctx.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        ctx.truncate(end);
        ctx.push_str("\n(truncated)\n");
    }

    ctx
}

/// Extract the title from a `# ADR: <title>` or `# <title>` line.
fn extract_adr_title(content: &str) -> Option<String> {
    for line in content.lines().take(10) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ADR:") {
            return Some(rest.trim().to_string());
        }
        if let Some(rest) = trimmed.strip_prefix("# ")
            && !rest.is_empty()
        {
            return Some(rest.to_string());
        }
    }
    None
}

/// Extract the status from a `**Status:** <value>` line.
fn extract_adr_status(content: &str) -> Option<String> {
    for line in content.lines().take(20) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("**Status:**") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Extract the first ~500 chars after a `## Decision` heading.
fn extract_adr_decision(content: &str) -> Option<String> {
    let mut in_decision = false;
    let mut excerpt = String::new();

    for line in content.lines() {
        if in_decision {
            // Stop at the next heading.
            if line.starts_with("## ") {
                break;
            }
            if excerpt.len() < ADR_DECISION_EXCERPT_MAX
                && (!excerpt.is_empty() || !line.trim().is_empty())
            {
                excerpt.push_str(line);
                excerpt.push('\n');
            }
        } else if line.trim().starts_with("## Decision") {
            in_decision = true;
        }
    }

    let trimmed = excerpt.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// ---------------------------------------------------------------------------
// ADR Fingerprinting — change detection for watch loop
// ---------------------------------------------------------------------------

/// Content fingerprint for an ADR file. Used by the orchestrator to detect
/// new or changed ADRs between scans without re-reading full file contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdrFingerprint {
    pub filename: String,
    pub content_hash: u64,
}

/// Compute content fingerprints for all `docs/adr-*.md` files in the repo.
///
/// Uses `DefaultHasher` (SipHash) for stable, collision-resistant hashing.
/// Returns an empty vec if the `docs/` directory doesn't exist.
pub fn compute_adr_fingerprints(repo_path: &Path) -> Result<Vec<AdrFingerprint>> {
    let docs_dir = repo_path.join("docs");
    if !docs_dir.is_dir() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&docs_dir)
        .map_err(|e| Error::Workspace(format!("failed to read docs/: {e}")))?;

    let mut fingerprints = Vec::new();

    let mut filenames: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("adr-") && name.ends_with(".md") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    filenames.sort();

    for filename in filenames {
        let path = docs_dir.join(&filename);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let content_hash = hasher.finish();

        fingerprints.push(AdrFingerprint {
            filename,
            content_hash,
        });
    }

    Ok(fingerprints)
}

/// Diff two sets of ADR fingerprints, returning `(new_filenames, changed_filenames)`.
///
/// - **New:** present in `current` but not in `previous`.
/// - **Changed:** present in both but with different content hashes.
/// - Files present in `previous` but missing from `current` are silently ignored
///   (ADR was deleted — nothing to decompose).
pub fn diff_adr_fingerprints(
    previous: &[AdrFingerprint],
    current: &[AdrFingerprint],
) -> (Vec<String>, Vec<String>) {
    let prev_map: HashMap<&str, u64> = previous
        .iter()
        .map(|fp| (fp.filename.as_str(), fp.content_hash))
        .collect();

    let mut new_files = Vec::new();
    let mut changed_files = Vec::new();

    for fp in current {
        match prev_map.get(fp.filename.as_str()) {
            None => new_files.push(fp.filename.clone()),
            Some(&old_hash) if old_hash != fp.content_hash => {
                changed_files.push(fp.filename.clone());
            }
            Some(_) => {} // unchanged
        }
    }

    (new_files, changed_files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_path_based() {
        assert!(is_test_file("src/tests/foo.rs"));
        assert!(is_test_file("src/test/bar.py"));
        assert!(is_test_file("src/__tests__/baz.ts"));
        assert!(is_test_file("lib/spec/model_spec.rb"));
    }

    #[test]
    fn test_file_name_based() {
        assert!(is_test_file("test_something.py"));
        assert!(is_test_file("something_test.rs"));
        assert!(is_test_file("something_test.go"));
        assert!(is_test_file("something_test.py"));
        assert!(is_test_file("something.test.ts"));
        assert!(is_test_file("something.test.tsx"));
        assert!(is_test_file("something.test.js"));
        assert!(is_test_file("something.spec.ts"));
        assert!(is_test_file("something.spec.js"));
    }

    #[test]
    fn not_test_file() {
        assert!(!is_test_file("src/main.rs"));
        assert!(!is_test_file("lib/utils.py"));
        assert!(!is_test_file("index.ts"));
    }

    #[test]
    fn to_agent_context_full() {
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 3,
            languages: HashMap::from([("rs".into(), 2), ("toml".into(), 1)]),
            files: vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "Cargo.toml".into(),
            ],
            directories: vec!["src".into()],
            key_files: vec!["Cargo.toml".into()],
            test_files: vec!["src/tests/mod.rs".into()],
        };

        let ctx = index.to_agent_context(10);
        assert!(ctx.contains("Repository Structure"));
        assert!(ctx.contains("Languages"));
        assert!(ctx.contains("rs"));
        assert!(ctx.contains("Key files"));
        assert!(ctx.contains("Cargo.toml"));
        assert!(ctx.contains("Directories"));
        assert!(ctx.contains("src"));
        assert!(ctx.contains("Files (3 total"));
        assert!(ctx.contains("Test files"));
    }

    #[test]
    fn to_agent_context_empty() {
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 0,
            languages: HashMap::new(),
            files: vec![],
            directories: vec![],
            key_files: vec![],
            test_files: vec![],
        };

        let ctx = index.to_agent_context(10);
        assert!(ctx.contains("Repository Structure"));
        assert!(ctx.contains("Files (0 total"));
        assert!(!ctx.contains("Languages"));
        assert!(!ctx.contains("Key files"));
    }

    #[test]
    fn to_agent_context_truncates_file_list() {
        let files: Vec<String> = (0..50).map(|i| format!("src/file_{i}.rs")).collect();
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 50,
            languages: HashMap::from([("rs".into(), 50)]),
            files,
            directories: vec!["src".into()],
            key_files: vec![],
            test_files: vec![],
        };

        let ctx = index.to_agent_context(5);
        assert!(ctx.contains("file_0"));
        assert!(ctx.contains("file_4"));
        assert!(!ctx.contains("file_5"));
    }

    #[tokio::test]
    async fn build_index_on_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn hello() {}").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        std::fs::write(dir.path().join("src/test_utils.py"), "").unwrap();

        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let index = build_index(dir.path()).await.unwrap();
        assert!(index.total_files >= 3);
        assert!(index.languages.contains_key("rs"));
        assert!(index.key_files.contains(&"Cargo.toml".to_string()));
        assert!(index.directories.contains(&"src".to_string()));
        assert!(!index.test_files.is_empty());
    }

    #[tokio::test]
    async fn build_index_skips_junk_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        // Create a file in a skip dir (node_modules) and a normal file.
        std::fs::create_dir_all(dir.path().join("node_modules/foo")).unwrap();
        std::fs::write(dir.path().join("node_modules/foo/index.js"), "").unwrap();
        std::fs::write(dir.path().join("index.js"), "console.log('hi')").unwrap();

        // Force-add to bypass .gitignore.
        std::process::Command::new("git")
            .args(["add", "-f", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let index = build_index(dir.path()).await.unwrap();
        // node_modules files should be filtered out.
        for file in &index.files {
            assert!(
                !file.contains("node_modules"),
                "should skip node_modules: {file}"
            );
        }
    }

    #[test]
    fn key_files_includes_compose_yaml() {
        assert!(KEY_FILES.contains(&"compose.yaml"));
    }

    #[tokio::test]
    async fn test_build_index_with_compose_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        // Create a compose.yaml file
        std::fs::write(
            dir.path().join("compose.yaml"),
            "version: '3'\nservices:\n  web:\n    image: nginx",
        )
        .unwrap();

        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let index = build_index(dir.path()).await.unwrap();
        assert!(index.key_files.contains(&"compose.yaml".to_string()));
    }

    #[test]
    fn codebase_knowledge_rust_workspace() {
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 8,
            languages: HashMap::from([("rs".into(), 6), ("toml".into(), 2)]),
            files: vec![
                "crates/kernel/src/lib.rs".into(),
                "crates/kernel/src/agent.rs".into(),
                "crates/router/src/lib.rs".into(),
                "crates/router/src/router.rs".into(),
                "crates/cli/src/main.rs".into(),
                "Cargo.toml".into(),
                "crates/kernel/Cargo.toml".into(),
                "crates/router/Cargo.toml".into(),
            ],
            directories: vec!["crates".into()],
            key_files: vec![
                "Cargo.toml".into(),
                "crates/kernel/Cargo.toml".into(),
                "crates/router/Cargo.toml".into(),
            ],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, Path::new("/nonexistent"));
        assert!(kb.contains("Codebase Knowledge"));
        assert!(kb.contains("Rust workspace"));
        assert!(kb.contains("Workspace crates"));
        assert!(kb.contains("kernel"));
        assert!(kb.contains("router"));
        assert!(kb.contains("cargo test"));
        assert!(kb.contains("Source files"));
    }

    #[test]
    fn codebase_knowledge_non_rust() {
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 3,
            languages: HashMap::from([("py".into(), 2), ("toml".into(), 1)]),
            files: vec![
                "src/main.py".into(),
                "src/utils.py".into(),
                "pyproject.toml".into(),
            ],
            directories: vec!["src".into()],
            key_files: vec!["pyproject.toml".into()],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, Path::new("/nonexistent"));
        assert!(kb.contains("Python"));
        assert!(kb.contains("pytest"));
        assert!(!kb.contains("Rust"));
    }

    #[test]
    fn codebase_knowledge_node_project() {
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 3,
            languages: HashMap::from([("js".into(), 2), ("json".into(), 1)]),
            files: vec![
                "src/index.js".into(),
                "src/utils.js".into(),
                "package.json".into(),
            ],
            directories: vec!["src".into()],
            key_files: vec!["package.json".into()],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, Path::new("/nonexistent"));
        assert!(kb.contains("Node.js"));
        assert!(kb.contains("npm test"));
        assert!(!kb.contains("Rust"));
    }

    #[test]
    fn codebase_knowledge_go_project() {
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 3,
            languages: HashMap::from([("go".into(), 2)]),
            files: vec!["cmd/main.go".into(), "pkg/handler.go".into()],
            directories: vec!["cmd".into(), "pkg".into()],
            key_files: vec!["go.mod".into()],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, Path::new("/nonexistent"));
        assert!(kb.contains("Go"));
        assert!(kb.contains("go test"));
        assert!(!kb.contains("Rust"));
    }

    #[test]
    fn codebase_knowledge_with_crate_purpose() {
        // Create a temp dir with a real lib.rs containing a doc comment.
        let dir = tempfile::tempdir().unwrap();
        let crate_dir = dir.path().join("crates/mylib/src");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("lib.rs"),
            "//! My awesome library for doing things.\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(crate_dir.join("utils.rs"), "fn helper() {}\n").unwrap();

        // Create Cargo.toml files.
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            dir.path().join("crates/mylib/Cargo.toml"),
            "[package]\nname = \"mylib\"\n",
        )
        .unwrap();

        let index = RepoIndex {
            root: dir.path().to_string_lossy().into(),
            total_files: 4,
            languages: HashMap::from([("rs".into(), 2), ("toml".into(), 2)]),
            files: vec![
                "crates/mylib/src/lib.rs".into(),
                "crates/mylib/src/utils.rs".into(),
            ],
            directories: vec!["crates".into()],
            key_files: vec!["Cargo.toml".into(), "crates/mylib/Cargo.toml".into()],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, dir.path());
        assert!(kb.contains("Rust workspace"));
        assert!(kb.contains("My awesome library for doing things"));
        assert!(kb.contains("modules:"));
    }

    #[test]
    fn codebase_knowledge_single_component_path() {
        // Test files at root level (single path component).
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 2,
            languages: HashMap::from([("py".into(), 2)]),
            files: vec!["main.py".into(), "utils.py".into()],
            directories: vec![],
            key_files: vec!["pyproject.toml".into()],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, Path::new("/nonexistent"));
        assert!(kb.contains("Source files"));
        assert!(kb.contains("main.py"));
    }

    #[test]
    fn codebase_knowledge_respects_size_limit() {
        // Create a large index that would exceed the 6KB limit.
        let files: Vec<String> = (0..500)
            .map(|i| format!("src/module_{i}/component_{i}.rs"))
            .collect();
        let index = RepoIndex {
            root: "/tmp/repo".into(),
            total_files: 500,
            languages: HashMap::from([("rs".into(), 500)]),
            files,
            directories: vec!["src".into()],
            key_files: vec!["Cargo.toml".into()],
            test_files: vec![],
        };

        let kb = build_codebase_knowledge(&index, Path::new("/nonexistent"));
        assert!(kb.len() <= KNOWLEDGE_MAX_BYTES);
        assert!(kb.contains("truncated"));
    }

    #[test]
    fn repo_index_serde_roundtrip() {
        let index = RepoIndex {
            root: "/tmp".into(),
            total_files: 1,
            languages: HashMap::from([("rs".into(), 1)]),
            files: vec!["src/lib.rs".into()],
            directories: vec!["src".into()],
            key_files: vec![],
            test_files: vec![],
        };
        let json = serde_json::to_string(&index).unwrap();
        let parsed: RepoIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_files, 1);
    }

    // --- ADR indexer tests ---

    #[tokio::test]
    async fn build_adr_index_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("adr-001.md"),
            "# ADR: Use Rust\n\n**Status:** Accepted\n\n## Decision\n\nWe will use Rust for all backend services.\n\n## Consequences\n\nFast.\n",
        )
        .unwrap();
        std::fs::write(
            docs.join("adr-002.md"),
            "# ADR: Provider Agnostic\n\n**Status:** Proposed\n\n## Decision\n\nAll LLM calls go through the Router.\n",
        )
        .unwrap();
        // Non-ADR file should be ignored.
        std::fs::write(docs.join("readme.md"), "# Docs\n").unwrap();

        let summaries = build_adr_index(dir.path()).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].filename, "adr-001.md");
        assert_eq!(summaries[0].title, "Use Rust");
        assert_eq!(summaries[0].status, "Accepted");
        assert!(summaries[0].decision_excerpt.contains("Rust"));
        assert_eq!(summaries[1].filename, "adr-002.md");
        assert_eq!(summaries[1].title, "Provider Agnostic");
    }

    #[test]
    fn adr_summaries_to_context_formats_markdown() {
        let summaries = vec![
            AdrSummary {
                filename: "adr-001.md".into(),
                title: "Use Rust".into(),
                status: "Accepted".into(),
                decision_excerpt: "We will use Rust.".into(),
            },
            AdrSummary {
                filename: "adr-002.md".into(),
                title: "Router".into(),
                status: "Proposed".into(),
                decision_excerpt: "All calls go through Router.".into(),
            },
        ];
        let ctx = adr_summaries_to_context(&summaries);
        assert!(ctx.contains("Architecture Decision Records"));
        assert!(ctx.contains("adr-001.md"));
        assert!(ctx.contains("Use Rust"));
        assert!(ctx.contains("Accepted"));
        assert!(ctx.contains("We will use Rust."));
        assert!(ctx.contains("adr-002.md"));
    }

    #[tokio::test]
    async fn build_adr_index_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        // No docs/ directory at all.
        let summaries = build_adr_index(dir.path()).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn adr_summaries_to_context_empty() {
        let ctx = adr_summaries_to_context(&[]);
        assert!(ctx.is_empty());
    }

    #[test]
    fn extract_adr_title_hash_prefix() {
        let content = "# ADR: My Decision\n\nSome text.\n";
        assert_eq!(extract_adr_title(content), Some("My Decision".into()));
    }

    #[test]
    fn extract_adr_title_plain_heading() {
        let content = "# My Plain Title\n\nText.\n";
        assert_eq!(extract_adr_title(content), Some("My Plain Title".into()));
    }

    #[test]
    fn extract_adr_status_found() {
        let content = "# ADR: Foo\n\n**Status:** Accepted\n\n## Decision\n";
        assert_eq!(extract_adr_status(content), Some("Accepted".into()));
    }

    #[test]
    fn extract_adr_decision_excerpt() {
        let content = "# ADR: Foo\n\n## Decision\n\nWe will do X.\nAnd also Y.\n\n## Consequences\n\nThings.\n";
        let excerpt = extract_adr_decision(content).unwrap();
        assert!(excerpt.contains("We will do X."));
        assert!(excerpt.contains("And also Y."));
        assert!(!excerpt.contains("Things."));
    }

    #[test]
    fn extract_adr_decision_truncates_long_excerpt() {
        // Build multi-line content that exceeds the limit.
        let lines: Vec<String> = (0..20)
            .map(|i| format!("Line {i}: {}", "x".repeat(40)))
            .collect();
        let content = format!("## Decision\n\n{}\n\n## Consequences\n", lines.join("\n"));
        let excerpt = extract_adr_decision(&content).unwrap();
        // The function stops adding lines once the limit is reached.
        assert!(excerpt.len() <= ADR_DECISION_EXCERPT_MAX + 50);
        assert!(excerpt.len() < content.len());
    }

    // --- ADR fingerprinting tests ---

    #[test]
    fn compute_fingerprints_hashes_content() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("adr-001.md"), "# ADR: Use Rust\n").unwrap();
        std::fs::write(docs.join("adr-002.md"), "# ADR: Router\n").unwrap();
        // Non-ADR file should be ignored.
        std::fs::write(docs.join("readme.md"), "# Docs\n").unwrap();

        let fps = compute_adr_fingerprints(dir.path()).unwrap();
        assert_eq!(fps.len(), 2);
        assert_eq!(fps[0].filename, "adr-001.md");
        assert_eq!(fps[1].filename, "adr-002.md");
        assert_ne!(fps[0].content_hash, 0);
        // Different content should produce different hashes.
        assert_ne!(fps[0].content_hash, fps[1].content_hash);
    }

    #[test]
    fn compute_fingerprints_empty_when_no_docs() {
        let dir = tempfile::tempdir().unwrap();
        let fps = compute_adr_fingerprints(dir.path()).unwrap();
        assert!(fps.is_empty());
    }

    #[test]
    fn diff_detects_new() {
        let prev = vec![AdrFingerprint {
            filename: "adr-001.md".into(),
            content_hash: 111,
        }];
        let curr = vec![
            AdrFingerprint {
                filename: "adr-001.md".into(),
                content_hash: 111,
            },
            AdrFingerprint {
                filename: "adr-002.md".into(),
                content_hash: 222,
            },
        ];
        let (new, changed) = diff_adr_fingerprints(&prev, &curr);
        assert_eq!(new, vec!["adr-002.md"]);
        assert!(changed.is_empty());
    }

    #[test]
    fn diff_detects_changed() {
        let prev = vec![AdrFingerprint {
            filename: "adr-001.md".into(),
            content_hash: 111,
        }];
        let curr = vec![AdrFingerprint {
            filename: "adr-001.md".into(),
            content_hash: 999,
        }];
        let (new, changed) = diff_adr_fingerprints(&prev, &curr);
        assert!(new.is_empty());
        assert_eq!(changed, vec!["adr-001.md"]);
    }

    #[test]
    fn diff_no_changes() {
        let fps = vec![
            AdrFingerprint {
                filename: "adr-001.md".into(),
                content_hash: 111,
            },
            AdrFingerprint {
                filename: "adr-002.md".into(),
                content_hash: 222,
            },
        ];
        let (new, changed) = diff_adr_fingerprints(&fps, &fps);
        assert!(new.is_empty());
        assert!(changed.is_empty());
    }

    #[test]
    fn diff_handles_removed() {
        let prev = vec![
            AdrFingerprint {
                filename: "adr-001.md".into(),
                content_hash: 111,
            },
            AdrFingerprint {
                filename: "adr-002.md".into(),
                content_hash: 222,
            },
        ];
        let curr = vec![AdrFingerprint {
            filename: "adr-001.md".into(),
            content_hash: 111,
        }];
        // Removed files are silently ignored.
        let (new, changed) = diff_adr_fingerprints(&prev, &curr);
        assert!(new.is_empty());
        assert!(changed.is_empty());
    }

    #[test]
    fn fingerprint_serde_roundtrip() {
        let fp = AdrFingerprint {
            filename: "adr-test.md".into(),
            content_hash: 12345,
        };
        let json = serde_json::to_string(&fp).unwrap();
        let parsed: AdrFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, fp);
    }
}
