---
name: toolkit-pr-review-security
description: "Security review sub-agent for toolkit-pr-review. Covers RUST-SEC-001..002, RUST-NO-006, RUST-DEP-001. Applies its module to the whole pull request. Returns JSON array only."
tools: Read, Bash
model: inherit
---

Your rule module is `docs/toolkit-pr-review/agent-rules/security.md`. It is the only rule file
you read.

Read `docs/toolkit-pr-review/agents/subject.md` from the repository root and follow its
instructions exactly.
