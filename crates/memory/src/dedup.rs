//! Deduplication and label normalization for beads.
//!
//! Pure functions — no IO, no LLM, fully unit-testable.

use crate::beads::Bead;

// ---------------------------------------------------------------------------
// Dedup verdict
// ---------------------------------------------------------------------------

/// Result of checking a candidate title against existing beads.
#[derive(Debug, Clone, PartialEq)]
pub enum DedupVerdict {
    /// No match found — safe to create.
    Unique,
    /// Exact title match (case-insensitive).
    ExactDuplicate { existing_id: String },
    /// Near-duplicate above similarity threshold.
    NearDuplicate {
        existing_id: String,
        similarity: f64,
    },
}

/// Check whether `candidate_title` duplicates any existing bead.
///
/// 1. Lowercased exact match → `ExactDuplicate`
/// 2. Trigram Jaccard similarity ≥ `threshold` → `NearDuplicate`
/// 3. Otherwise → `Unique`
///
/// Only open beads (status != "closed") are considered.
pub fn check_duplicate(
    candidate_title: &str,
    existing_beads: &[Bead],
    similarity_threshold: f64,
) -> DedupVerdict {
    let candidate_lower = candidate_title.to_lowercase();

    // Phase 1: exact match (case-insensitive)
    for bead in existing_beads {
        if bead.status == "closed" {
            continue;
        }
        if bead.title.to_lowercase() == candidate_lower {
            return DedupVerdict::ExactDuplicate {
                existing_id: bead.id.clone(),
            };
        }
    }

    // Phase 2: trigram similarity
    let mut best_sim = 0.0_f64;
    let mut best_id = String::new();

    for bead in existing_beads {
        if bead.status == "closed" {
            continue;
        }
        let sim = trigram_similarity(&candidate_lower, &bead.title.to_lowercase());
        if sim > best_sim {
            best_sim = sim;
            best_id = bead.id.clone();
        }
    }

    if best_sim >= similarity_threshold {
        DedupVerdict::NearDuplicate {
            existing_id: best_id,
            similarity: best_sim,
        }
    } else {
        DedupVerdict::Unique
    }
}

// ---------------------------------------------------------------------------
// Trigram similarity
// ---------------------------------------------------------------------------

