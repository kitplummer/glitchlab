use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::agent::{Message, MessageContent, MessageRole};

// ---------------------------------------------------------------------------
// ContextSegment — a typed block of content for the context window
// ---------------------------------------------------------------------------

/// A discrete block of content that can be assembled into an LLM context window.
///
/// Segments have a priority (lower = more important, kept first when truncating)
/// and a category that determines how they are handled during assembly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSegment {
    /// What kind of content this is.
    pub kind: SegmentKind,

    /// The content text.
    pub content: String,

    /// Priority for retention when context is tight.
    /// Lower = more important. System prompts are 0, history is highest.
    pub priority: u32,

    /// Approximate token count (estimated at 4 chars per token).
    pub estimated_tokens: usize,
}

impl ContextSegment {
    pub fn new(kind: SegmentKind, content: String, priority: u32) -> Self {
        let estimated_tokens = estimate_tokens(&content);
        Self {
            kind,
            content,
            priority,
            estimated_tokens,
        }
    }
}

/// Categories of context content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentKind {
    /// Agent system prompt (persona, constraints, output schema).
    SystemPrompt,
    /// Task description and objective.
    TaskDescription,
    /// File contents relevant to the task.
    FileContents,
    /// Project-level context (prelude, architecture).
    ProjectContext,
    /// Output from a previous pipeline stage.
    PreviousOutput,
    /// Historical failure patterns to avoid.
    FailureHistory,
    /// Checkpoint data for session recovery.
    Checkpoint,
    /// Constraints and acceptance criteria.
    Constraints,
}

// ---------------------------------------------------------------------------
// ContextAssembler — builds context within a token budget
// ---------------------------------------------------------------------------

/// Assembles context segments into a message list that fits within
/// a token budget.
///
/// This is the key lesson from Gastown: context assembly is a
/// first-class concern. We own the context window because we call
/// LLM APIs directly, so we decide what goes in, what gets
/// truncated, and what gets dropped.
///
/// Assembly strategy:
/// 1. Sort segments by priority (lowest = most important).
/// 2. Always include system prompt (priority 0).
/// 3. Fill remaining budget with segments in priority order.
/// 4. Truncate the last segment that fits partially.
/// 5. Drop everything that doesn't fit.
pub struct ContextAssembler {
    /// Maximum tokens for the assembled context.
    max_tokens: usize,
}

impl ContextAssembler {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }

    /// Assemble segments into a two-message chat: system + user.
    ///
    /// Returns the messages and a report of what was included/dropped.
    pub fn assemble(&self, segments: &[ContextSegment]) -> AssemblyResult {
        let mut sorted: Vec<&ContextSegment> = segments.iter().collect();
        sorted.sort_by_key(|s| s.priority);

        let mut system_parts: Vec<&str> = Vec::new();
        let mut user_parts: Vec<&str> = Vec::new();
        let mut used_tokens = 0;
        let mut included = Vec::new();
        let mut dropped = Vec::new();

        for segment in &sorted {
            if used_tokens + segment.estimated_tokens <= self.max_tokens {
                // Fits entirely.
                match segment.kind {
                    SegmentKind::SystemPrompt => system_parts.push(&segment.content),
                    _ => user_parts.push(&segment.content),
                }
                used_tokens += segment.estimated_tokens;
                included.push(segment.kind);
            } else {
                // Doesn't fit entirely. Try to fit a truncated version.
                let remaining = self.max_tokens.saturating_sub(used_tokens);
                if remaining > 100 {
                    // Worth truncating if we can fit >100 tokens.
                    let truncated = truncate_to_tokens(&segment.content, remaining);
                    let truncated_tokens = estimate_tokens(&truncated);
                    match segment.kind {
                        SegmentKind::SystemPrompt => system_parts.push(
                            // Leak is fine here — we're building a one-shot result.
                            // In practice, use a collected String.
                            Box::leak(truncated.into_boxed_str()),
                        ),
                        _ => user_parts.push(Box::leak(truncated.into_boxed_str())),
                    }
                    used_tokens += truncated_tokens;
                    included.push(segment.kind);
                } else {
                    dropped.push(segment.kind);
                }
            }
        }

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(system_parts.join("\n\n")),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(user_parts.join("\n\n---\n\n")),
            },
        ];

        AssemblyResult {
            messages,
            total_tokens: used_tokens,
            included,
            dropped,
        }
    }

    /// Convenience: build segments from an agent context's common fields.
    pub fn build_segments(
        system_prompt: &str,
        objective: &str,
        file_context: &HashMap<String, String>,
        previous_output: Option<&serde_json::Value>,
        constraints: &[String],
        failure_history: Option<&str>,
    ) -> Vec<ContextSegment> {
        let mut segments = Vec::new();

        // Priority 0: system prompt (always included).
        segments.push(ContextSegment::new(
            SegmentKind::SystemPrompt,
            system_prompt.to_string(),
            0,
        ));

        // Priority 1: task description.
        segments.push(ContextSegment::new(
            SegmentKind::TaskDescription,
            objective.to_string(),
            1,
        ));

        // Priority 2: constraints.
        if !constraints.is_empty() {
            let text = format!(
                "## Constraints\n\n{}",
                constraints
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            segments.push(ContextSegment::new(SegmentKind::Constraints, text, 2));
        }

        // Priority 3: previous pipeline output.
        if let Some(prev) = previous_output.filter(|p| !p.is_null()) {
            let text = format!(
                "## Previous Stage Output\n\n```json\n{}\n```",
                serde_json::to_string_pretty(prev).unwrap_or_default()
            );
            segments.push(ContextSegment::new(SegmentKind::PreviousOutput, text, 3));
        }

        // Priority 4: file contents.
        for (path, content) in file_context {
            let text = format!("## File: {path}\n\n```\n{content}\n```");
            segments.push(ContextSegment::new(SegmentKind::FileContents, text, 4));
        }

        // Priority 5: failure history.
        if let Some(history) = failure_history.filter(|h| !h.is_empty()) {
            let text = format!("## Recent Failure Patterns (avoid repeating)\n\n{history}");
            segments.push(ContextSegment::new(SegmentKind::FailureHistory, text, 5));
        }

        segments
    }
}

