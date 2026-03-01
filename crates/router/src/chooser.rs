use std::collections::{HashMap, HashSet};
use std::fmt;

// ---------------------------------------------------------------------------
// ModelChooser — rule-based cost-aware model routing (Tier 1)
// ---------------------------------------------------------------------------

/// Model quality/cost tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModelTier {
    Economy,      // Flash Lite, Haiku, local
    Standard,     // Flash, GPT-4o-mini
    StandardPlus, // Pro, Sonnet — better structured output than Standard
    Premium,      // Opus, GPT-4o
}

impl ModelTier {
    /// Parse a tier from a string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "economy" => Some(Self::Economy),
            "standard" => Some(Self::Standard),
            "standard_plus" => Some(Self::StandardPlus),
            "premium" => Some(Self::Premium),
            _ => None,
        }
    }

    /// Downgrade a tier by one level. Economy stays Economy.
    fn downgrade(self) -> Self {
        match self {
            Self::Premium => Self::StandardPlus,
            Self::StandardPlus => Self::Standard,
            Self::Standard => Self::Economy,
            Self::Economy => Self::Economy,
        }
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Economy => "economy",
            Self::Standard => "standard",
            Self::StandardPlus => "standard_plus",
            Self::Premium => "premium",
        };
        write!(f, "{}", s)
    }
}

/// A model available in the pool with cost and capability metadata.
#[derive(Debug, Clone)]
pub struct ModelProfile {
    pub model_string: String,
    pub input_cost_per_m: f64,
    pub output_cost_per_m: f64,
    pub tier: ModelTier,
    pub capabilities: HashSet<String>,
}

impl ModelProfile {
    /// Combined cost metric for sorting (sum of input + output per-million).
    fn combined_cost(&self) -> f64 {
        self.input_cost_per_m + self.output_cost_per_m
    }
}

/// Per-role requirements for model selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePreference {
    pub min_tier: ModelTier,
    pub required_capabilities: HashSet<String>,
}

impl Default for RolePreference {
    fn default() -> Self {
        Self {
            min_tier: ModelTier::Economy,
            required_capabilities: HashSet::new(),
        }
    }
}

impl fmt::Display for RolePreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let caps: Vec<&str> = self
            .required_capabilities
            .iter()
            .map(|s| s.as_str())
            .collect();
        let caps_str = caps.join(", ");
        write!(f, "min_tier={}, requires=[{}]", self.min_tier, caps_str)
    }
}

/// Rule-based model chooser. Selects the cheapest eligible model for a role,
/// taking into account tier requirements, capabilities, and budget pressure.
#[derive(Debug, Clone)]
pub struct ModelChooser {
    /// Available models, sorted by combined cost ascending at construction.
    models: Vec<ModelProfile>,
    /// Per-role preferences (role name → preference).
    role_preferences: HashMap<String, RolePreference>,
    /// 0.0 = quality-first, 1.0 = cost-first. Controls budget pressure sensitivity.
    cost_quality_threshold: f64,
}

impl ModelChooser {
    /// Create a new chooser. Models are sorted by cost at construction time.
    pub fn new(
        mut models: Vec<ModelProfile>,
        role_preferences: HashMap<String, RolePreference>,
        cost_quality_threshold: f64,
    ) -> Self {
        models.sort_by(|a, b| {
            a.combined_cost()
                .partial_cmp(&b.combined_cost())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            models,
            role_preferences,
            cost_quality_threshold,
        }
    }

    /// Select the cheapest eligible model for a role given remaining budget.
    ///
    /// Returns `None` if no models match the requirements.
    ///
    /// Selection logic:
    /// 1. Look up role preference (default: economy, no capabilities).
    /// 2. Filter models by required capabilities.
    /// 3. Compute budget pressure and possibly downgrade the effective minimum tier.
    /// 4. Filter by effective minimum tier (model.tier >= effective_min).
    /// 5. Return the cheapest eligible model (already sorted by cost).
    pub fn select(
        &self,
        role: &str,
        budget_remaining_dollars: f64,
        max_budget: f64,
    ) -> Option<&str> {
        let pref = self.role_preferences.get(role).cloned().unwrap_or_default();

        // Compute budget pressure: 1.0 when fully spent, 0.0 when full budget remains.
        let budget_pressure = if max_budget > 0.0 {
            (1.0 - (budget_remaining_dollars / max_budget)).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Determine effective minimum tier.
        let effective_min_tier = if budget_pressure > self.cost_quality_threshold {
            pref.min_tier.downgrade()
        } else {
            pref.min_tier
        };

        // Models are pre-sorted by cost ascending, so first match is cheapest.
        self.models
            .iter()
            .filter(|m| {
                // Must meet tier requirement.
                m.tier >= effective_min_tier
                    // Must have all required capabilities.
                    && pref
                        .required_capabilities
                        .iter()
                        .all(|cap| m.capabilities.contains(cap))
            })
            .map(|m| m.model_string.as_str())
            .next()
    }

    /// Try chooser first, fall back to the static routing entry.
    pub fn select_with_fallback<'a>(
        &'a self,
        role: &str,
        budget_remaining_dollars: f64,
        max_budget: f64,
        static_fallback: &'a str,
    ) -> &'a str {
        self.select(role, budget_remaining_dollars, max_budget)
            .unwrap_or(static_fallback)
    }

