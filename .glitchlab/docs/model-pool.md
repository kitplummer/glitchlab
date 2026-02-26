# Model Pool Configuration

The ModelChooser, implemented in `crates/router/src/chooser.rs`,
manages how models are selected based on routing configuration.

## 1. `routing.models[]` Array Schema

This array defines the available models and their properties:

- `model`: (String) The unique identifier for the model (e.g., "gpt-4", "claude-3-opus").
- `tier`: (Integer) A numerical representation of the model's quality/capability tier. Higher numbers indicate better models.
- `capabilities`: (Array<String>) A list of features or capabilities the model possesses (e.g., "vision", "long_context").

## 2. `routing.roles{}` Map Schema

This map defines the requirements for different roles or use cases:

- `min_tier`: (Integer) The minimum `tier` a model must have to be considered for this role.
- `requires`: (Array<String>) A list of `capabilities` a model must possess to be considered.

## 3. Model Selection Logic

The ModelChooser selects models in the following order:

1. **Filter by Tier:** Exclude models with a `tier` less than the role's `min_tier`.
2. **Filter by Capabilities:** Exclude models that do not possess all `requires` capabilities for the role.
3. **Select Cheapest:** From the remaining eligible models, the chooser selects the one with the lowest cost.

## 4. `cost_quality_threshold` Field

This optional field (within a role configuration) allows for a trade-off between cost and quality. If specified, the chooser will prefer a model that is one tier higher than the cheapest eligible model, provided its cost does not exceed `cheapest_model_cost * (1 + cost_quality_threshold)`.

## 5. Example Configurations

### Budget-Constrained Setup

```yaml
routing:
  models:
    - model: "cheap-fast"
      tier: 1
      capabilities: []
    - model: "medium-quality"
      tier: 2
      capabilities: []
  roles:
    default:
      min_tier: 1
      requires: []
```

### Quality-Focused Setup

```yaml
routing:
  models:
    - model: "medium-quality"
      tier: 2
      capabilities: []
    - model: "premium-best"
      tier: 3
      capabilities: ["vision", "long_context"]
  roles:
    default:
      min_tier: 2
      requires: []
      cost_quality_threshold: 0.15
    vision_task:
      min_tier: 3
      requires: ["vision"]
```