use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// UsageRecord — accumulator for token + cost usage
// ---------------------------------------------------------------------------

/// Tracks cumulative usage across multiple LLM calls within a task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageRecord {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost: f64,
    pub call_count: u64,
}

// ---------------------------------------------------------------------------
// BudgetTracker — hard limits on token and dollar spend
// ---------------------------------------------------------------------------

/// Enforces per-task budget limits (tokens + dollars).
///
/// Checked before every LLM call. Exceeding either limit raises
/// `Error::BudgetExceeded` and halts the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetTracker {
    pub max_tokens: u64,
    pub max_dollars: f64,
    pub usage: UsageRecord,
}

impl BudgetTracker {
    pub fn new(max_tokens: u64, max_dollars: f64) -> Self {
        Self {
            max_tokens,
            max_dollars,
            usage: UsageRecord::default(),
        }
    }

    /// Record usage from an LLM call.
    pub fn record(&mut self, prompt_tokens: u64, completion_tokens: u64, cost: f64) {
        self.usage.prompt_tokens += prompt_tokens;
        self.usage.completion_tokens += completion_tokens;
        self.usage.total_tokens += prompt_tokens + completion_tokens;
        self.usage.estimated_cost += cost;
        self.usage.call_count += 1;
    }

    /// True if either token or dollar limit has been exceeded.
    pub fn exceeded(&self) -> bool {
        self.usage.total_tokens > self.max_tokens || self.usage.estimated_cost > self.max_dollars
    }

    /// Tokens remaining before hitting the limit.
    pub fn tokens_remaining(&self) -> u64 {
        self.max_tokens.saturating_sub(self.usage.total_tokens)
    }

    /// Dollars remaining before hitting the limit.
    pub fn dollars_remaining(&self) -> f64 {
        (self.max_dollars - self.usage.estimated_cost).max(0.0)
    }

    /// Fraction of the dollar budget still remaining (0.0 = exhausted, 1.0 = full).
    ///
    /// Returns `None` when `max_dollars` is zero (unlimited / unconfigured budget)
    /// to avoid a division-by-zero and signal "no budget pressure" to callers.
    pub fn remaining_cost_percentage(&self) -> Option<f64> {
        if self.max_dollars == 0.0 {
            None
        } else {
            Some((self.dollars_remaining() / self.max_dollars).clamp(0.0, 1.0))
        }
    }

    /// Check the budget and return an error if exceeded.
    /// Call this before every LLM invocation.
    pub fn check(&self) -> Result<()> {
        if self.usage.total_tokens > self.max_tokens {
            return Err(Error::BudgetExceeded {
                reason: format!(
                    "token limit exceeded: {} / {} tokens used",
                    self.usage.total_tokens, self.max_tokens,
                ),
            });
        }
        if self.usage.estimated_cost > self.max_dollars {
            return Err(Error::BudgetExceeded {
                reason: format!(
                    "dollar limit exceeded: ${:.4} / ${:.2} spent",
                    self.usage.estimated_cost, self.max_dollars,
                ),
            });
        }
        Ok(())
    }

    /// Human-readable summary of current budget state.
    ///
    /// Returns a single-line string with tokens used/remaining,
    /// dollars used/remaining, and the number of LLM calls made.
    pub fn display_budget(&self) -> String {
        let calls = self.usage.call_count;
        let call_label = if calls == 1 { "call" } else { "calls" };
        format!(
            "Tokens: {} used / {} remaining | Cost: ${:.4} used / ${:.2} remaining | {} {}",
            self.usage.total_tokens,
            self.tokens_remaining(),
            self.usage.estimated_cost,
            self.dollars_remaining(),
            calls,
            call_label,
        )
    }

    /// Summary snapshot for history/reporting.
    pub fn summary(&self) -> BudgetSummary {
        BudgetSummary {
            total_tokens: self.usage.total_tokens,
            estimated_cost: self.usage.estimated_cost,
            call_count: self.usage.call_count,
            tokens_remaining: self.tokens_remaining(),
            dollars_remaining: self.dollars_remaining(),
        }
    }
}

