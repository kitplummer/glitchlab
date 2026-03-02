//! Repository path resolution for the sibling-directory dev-mode workflow.
//!
//! When both GLITCHLAB and target repositories live side-by-side under a common
//! parent (e.g., `~/Code/glitchlab` and `~/Code/lowendinsight`), users can
//! pass a bare repository name and the CLI will resolve it against the parent
//! of the GLITCHLAB workspace root.
//!
//! Absolute paths are always returned unchanged.

use std::path::{Path, PathBuf};

/// Resolve a repository path argument.
///
/// - Absolute paths are returned as-is.
/// - Relative paths are resolved against the parent of the nearest ancestor
///   directory that contains a `.glitchlab/` subdirectory (the GLITCHLAB
///   workspace root).  If no workspace root is found, falls back to the parent
///   of `current_dir()`.
pub fn resolve_repo_path(input: &str) -> PathBuf {
    resolve_repo_path_from(input, std::env::current_dir().ok().as_deref())
}

/// Testable variant — accepts an explicit starting directory instead of
/// reading `current_dir()`.
pub(crate) fn resolve_repo_path_from(input: &str, start: Option<&Path>) -> PathBuf {
    let p = Path::new(input);
    if p.is_absolute() {
        return p.to_path_buf();
    }

    let base = find_workspace_parent(start)
        .or_else(|| start.and_then(|d| d.parent()).map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    base.join(input)
}

/// Walk up the directory tree from `start` looking for a `.glitchlab/`
/// directory.  Returns the *parent* of the first directory that contains one
/// (i.e., the directory where sibling repositories live), or `None` if the
/// root is reached without finding a workspace.
fn find_workspace_parent(start: Option<&Path>) -> Option<PathBuf> {
    let start = start?;
    let mut dir = start;
    loop {
        if dir.join(".glitchlab").is_dir() {
            return dir.parent().map(|p| p.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;

    // -----------------------------------------------------------------------
    // find_workspace_parent
    // -----------------------------------------------------------------------

    #[test]
    fn finds_workspace_parent_from_root() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("glitchlab");
        fs::create_dir_all(workspace.join(".glitchlab")).unwrap();

        let result = find_workspace_parent(Some(&workspace));
        assert_eq!(result, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn finds_workspace_parent_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("glitchlab");
        let subdir = workspace.join("crates").join("cli");
        fs::create_dir_all(workspace.join(".glitchlab")).unwrap();
        fs::create_dir_all(&subdir).unwrap();

        let result = find_workspace_parent(Some(&subdir));
        assert_eq!(result, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn returns_none_when_no_workspace_found() {
        let tmp = tempfile::tempdir().unwrap();
        // No .glitchlab anywhere under tmp
        let result = find_workspace_parent(Some(tmp.path()));
        assert!(result.is_none());
    }

    #[test]
    fn returns_none_for_none_start() {
        assert!(find_workspace_parent(None).is_none());
    }

    // -----------------------------------------------------------------------
    // resolve_repo_path_from
    // -----------------------------------------------------------------------

    #[test]
    fn absolute_path_returned_unchanged() {
        let result = resolve_repo_path_from("/home/user/myrepo", None);
        assert_eq!(result, PathBuf::from("/home/user/myrepo"));
    }

    #[test]
    fn absolute_path_returned_unchanged_with_start() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("glitchlab");
        fs::create_dir_all(workspace.join(".glitchlab")).unwrap();

        let result = resolve_repo_path_from("/absolute/target", Some(&workspace));
        assert_eq!(result, PathBuf::from("/absolute/target"));
    }

    #[test]
    fn relative_path_resolved_against_workspace_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("glitchlab");
        fs::create_dir_all(workspace.join(".glitchlab")).unwrap();

        let result = resolve_repo_path_from("lowendinsight", Some(&workspace));
        assert_eq!(result, tmp.path().join("lowendinsight"));
    }

    #[test]
    fn relative_path_resolved_from_deep_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("glitchlab");
        let deep = workspace.join("crates").join("cli").join("src");
        fs::create_dir_all(workspace.join(".glitchlab")).unwrap();
        fs::create_dir_all(&deep).unwrap();

        let result = resolve_repo_path_from("sibling-repo", Some(&deep));
        assert_eq!(result, tmp.path().join("sibling-repo"));
    }

    #[test]
    fn relative_path_fallback_when_no_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        // No .glitchlab; falls back to parent of tmp.path()
        let result = resolve_repo_path_from("myrepo", Some(tmp.path()));
        // parent of tmp.path() joined with "myrepo"
        let expected = tmp.path().parent().unwrap().join("myrepo");
        assert_eq!(result, expected);
    }

    #[test]
    fn relative_path_fallback_no_start() {
        // No start dir at all; base defaults to "."
        let result = resolve_repo_path_from("myrepo", None);
        assert_eq!(result, PathBuf::from(".").join("myrepo"));
    }

    #[test]
    fn bare_name_produces_sibling_path() {
        // Simulates: `glitchlab init lowendinsight` run from glitchlab root
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("code").join("glitchlab");
        fs::create_dir_all(workspace.join(".glitchlab")).unwrap();

        let result = resolve_repo_path_from("lowendinsight", Some(&workspace));
        assert_eq!(result, tmp.path().join("code").join("lowendinsight"));
    }
}
