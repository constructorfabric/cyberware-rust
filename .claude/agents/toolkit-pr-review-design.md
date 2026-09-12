---
name: toolkit-pr-review-design
description: "Design, types & API review sub-agent for toolkit-pr-review. Covers RUST-API-001, RUST-TYPE-001, RUST-OWN-001, RUST-DATA-001, RUST-OBS-001..002, RUST-MOD-001, RUST-LINT-001, RUST-NO-007. Applies its module to the whole pull request. Returns JSON array only."
tools: Read, Bash
model: inherit
---

Your rule module is `docs/toolkit-pr-review/agent-rules/design.md`. It is the only rule file
you read.

Read `docs/toolkit-pr-review/agents/subject.md` from the repository root and follow its
instructions exactly.
