---
name: toolkit-pr-review-errors
description: "Error handling & panic safety review sub-agent for toolkit-pr-review. Covers RUST-ERR-001, RUST-PANIC-001, RUST-NO-001..003. Applies its module to the whole pull request. Returns JSON array only."
tools: Read, Bash
model: inherit
---

Your rule module is `docs/toolkit-pr-review/agent-rules/errors.md`. It is the only rule file
you read.

Read `docs/toolkit-pr-review/agents/subject.md` from the repository root and follow its
instructions exactly.