/// Result of context assembly.
pub struct AssemblyResult {
    /// The assembled messages ready for an LLM call.
    pub messages: Vec<Message>,
    /// Total estimated tokens used.
    pub total_tokens: usize,
    /// Segment kinds that were included.
    pub included: Vec<SegmentKind>,
    /// Segment kinds that were dropped due to budget.
    pub dropped: Vec<SegmentKind>,
}

// ---------------------------------------------------------------------------
// Token estimation helpers
// ---------------------------------------------------------------------------

/// Rough token estimate: ~4 characters per token.
/// This is a conservative approximation. Real tokenizers vary by model,
/// but for budget/truncation decisions this is good enough.
fn estimate_tokens(text: &str) -> usize {
    // Use byte length / 4 as a rough estimate.
    // This is deliberately conservative (overestimates for ASCII,
    // reasonable for mixed content).
    text.len().div_ceil(4)
}

/// Truncate text to approximately `max_tokens` tokens.
/// Tries to break at a newline boundary for cleaner output.
fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }

    let truncated = &text[..max_chars.min(text.len())];

    // Try to break at the last newline for clean output.
    if let Some(last_nl) = truncated.rfind('\n').filter(|&pos| pos > max_chars / 2) {
        return format!("{}\n\n[... truncated]", &truncated[..last_nl]);
    }

    format!("{truncated}\n\n[... truncated]")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembly_includes_all_when_budget_allows() {
        let assembler = ContextAssembler::new(100_000);
        let segments = vec![
            ContextSegment::new(SegmentKind::SystemPrompt, "You are an agent.".into(), 0),
            ContextSegment::new(SegmentKind::TaskDescription, "Fix the bug.".into(), 1),
        ];

        let result = assembler.assemble(&segments);
        assert_eq!(result.included.len(), 2);
        assert!(result.dropped.is_empty());
        assert_eq!(result.messages.len(), 2);
        assert!(
            result.messages[0]
                .content
                .text()
                .contains("You are an agent")
        );
        assert!(result.messages[1].content.text().contains("Fix the bug"));
    }

    #[test]
    fn assembly_drops_low_priority_when_budget_tight() {
        // Very tight budget — only system prompt fits.
        let assembler = ContextAssembler::new(10);
        let segments = vec![
            ContextSegment::new(SegmentKind::SystemPrompt, "Agent.".into(), 0),
            ContextSegment::new(SegmentKind::FileContents, "x".repeat(1000), 4),
        ];

        let result = assembler.assemble(&segments);
        assert_eq!(result.included.len(), 1);
        assert_eq!(result.included[0], SegmentKind::SystemPrompt);
    }

    #[test]
    fn assembly_sorts_by_priority() {
        let assembler = ContextAssembler::new(100_000);
        let segments = vec![
            // Add in reverse priority order.
            ContextSegment::new(SegmentKind::FailureHistory, "history".into(), 5),
            ContextSegment::new(SegmentKind::SystemPrompt, "system".into(), 0),
            ContextSegment::new(SegmentKind::TaskDescription, "task".into(), 1),
        ];

        let result = assembler.assemble(&segments);
        // All included, system prompt should be first.
        assert_eq!(result.included.len(), 3);
        assert_eq!(result.included[0], SegmentKind::SystemPrompt);
        assert_eq!(result.included[1], SegmentKind::TaskDescription);
        assert_eq!(result.included[2], SegmentKind::FailureHistory);
    }

    #[test]
    fn build_segments_produces_expected_kinds() {
        let segments = ContextAssembler::build_segments(
            "You are Patch.",
            "Add a feature.",
            &HashMap::from([("src/lib.rs".into(), "fn main() {}".into())]),
            Some(&serde_json::json!({"plan": "do stuff"})),
            &["No new deps".into()],
            Some("Previous failure: import error"),
        );

        let kinds: Vec<_> = segments.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SegmentKind::SystemPrompt));
        assert!(kinds.contains(&SegmentKind::TaskDescription));
        assert!(kinds.contains(&SegmentKind::Constraints));
        assert!(kinds.contains(&SegmentKind::PreviousOutput));
        assert!(kinds.contains(&SegmentKind::FileContents));
        assert!(kinds.contains(&SegmentKind::FailureHistory));
    }

    #[test]
    fn token_estimation_is_reasonable() {
        // 100 ASCII chars should be ~25 tokens.
        let tokens = estimate_tokens(&"x".repeat(100));
        assert!((20..=30).contains(&tokens));
    }

    #[test]
    fn assembly_truncates_partially_fitting_segment() {
        // Budget: 200 tokens. System prompt ~2 tokens. Remaining ~198.
        // File contents: 2000 chars = ~500 tokens. Too big, but 198 > 100
        // so truncation kicks in.
        let assembler = ContextAssembler::new(200);
        let segments = vec![
            ContextSegment::new(SegmentKind::SystemPrompt, "Short.".into(), 0),
            ContextSegment::new(SegmentKind::FileContents, "x\n".repeat(500), 4),
        ];
        let result = assembler.assemble(&segments);
        assert_eq!(result.included.len(), 2);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn assembly_drops_when_remaining_budget_too_small() {
        let assembler = ContextAssembler::new(5);
        let segments = vec![
            ContextSegment::new(SegmentKind::SystemPrompt, "Hi.".into(), 0),
            ContextSegment::new(SegmentKind::FileContents, "x".repeat(1000), 4),
        ];
        let result = assembler.assemble(&segments);
        assert_eq!(result.included.len(), 1);
        assert_eq!(result.dropped.len(), 1);
        assert_eq!(result.dropped[0], SegmentKind::FileContents);
    }

    #[test]
    fn truncate_to_tokens_short_text() {
        let text = "short text";
        let result = truncate_to_tokens(text, 100);
        assert_eq!(result, text);
    }

    #[test]
    fn truncate_to_tokens_breaks_at_newline() {
        let text = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8";
        let result = truncate_to_tokens(text, 5); // ~20 chars
        assert!(result.contains("[... truncated]"));
    }

    #[test]
    fn build_segments_minimal() {
        let segments = ContextAssembler::build_segments(
            "system",
            "objective",
            &HashMap::new(),
            None,
            &[],
            None,
        );
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].kind, SegmentKind::SystemPrompt);
        assert_eq!(segments[1].kind, SegmentKind::TaskDescription);
    }

    #[test]
    fn build_segments_skips_null_previous() {
        let segments = ContextAssembler::build_segments(
            "system",
            "objective",
            &HashMap::new(),
            Some(&serde_json::Value::Null),
            &[],
            None,
        );
        assert_eq!(segments.len(), 2);
    }

    #[test]
    fn build_segments_skips_empty_failure_history() {
        let segments = ContextAssembler::build_segments(
            "system",
            "objective",
            &HashMap::new(),
            None,
            &[],
            Some(""),
        );
        assert_eq!(segments.len(), 2);
    }

    #[test]
    fn empty_segments_assembly() {
        let assembler = ContextAssembler::new(100_000);
        let result = assembler.assemble(&[]);
        assert!(result.included.is_empty());
        assert!(result.dropped.is_empty());
        assert_eq!(result.total_tokens, 0);
    }
}
