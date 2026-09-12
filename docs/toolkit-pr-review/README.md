# toolkit-pr-review

Everything the `/toolkit-pr-review` skill reads at review time. This is **not** the Constructor
Studio review flow: that one lives in `docs/pr-review/` and is driven by `/cf-gears-pr-review`.
The two are unrelated and share no files.

| Path | What it is | Read by |
|---|---|---|
| `agents/subject.md` | Shared working instructions for the six subject agents | every subject agent |
| `agents/architecture.md` | Prompt for the PR-level pass | one agent per review |
| `rules/*.md` | The six rule modules, 51 check IDs. **Authored here, by hand** | humans |
| `agent-rules/*.md` | Generated from `rules/`: rationale stripped, version-gated criteria dropped | one module per agent |
| `comment-style.md` | How a finding is worded. The only authority on comment voice | every agent |
| `review-conventions.md` | Severity, criterion markers, reporting discipline | every agent |
| `RULES.md` | Generated index of every rule. Not authoritative, never hand-edited | humans |

Review work is split **by subject, not by file**: one agent per rule module, each reading the whole
diff, plus the architecture pass. An agent holds one module and nothing else. Measured across five
real PRs, that reproduced 58% of a careful human reviewer's findings against 44% for the
file-sharding design it replaced; what moved the number is how many rules one agent carries at once,
not how many files it is given. `tools/scripts/toolkit-pr-review/budget.py` carries the table.

Edit `rules/`, never `agent-rules/`. Regenerate with
`python3 tools/scripts/toolkit-pr-review/review.py render-rules`; CI fails on a stale copy.

The orchestrators live outside this directory: `.claude/skills/toolkit-pr-review/SKILL.md` and
`.devin/workflows/toolkit-pr-review.md`. `.claude/agents/toolkit-pr-review-*.md` are thin
registration stubs that point back at the prompts here.

`tools/scripts/toolkit-pr-review/lint.py` checks that the rules, the orchestrators and
`Cargo.toml` agree, and
regenerates `RULES.md` with `--write-index`.
