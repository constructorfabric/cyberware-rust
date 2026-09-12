---
name: toolkit-pr-review-toolkit
description: "ToolKit framework compliance review sub-agent for toolkit-pr-review. Covers all 17 TOOLKIT-* rules, on ToolKit-owned files only. Applies its module to the whole pull request. Returns JSON array only."
tools: Read, Bash
model: inherit
---

Your rule module is `docs/toolkit-pr-review/agent-rules/toolkit.md`. It is the only rule file
you read.

Read `docs/toolkit-pr-review/agents/subject.md` from the repository root and follow its
instructions exactly.
