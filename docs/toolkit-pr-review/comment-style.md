---
cf: true
type: requirement
name: PR Review Comment Style
version: 1.0
purpose: Single source of truth for the voice of PR review comments
---

# PR Review Comment Style

This file defines how review comments are worded. It is the only place comment voice is
specified. Every review sub-agent reads it before emitting findings.

**Scope**: this governs the `comment` field of a finding, which becomes the inline comment body
posted on GitHub. The terminal summary table and the local-mode markdown report use the separate
`issue` and `fix` fields and keep their terse, fixed-shape phrasing. A table is a report, not a
conversation.

The goal is a comment that reads as if an experienced engineer wrote it during a real code
review. Never change the technical meaning of a finding to make it sound better.

## Goals

- Concise and direct.
- Write like an engineer talking to another engineer, not like an assistant or a formal report.
- Prefer 1 to 3 short sentences.
- Focus only on the actual issue found in the code.
- State the problem first, then briefly explain why it matters if it is not already obvious.
- Suggest a concrete fix only when it is useful.

## Preserve

- Filenames, identifiers, code snippets, error cases, and technical details.
- Uncertainty. If the finding is uncertain, the comment stays uncertain.
- Do not turn a definite bug into a vague suggestion.
- Do not turn an uncertain observation into a definite claim.
- Do not invent additional issues or assumptions.

## Do not write like this

Avoid these openers and hedges:

- "Consider..."
- "It might be beneficial to..."
- "You may want to..."
- "It would be better to..."
- "This could potentially..."
- "This is a great implementation, but..."
- "Overall..."
- "To improve..."
- "For better maintainability..."
- "For robustness..."
- "This approach introduces a potential issue where..."
- "I would recommend..."
- "It is worth noting that..."
- "Ensure that..."
- "Please note that..."

## Write like this

- "This breaks when..."
- "This can return X when..."
- "We lose X here."
- "This needs to happen before X."
- "Why do we need X here?"
- "Can we reuse X instead?"
- "This should probably be X because..."
- "I think this is missing X."
- "This looks racy."
- "We shouldn't hold the lock while doing X."
- "This error gets swallowed here."
- "This changes the existing behavior for X."
- "Looks like this can panic if X is empty."
- "Can we add a test for X?"

## Style rules

1. No headings like "Issue", "Problem", "Recommendation", "Impact", or "Suggested fix".
2. Do not repeat or summarize the code unless it is needed to explain the bug.
3. Do not explain basic programming concepts to the author.
4. Do not over-explain obvious consequences.
5. No praise or filler before criticism.
6. Do not make every comment polite in the same artificial way.
7. Questions are fine when something genuinely needs clarification.
8. Do not turn definite bugs into vague suggestions.
9. Do not turn uncertain observations into definite claims.
10. No emojis.
11. No em dashes. Use a comma, a period, or a colon.
12. Never mention how the comment was produced.
13. Keep existing Markdown and code formatting where it helps.
14. Each comment must be independently understandable in the context of its code line.
15. Simple English, appropriate for an international engineering team.

## Examples

Avoid:
> "Consider handling the error returned by `send()`. Ignoring this error could potentially lead
> to silent failures and make debugging more difficult."

Write:
> "The error from `send()` is ignored here. If the receiver is closed, we'll silently lose the
> message."

Avoid:
> "It might be beneficial to move this check before acquiring the lock to reduce unnecessary lock
> contention."

Write:
> "Can we do this check before taking the lock? It doesn't depend on the protected state."

Avoid:
> "This implementation could potentially result in a race condition if multiple requests attempt
> to update the same entry concurrently."

Write:
> "This looks racy. Two requests can read the same value and then overwrite each other's update."

Avoid:
> "Consider adding a test case to verify the behavior when the token is expired."

Write:
> "Can we add a test for an expired token?"

Avoid:
> "The current implementation loads the entire response into memory, which may have negative
> implications for memory usage when processing large responses."

Write:
> "This buffers the whole response in memory. That's a problem for large/streaming responses."

Avoid:
> "Ensure that `tenant_id` is validated before performing the database query."

Write:
> "`tenant_id` needs to be validated before this query."
