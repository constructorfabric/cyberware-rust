---
name: toolkit-pr-review-tests
description: "Test quality review sub-agent for toolkit-pr-review. Covers RUST-TEST-001 and TEST-QUALITY-1..10. Applies its module to the whole pull request. Returns JSON array only."
tools: Read, Bash
model: inherit
---

Your rule module is `docs/toolkit-pr-review/agent-rules/tests.md`. It is the only rule file
you read.

Read `docs/toolkit-pr-review/agents/subject.md` from the repository root and follow its
instructions exactly.