/// Character-trigram Jaccard similarity between two strings.
///
/// Returns 0.0 for strings shorter than 3 characters (no trigrams).
/// Returns 1.0 for identical strings.
pub fn trigram_similarity(a: &str, b: &str) -> f64 {
    let tg_a = trigrams(a);
    let tg_b = trigrams(b);

    if tg_a.is_empty() && tg_b.is_empty() {
        return if a == b { 1.0 } else { 0.0 };
    }
    if tg_a.is_empty() || tg_b.is_empty() {
        return 0.0;
    }

    let intersection = tg_a.intersection(&tg_b).count();
    let union = tg_a.union(&tg_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Extract the set of character trigrams from a string.
fn trigrams(s: &str) -> std::collections::HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return std::collections::HashSet::new();
    }
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

// ---------------------------------------------------------------------------
// Label normalization
// ---------------------------------------------------------------------------

/// Normalize ADR labels and deduplicate.
///
/// - `adr:NAME` and `adr:adr-NAME.md` both become `adr:NAME.md`
/// - Exact duplicates are removed
/// - Result is sorted for determinism
pub fn normalize_labels(labels: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = labels.iter().map(|l| normalize_one_label(l)).collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

/// Normalize a single label.
///
/// For `adr:` prefixed labels:
/// - `adr:foo` → `adr:foo.md` (add .md if missing)
/// - `adr:adr-foo.md` → `adr:foo.md` (strip redundant adr- prefix)
/// - `adr:adr-foo` → `adr:foo.md`
///
/// Non-`adr:` labels pass through unchanged.
fn normalize_one_label(label: &str) -> String {
    let Some(suffix) = label.strip_prefix("adr:") else {
        return label.to_string();
    };

    // Strip redundant "adr-" prefix inside the value
    let name = suffix.strip_prefix("adr-").unwrap_or(suffix);

    // Ensure .md extension
    if name.ends_with(".md") {
        format!("adr:{name}")
    } else {
        format!("adr:{name}.md")
    }
}

/// Count open beads (status != "closed") that have a given label.
pub fn count_open_with_label(beads: &[Bead], label: &str) -> usize {
    beads
        .iter()
        .filter(|b| b.status != "closed" && b.labels.iter().any(|l| l == label))
        .count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads::Bead;

    fn make_bead(id: &str, title: &str, status: &str) -> Bead {
        Bead {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            status: status.into(),
            priority: 2,
            issue_type: "task".into(),
            dependencies: vec![],
            external_ref: None,
            labels: vec![],
            assignee: None,
            created_at: None,
        }
    }

    fn make_bead_with_labels(id: &str, status: &str, labels: Vec<String>) -> Bead {
        Bead {
            id: id.into(),
            title: format!("Bead {id}"),
            description: String::new(),
            status: status.into(),
            priority: 2,
            issue_type: "task".into(),
            dependencies: vec![],
            external_ref: None,
            labels,
            assignee: None,
            created_at: None,
        }
    }

    // -----------------------------------------------------------------------
    // trigram_similarity tests
    // -----------------------------------------------------------------------

    #[test]
    fn trigram_identical_strings() {
        assert!((trigram_similarity("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trigram_completely_different() {
        let sim = trigram_similarity("abc", "xyz");
        assert!(sim < 0.1, "expected low similarity, got {sim}");
    }

    #[test]
    fn trigram_similar_strings() {
        let sim = trigram_similarity("fix authentication bug", "fix authentication issue");
        assert!(sim > 0.5, "expected moderate similarity, got {sim}");
    }

    #[test]
    fn trigram_empty_strings() {
        assert!((trigram_similarity("", "") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trigram_one_empty() {
        assert!((trigram_similarity("hello", "") - 0.0).abs() < f64::EPSILON);
        assert!((trigram_similarity("", "hello") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trigram_short_identical_strings() {
        // Strings shorter than 3 chars produce no trigrams, but identical → 1.0
        assert!((trigram_similarity("ab", "ab") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trigram_short_different_strings() {
        // Short different strings with no trigrams → 0.0
        assert!((trigram_similarity("ab", "xy") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trigram_one_short_one_long() {
        assert!((trigram_similarity("ab", "abcdef") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trigram_symmetry() {
        let a = "implement user login";
        let b = "add user login feature";
        assert!((trigram_similarity(a, b) - trigram_similarity(b, a)).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // check_duplicate tests
    // -----------------------------------------------------------------------

    #[test]
    fn exact_duplicate_case_insensitive() {
        let beads = vec![make_bead("gl-1", "Fix the login bug", "open")];
        let verdict = check_duplicate("fix the login bug", &beads, 0.7);
        assert_eq!(
            verdict,
            DedupVerdict::ExactDuplicate {
                existing_id: "gl-1".into()
            }
        );
    }

    #[test]
    fn exact_duplicate_same_case() {
        let beads = vec![make_bead("gl-1", "Fix login", "open")];
        let verdict = check_duplicate("Fix login", &beads, 0.7);
        assert_eq!(
            verdict,
            DedupVerdict::ExactDuplicate {
                existing_id: "gl-1".into()
            }
        );
    }

    #[test]
    fn closed_beads_ignored() {
        let beads = vec![make_bead("gl-1", "Fix login", "closed")];
        let verdict = check_duplicate("Fix login", &beads, 0.7);
        assert_eq!(verdict, DedupVerdict::Unique);
    }

    #[test]
    fn near_duplicate_above_threshold() {
        let beads = vec![make_bead(
            "gl-1",
            "Add authentication to user service",
            "open",
        )];
        let verdict = check_duplicate("Add authentication to the user service", &beads, 0.7);
        match verdict {
            DedupVerdict::NearDuplicate {
                existing_id,
                similarity,
            } => {
                assert_eq!(existing_id, "gl-1");
                assert!(similarity >= 0.7, "similarity {similarity} below threshold");
            }
            other => panic!("expected NearDuplicate, got {other:?}"),
        }
    }

    #[test]
    fn unique_below_threshold() {
        let beads = vec![make_bead("gl-1", "Fix login bug", "open")];
        let verdict = check_duplicate("Implement new dashboard widget", &beads, 0.7);
        assert_eq!(verdict, DedupVerdict::Unique);
    }

    #[test]
    fn empty_existing_beads() {
        let verdict = check_duplicate("anything", &[], 0.7);
        assert_eq!(verdict, DedupVerdict::Unique);
    }

    #[test]
    fn multiple_beads_picks_best_match() {
        let beads = vec![
            make_bead("gl-1", "Fix login page", "open"),
            make_bead("gl-2", "Fix login page styling", "open"),
        ];
        let verdict = check_duplicate("Fix login page", &beads, 0.7);
        // Should match exact duplicate gl-1
        assert_eq!(
            verdict,
            DedupVerdict::ExactDuplicate {
                existing_id: "gl-1".into()
            }
        );
    }

    #[test]
    fn exact_match_preferred_over_near() {
        let beads = vec![
            make_bead("gl-1", "implement feature X", "open"),
            make_bead("gl-2", "Implement Feature X", "open"),
        ];
        // Exact match (case-insensitive) should win
        let verdict = check_duplicate("implement feature x", &beads, 0.5);
        match &verdict {
            DedupVerdict::ExactDuplicate { existing_id } => {
                assert!(existing_id == "gl-1" || existing_id == "gl-2");
            }
            other => panic!("expected ExactDuplicate, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // normalize_labels tests
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_adr_adds_md() {
        let labels = vec!["adr:config-overhaul".into()];
        assert_eq!(normalize_labels(&labels), vec!["adr:config-overhaul.md"]);
    }

    #[test]
    fn normalize_adr_strips_prefix() {
        let labels = vec!["adr:adr-config-overhaul.md".into()];
        assert_eq!(normalize_labels(&labels), vec!["adr:config-overhaul.md"]);
    }

    #[test]
    fn normalize_adr_strips_prefix_no_md() {
        let labels = vec!["adr:adr-config-overhaul".into()];
        assert_eq!(normalize_labels(&labels), vec!["adr:config-overhaul.md"]);
    }

    #[test]
    fn normalize_deduplicates_adr_variants() {
        let labels = vec![
            "adr:config-overhaul".into(),
            "adr:adr-config-overhaul.md".into(),
        ];
        // Both normalize to "adr:config-overhaul.md"
        let result = normalize_labels(&labels);
        assert_eq!(result, vec!["adr:config-overhaul.md"]);
    }

    #[test]
    fn normalize_non_adr_labels_pass_through() {
        let labels = vec!["tqm:remediation".into(), "security".into()];
        assert_eq!(
            normalize_labels(&labels),
            vec!["security".to_string(), "tqm:remediation".to_string()]
        );
    }

    #[test]
    fn normalize_mixed_labels() {
        let labels = vec![
            "adr:adr-batch-pipeline.md".into(),
            "security".into(),
            "adr:batch-pipeline".into(),
        ];
        let result = normalize_labels(&labels);
        assert_eq!(
            result,
            vec!["adr:batch-pipeline.md".to_string(), "security".to_string()]
        );
    }

    #[test]
    fn normalize_empty_labels() {
        let result = normalize_labels(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn normalize_sorts_output() {
        let labels = vec!["z-label".into(), "a-label".into(), "m-label".into()];
        let result = normalize_labels(&labels);
        assert_eq!(result, vec!["a-label", "m-label", "z-label"]);
    }

    #[test]
    fn normalize_adr_already_correct() {
        let labels = vec!["adr:config-overhaul.md".into()];
        assert_eq!(normalize_labels(&labels), vec!["adr:config-overhaul.md"]);
    }

    // -----------------------------------------------------------------------
    // count_open_with_label tests
    // -----------------------------------------------------------------------

    #[test]
    fn count_open_with_label_basic() {
        let beads = vec![
            make_bead_with_labels("gl-1", "open", vec!["tqm:remediation".into()]),
            make_bead_with_labels("gl-2", "open", vec!["tqm:remediation".into()]),
            make_bead_with_labels("gl-3", "closed", vec!["tqm:remediation".into()]),
            make_bead_with_labels("gl-4", "open", vec!["security".into()]),
        ];
        assert_eq!(count_open_with_label(&beads, "tqm:remediation"), 2);
    }

    #[test]
    fn count_open_with_label_none_matching() {
        let beads = vec![make_bead_with_labels(
            "gl-1",
            "open",
            vec!["security".into()],
        )];
        assert_eq!(count_open_with_label(&beads, "tqm:remediation"), 0);
    }

    #[test]
    fn count_open_with_label_empty() {
        assert_eq!(count_open_with_label(&[], "tqm:remediation"), 0);
    }
}
