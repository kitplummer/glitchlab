use std::collections::HashMap;
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
    let root = repo_path
        .to_str()
        .unwrap_or(".")
        .to_string();

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
        if SKIP_DIRS.iter().any(|d| path.starts_with(d) || path.contains(&format!("/{d}/"))) {
            continue;
        }

        files.push(path.to_string());

        // Top-level directory.
        if let Some(first_slash) = path.find('/') {
            dir_set.insert(path[..first_slash].to_string());
        }

        // Language detection by extension.
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
            if CODE_EXTENSIONS.contains(&ext) {
                *languages.entry(ext.to_string()).or_default() += 1;
            }
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
            files: vec!["src/main.rs".into(), "src/lib.rs".into(), "Cargo.toml".into()],
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
            assert!(!file.contains("node_modules"), "should skip node_modules: {file}");
        }
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
}