    /// Check if the model pool is empty.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Select a model one tier above the current one for the same role.
    ///
    /// Finds the cheapest eligible model whose tier is strictly higher than
    /// `current_tier`, while still satisfying the role's capability requirements.
    /// Returns `None` if no higher-tier model is available.
    pub fn escalate(&self, role: &str, current_tier: ModelTier) -> Option<&str> {
        let pref = self.role_preferences.get(role).cloned().unwrap_or_default();

        // Models are sorted by cost ascending — first match above current tier wins.
        self.models
            .iter()
            .filter(|m| {
                m.tier > current_tier
                    && pref
                        .required_capabilities
                        .iter()
                        .all(|cap| m.capabilities.contains(cap))
            })
            .map(|m| m.model_string.as_str())
            .next()
    }

    /// Select an ordered list of models for a role, from cheapest to most expensive.
    ///
    /// This is used to provide a fallback chain to the caller. The logic is identical
    /// to `select` but it returns all eligible models instead of just the first.
    pub fn select_with_fallbacks(
        &self,
        role: &str,
        budget_remaining_dollars: f64,
        max_budget: f64,
    ) -> Vec<ModelProfile> {
        let pref = self.role_preferences.get(role).cloned().unwrap_or_default();

        // Compute budget pressure: 1.0 when fully spent, 0.0 when full budget remains.
        let budget_pressure = if max_budget > 0.0 {
            (1.0 - (budget_remaining_dollars / max_budget)).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Determine effective minimum tier.
        let effective_min_tier = if budget_pressure > self.cost_quality_threshold {
            pref.min_tier.downgrade()
        } else {
            pref.min_tier
        };

        // Models are pre-sorted by cost ascending.
        self.models
            .iter()
            .filter(|m| {
                // Must meet tier requirement.
                m.tier >= effective_min_tier
                    // Must have all required capabilities.
                    && pref
                        .required_capabilities
                        .iter()
                        .all(|cap| m.capabilities.contains(cap))
            })
            .cloned()
            .collect()
    }

    /// Look up the tier for a model string. Returns `None` if unknown.
    pub fn tier_of(&self, model_string: &str) -> Option<ModelTier> {
        self.models
            .iter()
            .find(|m| m.model_string == model_string)
            .map(|m| m.tier)
    }

    /// Fallback to the cheapest available model regardless of role preferences.
    ///
    /// Returns `None` if the model pool is empty.
    pub fn fallback_for_role(&self) -> Option<&str> {
        self.models.first().map(|m| m.model_string.as_str())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn economy_model() -> ModelProfile {
        ModelProfile {
            model_string: "gemini/gemini-2.5-flash-lite".into(),
            input_cost_per_m: 0.075,
            output_cost_per_m: 0.30,
            tier: ModelTier::Economy,
            capabilities: HashSet::from(["tool_use".into(), "code".into()]),
        }
    }

    fn standard_model() -> ModelProfile {
        ModelProfile {
            model_string: "gemini/gemini-2.5-flash".into(),
            input_cost_per_m: 0.15,
            output_cost_per_m: 0.60,
            tier: ModelTier::Standard,
            capabilities: HashSet::from(["tool_use".into(), "code".into(), "long_context".into()]),
        }
    }

    fn premium_model() -> ModelProfile {
        ModelProfile {
            model_string: "anthropic/claude-sonnet-4-20250514".into(),
            input_cost_per_m: 3.0,
            output_cost_per_m: 15.0,
            tier: ModelTier::Premium,
            capabilities: HashSet::from([
                "tool_use".into(),
                "code".into(),
                "long_context".into(),
                "vision".into(),
            ]),
        }
    }

    fn test_models() -> Vec<ModelProfile> {
        vec![premium_model(), economy_model(), standard_model()]
    }

    fn test_chooser(threshold: f64) -> ModelChooser {
        let roles = HashMap::from([
            (
                "planner".into(),
                RolePreference {
                    min_tier: ModelTier::Standard,
                    required_capabilities: HashSet::new(),
                },
            ),
            (
                "implementer".into(),
                RolePreference {
                    min_tier: ModelTier::Standard,
                    required_capabilities: HashSet::from(["tool_use".into()]),
                },
            ),
            (
                "debugger".into(),
                RolePreference {
                    min_tier: ModelTier::Economy,
                    required_capabilities: HashSet::from(["tool_use".into()]),
                },
            ),
        ]);
        ModelChooser::new(test_models(), roles, threshold)
    }

    #[test]
    fn new_sorts_by_cost() {
        let chooser = test_chooser(0.7);
        let costs: Vec<f64> = chooser.models.iter().map(|m| m.combined_cost()).collect();
        for w in costs.windows(2) {
            assert!(w[0] <= w[1], "models not sorted: {} > {}", w[0], w[1]);
        }
    }

    #[test]
    fn select_cheapest_eligible() {
        let chooser = test_chooser(0.7);
        // Debugger has min_tier=Economy, plenty of budget → picks cheapest with tool_use
        let selected = chooser.select("debugger", 10.0, 10.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash-lite"));
    }

    #[test]
    fn select_respects_min_tier() {
        let chooser = test_chooser(0.7);
        // Planner has min_tier=Standard, plenty of budget → skips economy
        let selected = chooser.select("planner", 10.0, 10.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash"));
    }

    #[test]
    fn select_filters_by_capability() {
        // Create a model pool where the cheapest lacks tool_use
        let cheap_no_tools = ModelProfile {
            model_string: "local/tiny".into(),
            input_cost_per_m: 0.0,
            output_cost_per_m: 0.0,
            tier: ModelTier::Economy,
            capabilities: HashSet::from(["code".into()]),
        };
        let roles = HashMap::from([(
            "implementer".into(),
            RolePreference {
                min_tier: ModelTier::Economy,
                required_capabilities: HashSet::from(["tool_use".into()]),
            },
        )]);
        let chooser = ModelChooser::new(vec![cheap_no_tools, economy_model()], roles, 0.5);

        let selected = chooser.select("implementer", 10.0, 10.0);
        // Should skip "local/tiny" (no tool_use) and pick economy_model
        assert_eq!(selected, Some("gemini/gemini-2.5-flash-lite"));
    }

    #[test]
    fn select_budget_pressure_downgrades_tier() {
        let chooser = test_chooser(0.5); // threshold = 0.5
        // Planner needs Standard. With budget_pressure > 0.5 → downgrade to Economy
        // budget_pressure = 1.0 - (1.0 / 10.0) = 0.9 > 0.5 → downgrade
        let selected = chooser.select("planner", 1.0, 10.0);
        // Now economy is eligible → picks cheapest (economy)
        assert_eq!(selected, Some("gemini/gemini-2.5-flash-lite"));
    }

    #[test]
    fn select_threshold_zero_uses_preferred_tier() {
        let chooser = test_chooser(0.0); // threshold = 0.0 (quality-first: always downgrade)
        // Even with full budget, threshold=0.0 means budget_pressure (0.0) > 0.0 is false
        // So no downgrade with full budget
        let selected = chooser.select("planner", 10.0, 10.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash"));
    }

    #[test]
    fn select_threshold_one_always_cheapest() {
        let chooser = test_chooser(1.0); // threshold = 1.0 (cost-first)
        // budget_pressure = 0.0, not > 1.0, so no downgrade.
        // But planner still needs Standard, so cheapest Standard is picked.
        let selected = chooser.select("planner", 10.0, 10.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash"));

        // Even low budget: pressure = 0.9, not > 1.0
        let selected2 = chooser.select("planner", 1.0, 10.0);
        assert_eq!(selected2, Some("gemini/gemini-2.5-flash"));
    }

    #[test]
    fn select_no_eligible_returns_none() {
        let roles = HashMap::from([(
            "vision_agent".into(),
            RolePreference {
                min_tier: ModelTier::Economy,
                required_capabilities: HashSet::from(["vision".into(), "telepathy".into()]),
            },
        )]);
        let chooser = ModelChooser::new(test_models(), roles, 0.5);

        let selected = chooser.select("vision_agent", 10.0, 10.0);
        assert_eq!(selected, None);
    }

    #[test]
    fn select_unknown_role_uses_defaults() {
        let chooser = test_chooser(0.7);
        // Unknown role → default preference (economy, no capabilities) → cheapest model
        let selected = chooser.select("unknown_role", 10.0, 10.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash-lite"));
    }

    #[test]
    fn select_with_fallback_uses_chooser_when_match() {
        let chooser = test_chooser(0.7);
        let result = chooser.select_with_fallback("debugger", 10.0, 10.0, "fallback/model");
        assert_eq!(result, "gemini/gemini-2.5-flash-lite");
    }

    #[test]
    fn select_with_fallback_uses_static_when_no_chooser_match() {
        let roles = HashMap::from([(
            "special".into(),
            RolePreference {
                min_tier: ModelTier::Economy,
                required_capabilities: HashSet::from(["nonexistent_cap".into()]),
            },
        )]);
        let chooser = ModelChooser::new(test_models(), roles, 0.5);

        let result = chooser.select_with_fallback("special", 10.0, 10.0, "fallback/model");
        assert_eq!(result, "fallback/model");
    }

    #[test]
    fn model_tier_ordering() {
        assert!(ModelTier::Economy < ModelTier::Standard);
        assert!(ModelTier::Standard < ModelTier::StandardPlus);
        assert!(ModelTier::StandardPlus < ModelTier::Premium);
        assert!(ModelTier::Economy < ModelTier::Premium);
    }

    #[test]
    fn model_tier_from_str_loose() {
        assert_eq!(
            ModelTier::from_str_loose("economy"),
            Some(ModelTier::Economy)
        );
        assert_eq!(
            ModelTier::from_str_loose("STANDARD"),
            Some(ModelTier::Standard)
        );
        assert_eq!(
            ModelTier::from_str_loose("standard_plus"),
            Some(ModelTier::StandardPlus)
        );
        assert_eq!(
            ModelTier::from_str_loose("Premium"),
            Some(ModelTier::Premium)
        );
        assert_eq!(ModelTier::from_str_loose("invalid"), None);
    }

    #[test]
    fn model_tier_downgrade() {
        assert_eq!(ModelTier::Premium.downgrade(), ModelTier::StandardPlus);
        assert_eq!(ModelTier::StandardPlus.downgrade(), ModelTier::Standard);
        assert_eq!(ModelTier::Standard.downgrade(), ModelTier::Economy);
        assert_eq!(ModelTier::Economy.downgrade(), ModelTier::Economy);
    }

    #[test]
    fn is_empty_with_no_models() {
        let chooser = ModelChooser::new(vec![], HashMap::new(), 0.5);
        assert!(chooser.is_empty());
    }

    #[test]
    fn is_empty_with_models() {
        let chooser = test_chooser(0.5);
        assert!(!chooser.is_empty());
    }

    #[test]
    fn fallback_for_role_returns_cheapest_when_available() {
        let chooser = test_chooser(0.7);
        let selected = chooser.fallback_for_role();
        assert_eq!(selected, Some("gemini/gemini-2.5-flash-lite"));
    }

    #[test]
    fn fallback_for_role_returns_none_when_pool_empty() {
        let chooser = ModelChooser::new(vec![], HashMap::new(), 0.5);
        let selected = chooser.fallback_for_role();
        assert_eq!(selected, None);
    }

    #[test]
    fn select_with_zero_max_budget() {
        let chooser = test_chooser(0.5);
        // max_budget=0 → pressure=0 → no downgrade
        let selected = chooser.select("planner", 0.0, 0.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash"));
    }

    #[test]
    fn escalate_selects_higher_tier() {
        let chooser = test_chooser(0.7);
        // Implementer currently on Standard → escalate should pick Premium (claude)
        let escalated = chooser.escalate("implementer", ModelTier::Standard);
        assert_eq!(escalated, Some("anthropic/claude-sonnet-4-20250514"));
    }

    #[test]
    fn escalate_returns_none_at_top_tier() {
        let chooser = test_chooser(0.7);
        // Already on Premium → no higher tier available
        let escalated = chooser.escalate("implementer", ModelTier::Premium);
        assert_eq!(escalated, None);
    }

    #[test]
    fn escalate_respects_capabilities() {
        let roles = HashMap::from([(
            "special".into(),
            RolePreference {
                min_tier: ModelTier::Economy,
                required_capabilities: HashSet::from(["vision".into()]),
            },
        )]);
        let chooser = ModelChooser::new(test_models(), roles, 0.5);
        // Only Premium model (claude) has vision — escalate from Economy should find it
        let escalated = chooser.escalate("special", ModelTier::Economy);
        assert_eq!(escalated, Some("anthropic/claude-sonnet-4-20250514"));
    }

    #[test]
    fn tier_of_known_model() {
        let chooser = test_chooser(0.7);
        assert_eq!(
            chooser.tier_of("gemini/gemini-2.5-flash"),
            Some(ModelTier::Standard)
        );
        assert_eq!(
            chooser.tier_of("gemini/gemini-2.5-flash-lite"),
            Some(ModelTier::Economy)
        );
    }

    #[test]
    fn tier_of_unknown_model() {
        let chooser = test_chooser(0.7);
        assert_eq!(chooser.tier_of("unknown/model"), None);
    }

    #[test]
    fn role_preference_default() {
        let pref = RolePreference::default();
        assert_eq!(pref.min_tier, ModelTier::Economy);
        assert!(pref.required_capabilities.is_empty());
    }

    #[test]
    fn model_tier_display() {
        assert_eq!(ModelTier::Economy.to_string(), "economy");
        assert_eq!(ModelTier::Standard.to_string(), "standard");
        assert_eq!(ModelTier::StandardPlus.to_string(), "standard_plus");
        assert_eq!(ModelTier::Premium.to_string(), "premium");
    }

    #[test]
    fn role_preference_display_with_capabilities() {
        let pref = RolePreference {
            min_tier: ModelTier::Standard,
            required_capabilities: HashSet::from(["tool_use".into(), "code".into()]),
        };
        let display = pref.to_string();
        assert!(display.starts_with("min_tier=standard, requires=["));
        assert!(display.contains("tool_use"));
        assert!(display.contains("code"));
        assert!(display.ends_with("]"));
    }

    #[test]
    fn role_preference_display_empty_requires() {
        let pref = RolePreference {
            min_tier: ModelTier::Premium,
            required_capabilities: HashSet::new(),
        };
        assert_eq!(pref.to_string(), "min_tier=premium, requires=[]");
    }

    #[test]
    fn role_preference_partial_eq() {
        let pref1 = RolePreference {
            min_tier: ModelTier::Standard,
            required_capabilities: HashSet::from(["tool_use".into()]),
        };
        let pref2 = RolePreference {
            min_tier: ModelTier::Standard,
            required_capabilities: HashSet::from(["tool_use".into()]),
        };
        let pref3 = RolePreference {
            min_tier: ModelTier::Economy,
            required_capabilities: HashSet::from(["tool_use".into()]),
        };
        let pref4 = RolePreference {
            min_tier: ModelTier::Standard,
            required_capabilities: HashSet::from(["code".into()]),
        };

        // Test equality
        assert_eq!(pref1, pref2);

        // Test inequality - different tier
        assert_ne!(pref1, pref3);

        // Test inequality - different capabilities
        assert_ne!(pref1, pref4);
    }

    #[test]
    fn select_with_fallbacks_returns_ordered_list() {
        let chooser = test_chooser(0.7);
        // Planner has min_tier=Standard, plenty of budget.
        // Should return Standard and Premium models, ordered by cost.
        let fallbacks = chooser.select_with_fallbacks("planner", 10.0, 10.0);
        let model_names: Vec<String> = fallbacks.iter().map(|m| m.model_string.clone()).collect();

        assert_eq!(
            model_names,
            vec![
                "gemini/gemini-2.5-flash".to_string(),
                "anthropic/claude-sonnet-4-20250514".to_string()
            ]
        );
    }

    #[test]
    fn select_with_fallbacks_budget_pressure_downgrades() {
        let chooser = test_chooser(0.7);
        // Budget pressure > threshold (0.7): should downgrade min_tier.
        // Planner normally min_tier=Standard; downgraded to Economy.
        // All models (including Economy) should now be returned.
        let fallbacks = chooser.select_with_fallbacks("planner", 1.0, 10.0);
        assert!(fallbacks.len() > 2, "should include Economy models");
        // First model should be cheapest (Economy tier)
        assert!(
            fallbacks[0].tier == ModelTier::Economy,
            "first model should be Economy"
        );
    }

    #[test]
    fn select_with_fallbacks_zero_max_budget() {
        let chooser = test_chooser(0.7);
        // max_budget = 0.0 → budget_pressure = 0.0 → no downgrade.
        let fallbacks = chooser.select_with_fallbacks("planner", 0.0, 0.0);
        let model_names: Vec<String> = fallbacks.iter().map(|m| m.model_string.clone()).collect();
        assert_eq!(
            model_names,
            vec![
                "gemini/gemini-2.5-flash".to_string(),
                "anthropic/claude-sonnet-4-20250514".to_string()
            ]
        );
    }
}
