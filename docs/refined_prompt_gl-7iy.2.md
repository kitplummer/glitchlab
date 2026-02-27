# Refined Prompt for Task gl-7iy.2: Mitigating Model Limitations

## Overview

This document demonstrates prompt refinement techniques to address common failure patterns observed in GLITCHLAB tasks, particularly budget overruns and parsing issues. The original `gl-7iy.2` task failed due to budget exceeded (96,495 / 85,120 tokens), highlighting the need for more efficient prompting strategies.

## Original Task Context

Task `gl-7iy.2` was likely a complex implementation task that exceeded token limits due to:
- Verbose context assembly
- Inefficient tool usage patterns
- Lack of explicit budget awareness in the prompt
- Insufficient guidance on minimal implementation approaches

## Refined Prompt Template

```markdown
## Task: [SPECIFIC_OBJECTIVE]

**BUDGET CONSTRAINT: Maximum 85,000 tokens. Reserve 15,000 for final verification.**

### Implementation Strategy
1. **Minimal Viable Implementation**: Focus on core functionality only
2. **Batch Tool Calls**: Use multiple tool calls per turn to minimize overhead
3. **Targeted File Access**: Only read files not already provided in context
4. **Test-First Approach**: Write failing tests, then minimal implementation

### Context Optimization
- Relevant files are pre-loaded below - DO NOT re-read them
- Use `start_line`/`end_line` parameters for large files
- Batch related operations in single responses

### Output Requirements
- Implement ONLY the specified functionality
- No feature creep or exploratory changes
- Use existing patterns from codebase
- Emit final JSON with exact schema:

```json
{
  "files_changed": ["path/to/file"],
  "tests_added": ["path/to/test"],
  "tests_passing": true,
  "commit_message": "feat: implement [specific change]",
  "summary": "Brief description"
}
```

### Error Handling
- If JSON parsing fails, include `"parse_error": true`
- Use graceful degradation, never panic
- Follow existing error patterns in codebase

## [SPECIFIC TASK DETAILS]
[Concise, focused requirements here]
```

## Refinement Principles Applied

### 1. Budget Awareness
- **Explicit token limits**: State the budget constraint upfront
- **Reserve allocation**: Explicitly reserve tokens for verification
- **Progress tracking**: Encourage batched operations to minimize overhead

### 2. Context Efficiency
- **Pre-loaded context**: Clearly indicate what files are already available
- **Targeted reading**: Discourage redundant file access
- **Focused scope**: Eliminate exploratory or tangential work

### 3. Parsing Robustness
- **Explicit schema**: Provide exact JSON structure expected
- **Error handling**: Include fallback patterns for parse failures
- **Structured output**: Use consistent formatting conventions

### 4. Implementation Guidance
- **Test-first mandate**: Enforce TDD to prevent over-implementation
- **Minimal scope**: Explicitly discourage feature creep
- **Pattern reuse**: Direct to existing codebase patterns

### 5. Tool Usage Optimization
- **Batch operations**: Encourage multiple tool calls per turn
- **Selective reading**: Use line ranges for large files
- **Verification strategy**: Plan final testing approach upfront

## Comparison: Before vs After

### Before (Problematic)
```markdown
Implement a comprehensive solution for [complex task]. 
Consider all edge cases and provide thorough error handling.
Make sure to explore the codebase and understand all related components.
```

### After (Refined)
```markdown
**BUDGET: 85K tokens max. Reserve 15K for verification.**

Implement minimal [specific functionality]:
1. Write failing test in [specific file]
2. Add [specific function] to [specific module]  
3. Verify with `cargo test [specific test]`

Files pre-loaded: [list]. DO NOT re-read.
Use batched tool calls. No feature creep.
```

## Lessons from gl-7iy.2 Failure

The original task likely failed because:

1. **Verbose context**: Excessive file reading and exploration
2. **Scope creep**: Implementation beyond minimal requirements  
3. **Inefficient tool usage**: Single tool calls per turn
4. **No budget planning**: Lack of explicit token management

The refined approach addresses each of these issues through:
- Explicit budget constraints and planning
- Pre-loaded context to prevent redundant reading
- Batched tool call requirements
- Minimal implementation mandates

## Usage Guidelines

When applying this template:

1. **Customize the budget** based on task complexity
2. **Pre-load relevant context** to prevent redundant file access
3. **Be specific** about expected outputs and file locations
4. **Include examples** of the exact JSON schema expected
5. **Emphasize batching** to maximize tool call efficiency

This refined approach should significantly reduce the likelihood of budget overruns while maintaining implementation quality and robustness.