---
name: pr
description: Create a pull request from the current branch. Use when the user says "create a PR", "open a PR", "make a PR", or invokes /pr.
allowed-tools: Bash(git *), Bash(gh *)
---

You are creating a pull request for the current branch. Follow these steps:

1. Run these commands in parallel to understand the current state:
   - `git status` (never use -uall)
   - `git diff` to see staged and unstaged changes
   - Check if the current branch tracks a remote: `git rev-parse --abbrev-ref --symbolic-full-name @{u} 2>/dev/null`
   - `git log --oneline $(git merge-base HEAD main)..HEAD` to see all commits on this branch
   - `git diff main...HEAD --stat` to see the full diff summary against main

2. If there are uncommitted changes, ask the user whether to commit them first or create the PR with only committed changes.

3. Analyze ALL commits that will be in the PR (not just the latest). Draft:
   - A short PR title (under 70 chars) — use imperative mood
   - A description summarizing the changes

4. Push and create the PR:
   - Push to remote with `-u` if needed
   - Create using `gh pr create` with this format:

```
gh pr create --title "the pr title" --body "$(cat <<'EOF'
## Summary
<1-3 bullet points summarizing what changed and why>

## Test plan
- [ ] cargo test --workspace
- [ ] cargo clippy --all-targets -- -D warnings
- [ ] cargo fmt --check

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

5. Return the PR URL when done.

If `$ARGUMENTS` is provided, use it as context for the PR (e.g., target branch, title hint).
