---
name: toolkit-pr-review-async
description: "Async, concurrency & performance review sub-agent for toolkit-pr-review. Covers RUST-ASYNC-001, RUST-CONC-001, RUST-PERF-001, RUST-NO-004..005. Applies its module to the whole pull request. Returns JSON array only."
tools: Read, Bash
model: inherit
---

Your rule module is `docs/toolkit-pr-review/agent-rules/async.md`. It is the only rule file
you read.

Read `docs/toolkit-pr-review/agents/subject.md` from the repository root and follow its
instructions exactly.