/// Serializable summary of budget state at a point in time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetSummary {
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub estimated_cost: f64,
    #[serde(default)]
    pub call_count: u64,
    #[serde(default)]
    pub tokens_remaining: u64,
    #[serde(default)]
    pub dollars_remaining: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_budget_not_exceeded() {
        let budget = BudgetTracker::new(150_000, 10.0);
        assert!(!budget.exceeded());
        assert!(budget.check().is_ok());
        assert_eq!(budget.tokens_remaining(), 150_000);
        assert!((budget.dollars_remaining() - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn record_accumulates() {
        let mut budget = BudgetTracker::new(150_000, 10.0);
        budget.record(1000, 500, 0.05);
        budget.record(2000, 800, 0.10);

        assert_eq!(budget.usage.prompt_tokens, 3000);
        assert_eq!(budget.usage.completion_tokens, 1300);
        assert_eq!(budget.usage.total_tokens, 4300);
        assert!((budget.usage.estimated_cost - 0.15).abs() < f64::EPSILON);
        assert_eq!(budget.usage.call_count, 2);
    }

    #[test]
    fn token_limit_exceeded() {
        let mut budget = BudgetTracker::new(1000, 10.0);
        budget.record(600, 500, 0.01);
        assert!(budget.exceeded());
        assert!(budget.check().is_err());
    }

    #[test]
    fn dollar_limit_exceeded() {
        let mut budget = BudgetTracker::new(150_000, 1.0);
        budget.record(100, 50, 1.50);
        assert!(budget.exceeded());
        let err = budget.check().unwrap_err();
        assert!(err.to_string().contains("dollar limit exceeded"));
    }

    #[test]
    fn summary_snapshot() {
        let mut budget = BudgetTracker::new(10_000, 5.0);
        budget.record(2000, 1000, 1.25);
        let summary = budget.summary();
        assert_eq!(summary.total_tokens, 3000);
        assert_eq!(summary.tokens_remaining, 7000);
        assert!((summary.dollars_remaining - 3.75).abs() < f64::EPSILON);
        assert_eq!(summary.call_count, 1);
    }

    #[test]
    fn display_budget_fresh() {
        let budget = BudgetTracker::new(10_000, 5.0);
        let s = budget.display_budget();
        assert!(s.contains("0"), "should show 0 tokens used");
        assert!(s.contains("10000"), "should show max tokens");
        assert!(s.contains("$0.0000"), "should show zero cost used");
        assert!(s.contains("$5.00"), "should show max dollars");
        assert!(s.contains("0 calls"), "should show call count");
    }

    #[test]
    fn display_budget_after_usage() {
        let mut budget = BudgetTracker::new(10_000, 5.0);
        budget.record(2000, 1000, 1.25);
        let s = budget.display_budget();
        assert!(s.contains("3000"), "should show tokens used");
        assert!(s.contains("7000"), "should show tokens remaining");
        assert!(s.contains("$1.2500"), "should show cost used");
        assert!(s.contains("$3.75"), "should show dollars remaining");
        assert!(s.contains("1 call"), "should show call count");
    }

    #[test]
    fn display_budget_multiple_calls() {
        let mut budget = BudgetTracker::new(10_000, 5.0);
        budget.record(1000, 500, 0.50);
        budget.record(500, 250, 0.25);
        let s = budget.display_budget();
        assert!(s.contains("2250"), "should show total tokens used");
        assert!(s.contains("2 calls"), "should show plural call count");
    }

    #[test]
    fn remaining_cost_percentage_full_budget() {
        let budget = BudgetTracker::new(10_000, 10.0);
        assert_eq!(budget.remaining_cost_percentage(), Some(1.0));
    }

    #[test]
    fn remaining_cost_percentage_half_spent() {
        let mut budget = BudgetTracker::new(10_000, 10.0);
        budget.record(0, 0, 5.0);
        let pct = budget.remaining_cost_percentage().unwrap();
        assert!((pct - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn remaining_cost_percentage_exhausted() {
        let mut budget = BudgetTracker::new(10_000, 10.0);
        budget.record(0, 0, 10.0);
        assert_eq!(budget.remaining_cost_percentage(), Some(0.0));
    }

    #[test]
    fn remaining_cost_percentage_zero_max_returns_none() {
        let budget = BudgetTracker::new(10_000, 0.0);
        assert_eq!(budget.remaining_cost_percentage(), None);
    }

    #[test]
    fn remaining_cost_percentage_clamps_at_zero() {
        let mut budget = BudgetTracker::new(10_000, 1.0);
        budget.record(0, 0, 2.0); // over-spend
        assert_eq!(budget.remaining_cost_percentage(), Some(0.0));
    }
}
