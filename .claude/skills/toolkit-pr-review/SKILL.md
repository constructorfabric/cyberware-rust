---
name: toolkit-pr-review
description: "Review Rust changes against idiomatic Rust guidelines and ToolKit framework rules. PR mode posts inline comments on GitHub; local mode reviews a branch against main and writes a markdown report."
user-invocable: true
allowed-tools: Bash, Read, Glob, Grep, Write, Agent
---

# Rust PR Review

Review Rust code changes for quality and ToolKit framework compliance.

**Usage**:
- `/toolkit-pr-review <PR_NUMBER>` — review a GitHub PR, post findings as inline review comments
- `/toolkit-pr-review local [<branch-name>]` — review a local branch against `main`, write findings to a markdown file

---

## Table of Contents

- [Inputs](#inputs)
- [Modes](#modes)
- [Resolving the target repository (PR mode)](#resolving-the-target-repository-pr-mode)
- [Review guidelines](#review-guidelines)
- [Coding guidelines reference](#coding-guidelines-reference)
- [Steps](#steps)
- [Comment formatting rules](#comment-formatting-rules)
- [What NOT to do](#what-not-to-do)

---

## Inputs

- `<PR_NUMBER>` — the GitHub PR number (e.g. `123`). Selects **PR mode**.
- `local [<branch-name>]` — selects **local mode**. `<branch-name>` is optional; when omitted the current branch is used.
- `--repo <owner/repo>` — optional, PR mode only (e.g. `constructorfabric/gears-rust`)

## Modes

Resolve the mode from the first argument **before** doing anything else.

### PR mode

First argument is a number. Review target is the GitHub PR; findings are posted as inline review comments.

Set:
- `MODE=pr`
- `REVIEW_ID=<PR_NUMBER>`

### Local mode

First argument is the literal word `local`. Review target is the local branch diffed against `main`; findings are written to a markdown file — **nothing is posted to GitHub**.

Resolve:

```bash
BASE_BRANCH=main
BRANCH_NAME=<branch-name argument, or `git symbolic-ref --quiet --short HEAD` if omitted>
REPO_ROOT=$(git rev-parse --show-toplevel)
HEAD_SHA=$(git rev-parse "$BRANCH_NAME")
```

If no branch argument was given and `git symbolic-ref` fails, HEAD is detached — print the error and stop.
Verify the branch exists (`git rev-parse --verify "$BRANCH_NAME"`) and that `main` exists locally
(`git rev-parse --verify main`; if it does not, fall back to `origin/main` and use that as `BASE_BRANCH`).
If neither resolves, print the error and stop.

Set:
- `MODE=local`
- `BRANCH_SLUG` = `BRANCH_NAME` with `/` replaced by `-`
- `REVIEW_ID=local-$BRANCH_SLUG`

## Resolving the target repository (PR mode)

PR mode only — skip this in local mode. Before fetching PR data, determine which repository to use:

1. If `--repo` was provided in the arguments, use it.
2. Otherwise, check if an `upstream` remote exists: `git remote get-url upstream 2>/dev/null`. If it returns a URL, extract `owner/repo` from it.
3. Otherwise, fall back to the current repo via `gh repo view --json nameWithOwner -q .nameWithOwner`.

Store the result as `REPO` and pass `--repo $REPO` to all `gh pr` commands, and use it in API paths as `repos/$REPO/pulls/...`.

## Review guidelines

The rules live in six **rule modules** under `docs/toolkit-pr-review/rules/`, organised by subject. They are
not agents. Every review sub-agent reads every module.

The work is split by **subject, not by file**. One agent per module — `toolkit-pr-review-errors`,
`-security`, `-async`, `-design`, `-tests`, `-toolkit` — and each of them reads the whole diff. A
file is therefore read six times, once per subject, and that duplication is the point: the agent
holding `async.md` and nothing else goes deeper on every file than an agent juggling all six.

This replaces a design that sharded by file and gave every shard agent all six modules. Measured on
five real PRs, that arrangement reproduced 44% of the findings a careful human reviewer made on the
same PRs; the subject split reproduced 58%, at +55% tokens. The variable is how many rules one agent
holds at once (11–21 KB here, 55–94 KB then), not how many files it is given. It also recovers
defects that span files, which a shard agent cannot see because the other file belongs to someone
else.

One extra agent, `toolkit-pr-review-architecture`, runs once over the whole diff and owns
`RUST-ARCH-001`. It is the only structural pass. It reads `diff.patch`, not full files.

The orchestrator applies no rules itself. Its job is to classify files, spawn the seven agents,
and merge what comes back.

Two shared files every agent reads:
- `docs/toolkit-pr-review/review-conventions.md` — severity, criterion markers, reporting discipline
- `docs/toolkit-pr-review/comment-style.md` — comment wording

A file is **ToolKit-owned** — which gates the `TOOLKIT-*` rules in `docs/toolkit-pr-review/rules/toolkit.md`
— when **any** of these signals is present:

1. **Cargo.toml signals** — the nearest `Cargo.toml` (same crate or workspace member) declares a `toolkit` dependency/feature, or the crate name starts with `toolkit`.
2. **Path heuristics** — the file lives under a path that matches ToolKit gear conventions (e.g. `gears/*/src/`, `crates/toolkit-*/`, or similar namespace).
3. **Source-level symbols** — the file imports from ToolKit crates (`use toolkit_*`, `use crate::` inside a toolkit crate) or references ToolKit-specific types/traits such as `OperationBuilder`, `SecureConn`, `SecureORM`, `ClientHub`, or `GearLifecycle`.

A file that qualifies goes into `toolkit_files`, and that list **is** the file list the
`toolkit` agent is given. When it is empty the agent is not spawned at all.

`RUST-DEP-001` (dependency and advisory manifest hygiene) applies only to the manifest and config
files `prepare` collects into `manifest_files`: `Cargo.toml`, `Cargo.lock`, `deny.toml`, `.cargo/audit.toml`,
`.cargo/config.toml`, `clippy.toml`, `rust-toolchain.toml`. The `security` agent is given that list
separately and applies the rule to nothing else.

**This review is Rust-only.** `prepare` keeps `.rs` files plus the seven manifest and lint/advisory
config files, and records everything else in `skipped_files`. YAML, SQL migrations, `.proto`, Dockerfiles and CI
workflow files in the diff reach no agent and are not reviewed. Say so in the summary rather than
implying they were covered. Extending to them is a rules-authoring project, not a routing change:
there are no rules for those languages, and sending them to an agent that holds only Rust rules
would spend a reviewer's attention on generic observations.

## Coding guidelines reference

When reviewing, also consult:
- `docs/toolkit-pr-review/rules/*.md` — the six rule modules; every check ID lives in exactly one of them
- `docs/toolkit-pr-review/review-conventions.md` — severity, criterion markers, reporting discipline
- `docs/toolkit-pr-review/comment-style.md` — comment voice. The only authority on how findings are worded

---

## Steps

### Step 1: Prepare the review

One command resolves the target, fetches the diff, parses it, classifies every file, snapshots the
sources and decides which agents to spawn. Do not do any of that by hand.

**PR mode:**

```bash
python3 tools/scripts/toolkit-pr-review/review.py prepare --pr <PR_NUMBER> [--repo owner/name]
```

**Local mode:**

```bash
python3 tools/scripts/toolkit-pr-review/review.py prepare --local [--branch REF] [--base REF]
```

It prints `WORK_DIR=<path>` on the last line. Capture it; every later step reads from there.

```text
<WORK_DIR>/context.json    everything below
<WORK_DIR>/meta.json       PR metadata (PR mode) or branch + base (local mode)
<WORK_DIR>/diff.patch      the full diff
<WORK_DIR>/files/<repo/path>   full source snapshots, repo tree mirrored
<WORK_DIR>/out/            where agent output goes
```

Exit code `2` means the run would cost more than `--max-total-tokens`; nothing was written and the
message names the heaviest files. Raise the limit deliberately or narrow the review; do not work
around it by reviewing a subset by hand.

**Why this is a script and not instructions.** Both harnesses used to carry their own prose version
of these steps and had already drifted on five behaviours, so the same PR got a different review
depending on who ran it. Two of those divergences were outright defects: line ranges taken from
`@@` hunk headers include the context lines around a change, and on one measured PR 39% of that
window was context — a finding anchored there is posted against code the PR never touched; and
escaping `/` to `__` for snapshot filenames is not injective, so `gears/mini__chat/src/lib.rs` and
`gears/mini/chat/src/lib.rs` collide and one snapshot silently overwrites the other. `prepare`
walks hunk bodies line by line and mirrors the repo tree instead. Its behaviour is pinned by
fixture tests in `tools/scripts/toolkit-pr-review/tests/`.

### Step 2: Read context.json

`context.json` (schema 3) is the contract between `prepare` and everything downstream:

```json
{
  "schema_version": 3,
  "mode": "pr | local",
  "repo": "owner/name",
  "pr_number": 4777,
  "head_sha": "...",
  "base_sha": "...",
  "work_dir": "...",
  "files": {
    "<path>": {
      "status": "added | modified | deleted | renamed",
      "old_path": "<previous path, or null>",
      "toolkit_owned": true,
      "manifest": false,
      "snapshot": "files/<path>",
      "snapshot_ref": "head | base",
      "ranges": { "right": [[12, 18]], "left": [[40, 44]] }
    }
  },
  "all_files": ["..."],
  "skipped_files": ["..."],
  "toolkit_files": ["..."],
  "manifest_files": ["..."],
  "agents": [ { "name": "errors", "rules": "...", "files": ["..."], "manifest_files": [] } ],
  "totals": { "files": 24, "skipped": 1, "agents": 6, "est_tokens": 436611 }
}
```

The parts that matter downstream:

- **`ranges.right`** are added lines in the head file, and nothing else — no context lines. These are
  the valid targets for an ordinary comment.
- **`ranges.left`** are removed lines in the base file. A finding about code the PR deleted anchors
  here, with `"side": "LEFT"`, and lands on the removed line itself. There is no separate
  "deletion anchor": that scheme pointed at whichever line happened to survive next to the deletion.
- **`status: "deleted"`** means the file is gone at the head. Its snapshot is taken from the base
  (`snapshot_ref`), so the agent can still see what was removed, and a finding on it carries no
  `line` at all — it posts as a file-level comment.
- **`snapshot`** is a path under `files/` that mirrors the repo tree. `gears/foo/src/lib.rs` is at
  `<WORK_DIR>/files/gears/foo/src/lib.rs`. There is no filename escaping.
- **`agents`** is the spawn list for Step 3, already scoped: `toolkit` carries only ToolKit-owned
  files and is absent entirely when there are none, and only `security` carries `manifest_files`.
- **`skipped_files`** are files in the diff that no rule covers. Say so in the summary rather than
  implying they were reviewed.

`Cargo.lock` is deliberately never snapshotted: it is generated, routinely over 300 KB, and
everything `RUST-DEP-001` needs from it — a `source = "git+..."` entry, a non-crates.io registry —
is visible in `diff.patch`, which every agent already has. It stays in `manifest_files` so a finding
can still anchor on a changed line.

### Step 3: Spawn parallel sub-agents

Spawn the **agents listed in `context.json`'s `agents` array**, all in parallel in a single message.

That array already carries `architecture` alongside the six subject agents, so iterate it and spawn
each entry once. Do not add a second `toolkit-pr-review-architecture` on top of it: the architecture
pass runs exactly once, and a duplicate doubles its cost and produces the same findings twice, which
the `(file, line, id)` pass in Step 4 then silently swallows.

Pass to every agent:
- The review target identity from context (PR number + repo, or branch + base branch)
- Paths to `context.json`, `diff.patch`, and the `files/` directory in /tmp

Pass to each subject agent additionally:
- **Its file list, written out in the prompt.** Do not make the agent derive it from
  `context.json`; give it the paths. For five of the six that list is `all_files`; for `toolkit`
  it is `toolkit_files`.
- For `security` only, the `manifest_files` list as well.

Subject agent prompt shape:

```text
You are the <module> review agent for <target>.

Context:  <WORK_DIR>/context.json
Diff:     <WORK_DIR>/diff.patch
Sources:  <WORK_DIR>/files/<repo/path>   (the repo tree is mirrored)

Your rule module, the only one you read:
  docs/toolkit-pr-review/agent-rules/<module>.md

The PR changes these <K> files:
  gears/mini-chat/src/lib.rs
  gears/mini-chat/src/service.rs
  ...

Follow docs/toolkit-pr-review/agents/subject.md exactly. Also read
docs/toolkit-pr-review/review-conventions.md and docs/toolkit-pr-review/comment-style.md.
Walk the files one at a time and apply every criterion of your module to each.
You hold one module, so go deep: a second and third finding in the same file is expected.
Return a JSON array only.
```

**The "go deep" line is load-bearing and must stay in the prompt.** It also appears in
`subject.md`, and that duplication is deliberate, not an oversight. Measured on two agents: with the
line in the prompt, 23 findings; with it only in `subject.md`, 16, and the count of files carrying
two or more findings fell from 3 to 0 — the agent reverted to one finding per file. An instruction
in the spawn prompt is in context from the first turn; the same sentence inside a 6 KB document the
agent opens with a tool call is not. Do not tidy it away as a repeat.

Do **not** tell a subject agent to read the other five modules, and do not hand it the authored
`rules/` corpus. `agent-rules/` is the generated, version-gated copy and is what the measurement
was run against; `rules/` carries rationale the agent does not need and criteria the pinned
toolchain cannot trigger.

The architecture agent gets no file list: it owns the whole diff.

```text
You are the PR-level architecture pass for the review of <target>.

Context:  <WORK_DIR>/context.json
Diff:     <WORK_DIR>/diff.patch

Follow docs/toolkit-pr-review/agents/architecture.md exactly. Emit only RUST-ARCH-001.
Work from the diff; do not read whole files from files/. Return a JSON array only.
```

Check IDs by agent. The marker comments delimit the routing table for
`tools/scripts/toolkit-pr-review/lint.py`, which checks it against the rule modules in both
directions. Keep them, and keep the table between them, wherever this section moves.

<!-- pr-review:routing-table -->

| Agent | Check IDs |
|---|---|
| `toolkit-pr-review-errors` (×1) | RUST-ERR-001, RUST-PANIC-001, RUST-NO-001, RUST-NO-002, RUST-NO-003 |
| `toolkit-pr-review-security` (×1) | RUST-SEC-001, RUST-SEC-002, RUST-NO-006, RUST-DEP-001 |
| `toolkit-pr-review-async` (×1) | RUST-ASYNC-001, RUST-CONC-001, RUST-PERF-001, RUST-NO-004, RUST-NO-005 |
| `toolkit-pr-review-design` (×1) | RUST-API-001, RUST-TYPE-001, RUST-OWN-001, RUST-DATA-001, RUST-OBS-001, RUST-OBS-002, RUST-MOD-001, RUST-LINT-001, RUST-NO-007 |
| `toolkit-pr-review-tests` (×1) | RUST-TEST-001, TEST-QUALITY-1..10 |
| `toolkit-pr-review-toolkit` (×1) | TOOLKIT-CORE-001..003, TOOLKIT-REST-001..003, TOOLKIT-ERR-001..002, TOOLKIT-SEC-001..002, TOOLKIT-DB-001..002, TOOLKIT-CLIENT-001..002, TOOLKIT-ODATA-001, TOOLKIT-LIFE-001, TOOLKIT-OOP-001 |
| `toolkit-pr-review-architecture` (×1) | RUST-ARCH-001 |

<!-- /pr-review:routing-table -->

The per-file scoping in each module's **Scope of this module** section still applies: the `toolkit`
agent runs only on `toolkit_files`, and `RUST-DEP-001` only on `manifest_files`.

Each agent returns a JSON array of findings. See `docs/toolkit-pr-review/agents/subject.md`
and `docs/toolkit-pr-review/agents/architecture.md` for the detailed prompts.

### Step 4: Collect and merge findings

Wait for all Agent calls to complete. For each result:

1. Extract JSON array from output: find the first `[` and last `]`, parse that substring as JSON.
2. If not valid JSON, log a warning to terminal and treat as `[]`.
3. Append valid findings to a combined list in agent order — errors, security, async, design, tests,
   toolkit — then the architecture agent's.

Deduplicate in three passes, **in this order**:

1. Drop any finding where `(file, line, id)` duplicates an earlier one.
2. **PR mode only:** drop any finding that repeats a comment already on the pull request.
3. Then collapse by `(file, line)` **regardless of `id`** — but only when the findings describe the
   **same defect**. Two defects that happen to share a line both survive.

The order matters, and pass 2 must run before pass 3. Measured on PR 4785, one collapse group
(`service.rs:87`) held three findings: a duplicate of an existing comment, a duplicate of another
existing comment, and one novel finding. Collapsing first kept the duplicate, because severity was
tied and it came earlier in agent order, and dropped the only finding worth posting. Removing the
duplicates first leaves the novel one to win its own group.

#### Pass 2: findings already covered by a comment on the PR

A PR under review has usually been reviewed before — by a human, by another bot, or by an earlier
run of this tool. Posting the same finding again wastes the author's attention and buries whatever
is new. Fetch the existing top-level comments first:

```bash
gh api --paginate "repos/$REPO/pulls/<PR_NUMBER>/comments?per_page=100" \
  --jq '.[] | select(.in_reply_to_id==null) | "\(.path):\(.line) \(.body | split("\n")[0])"'
```

`select(.in_reply_to_id==null)` matters: thread replies, `**RESOLVED**` follow-ups and answers from
the author are not findings, and counting them inflates the list several-fold.

**Match by topic, not by line number.** On PR 4747 the existing review was written against an
earlier commit, so line numbers had moved: 2 of 20 findings matched an existing comment positionally
while reading the headlines put 10 of them on a defect already reported. Across PRs 4785, 4747 and
4711 (80 findings) 40% matched an existing comment on the exact line and 51% within two lines, so a
positional check is a useful first cut and a bad last word. Read the first line of each existing
comment — it is the headline — and drop a finding when the defect is the same, wherever it is
anchored.

A finding that a resolved comment covered still gets dropped: the author has already seen it. If the
code shows it was not actually fixed, that is worth posting, but say so rather than restating the
original.

#### Pass 3: two findings on one line

Every agent sees every file, so collisions on one line are routine. Across the same three PRs, 22 of
80 findings (27.5%) landed on a line another finding already held.

Collapsing all of them is wrong. Of those 22, 10 described a **different** defect than the finding
that would have survived — 12.5% of every finding produced. On `oop.rs:196` in PR 4711 three agents
fired on one line with three unrelated defects: the master's OTLP credentials reaching gear-visible
config, log identity falling back to local config, and the branch having no test. A single comment
cannot carry all three, and two of them are not a wording variant of the third.

So:

- **Same defect** — keep the highest severity, drop the rest. If the survivor does not mention what
  a dropped one covered, extend it in a sentence.
- **Different defects** — keep both, and post both as separate comments on that line. GitHub allows
  it. Human reviewers in this repo do it: 10 of 281 real top-level comments across ten PRs sit on a
  line that already had one, and on PR 4747 two such comments were both substantive and the author
  fixed both.

Judge this on the `issue` field, which states the defect. Two findings whose `issue` lines would
take different fixes are different defects. Cap it at two comments per line — beyond that the line
is telling you the change itself is doing too much, which is `RUST-ARCH-001` territory, not four
comments.

A caveat none of the passes handle: two agents describing the **same** defect one line apart
(`runner.rs:352` and `:353`) survive as two findings, because the key is exact. This is common
against a prior review as well — on PR 4711 a finding on `logging.rs:117` restated an existing
comment on `:118`. When two adjacent findings clearly describe one defect, keep the more severe and
fold the other in by hand.

Apply filter rules:
- Drop any finding whose `id` is not in the emitting agent's row of the routing table. An agent
  reporting outside its module means its module list was not respected; log the count to terminal.
- Drop any finding from the `toolkit` agent whose `file` is not in `toolkit_files`.
- For a finding whose file has `status: "deleted"`: keep it regardless of `line` (it has none — it posts as a file-level comment in Step 5).
- For a `RUST-ARCH-001` finding with no `line`: keep it, it posts as a file-level comment in Step 5.
- For every other finding, validate against the side it claims:
  - no `side`, or `"side": "RIGHT"` — `line` must fall inside `files[file].ranges.right`.
  - `"side": "LEFT"` — `line` must fall inside `files[file].ranges.left`.
  Drop it otherwise. Do not "fix" a line by moving it to the nearest valid one: a comment on the
  wrong line is worse than no comment, because the reader cannot tell it is misplaced.
- Drop style-only issues that rustfmt or clippy should catch. This includes anything the checklist
  marks `Enforcement: clippy ... (deny)` — `Cargo.toml` `[workspace.lints]` already fails the build
  on it, so posting it costs a slot a real finding needs.
- Drop findings whose `issue` is speculative rather than evidenced: a hypothetical problem, not an
  observed one. Judge this on the `issue` field only. Do **not** filter on `comment` wording —
  hedged phrasing there ("this should probably be X", "looks like this can panic if...") is
  intentional when the finding itself is uncertain, and is required by
  `docs/toolkit-pr-review/comment-style.md`.

Sort by severity: CRITICAL → HIGH → MEDIUM → LOW.

**Drop every `LOW` finding. Post every `CRITICAL`, `HIGH` and `MEDIUM`, however many there are.**
There is no total cap. Log the count dropped to terminal (e.g. "Dropped 3 LOW findings; posting 47").

This replaces a flat cap of 30. The cap was measured against PR 4705, where the review produced 76
findings: 30 were posted and 46 were cut, and because severity decides the order, everything posted
was CRITICAL or HIGH while 30 HIGH and 15 MEDIUM fell off the end. Two of the discarded findings were
defects the previous version of this tool had posted and a human had acted on. A cut that deep is
not prioritisation, it is loss, and it is invisible: nothing in the PR says 46 findings existed.

Severity is currently a poor sort key — 24 of those 76 were marked CRITICAL, which is not credible
for one PR — so cutting by rank cuts close to arbitrarily. Until severity is recalibrated, the only
safe line is the one where a finding is explicitly not worth a reviewer's attention, and that is what
`LOW` means.

If the result is a large review, say so in the summary rather than trimming it: a PR that genuinely
carries 50 real findings is information the author needs.

This merged, filtered, sorted list becomes the input to Step 5.

### Step 5 (PR mode): Post inline review comments on GitHub

Local mode skips this step entirely — go to Step 5L.

Split the merged findings into two groups:
- **Line-anchored findings** — the finding has a `line`. Post together in one review (below).
- **File-level findings** — the finding has **no** `line`. Two cases produce these: a file in
  a deleted file, which has no line on either side, and a `RUST-ARCH-001` finding that no single
  line represents. Neither can go in the batch review's `comments` array (which requires
  `line`+`side`). Post each individually via the single-comment endpoint with
  `subject_type: "file"` and no `line`/`side`:

```bash
gh api repos/$REPO/pulls/<PR_NUMBER>/comments \
  --method POST \
  -f commit_id="<HEAD_SHA>" \
  -f path="gears/foo/src/tests.rs" \
  -f subject_type="file" \
  -f body=$'**HIGH**\n\n`send_dead_letter_on_timeout` is gone and nothing links a follow-up. If it was failing, we lose both the coverage and the signal.'
```

Note the `$'...'` quoting: inside plain double quotes bash keeps `\n` as two literal characters and
`gh` posts them verbatim, so the comment renders with visible `\n`. ANSI-C quoting turns them into
real newlines, and backticks stay literal inside it. The JSON-payload path below does not need this
(there `\n` is a JSON escape that the parser turns into a newline).

Post these after the batch review below. If there are zero findings in both groups, skip straight to the "zero findings" review call.

Use `gh api` to create a pull request review with the line-anchored inline comments.

Build the review payload:

IMPORTANT: The `gh api` `-f` array syntax is limited. For multiple comments, build a JSON file and POST it.

The review `body` MUST be empty string — no summary in the review itself. The summary goes to the terminal only (Step 6).

Each comment `body` is built from the finding's `comment` field, with the severity header added
only for the two top severities:

```text
CRITICAL or HIGH  ->  "**<SEVERITY>**\n\n<comment>"
MEDIUM or LOW     ->  "<comment>"
```

Never post `issue` or `fix` as comment text — those two fields exist for the summary table and the
local-mode report. The second example below shows a MEDIUM comment, with no header.

The payload goes **inside this review's work directory**, never at a bare `/tmp/` path. A fixed
filename is shared by every concurrent review in the machine, including reviews of *different* PRs,
so one run can overwrite another's payload and post it to the wrong pull request.

```bash
cat > "$WORK_DIR/review-payload.json" << 'REVIEW_EOF'
{
  "commit_id": "<HEAD_SHA>",
  "event": "COMMENT",
  "body": "",
  "comments": [
    {
      "path": "gears/foo/src/domain/service.rs",
      "line": 42,
      "side": "RIGHT",
      "body": "**HIGH**\n\nThe original error gets dropped by `map_err(|_| ...)` here. When this fails in production we only see the generic variant, not what actually went wrong."
    },
    {
      "path": "gears/foo/src/domain/service.rs",
      "line": 77,
      "side": "RIGHT",
      "body": "Why do we need the intermediate `Vec` here? The iterator is consumed once right below."
    },
    {
      "path": "gears/foo/src/domain/service_tests.rs",
      "line": 140,
      "side": "LEFT",
      "body": "**MEDIUM**\n\nThis test is removed with no follow-up issue and no note saying why."
    }
  ]
}
REVIEW_EOF

gh api repos/$REPO/pulls/<PR_NUMBER>/reviews \
  --method POST \
  --input "$WORK_DIR/review-payload.json"
```

**`side` comes from the finding, not from a default.** A finding that carries `"side": "LEFT"` is
about a line this PR removed, and its `line` is a base-file number from `ranges.left`; emitting it
as `RIGHT` points the comment at an unrelated line in the head file, which GitHub accepts without
complaint. Everything else is `"side": "RIGHT"`.

If there are zero line-anchored findings but at least one file-level finding, skip this review call and post only the file-level comments above.

If there are zero findings in both groups, skip the payload above and post a single review whose
`body` carries the message instead of an inline comment:

```bash
gh api repos/$REPO/pulls/<PR_NUMBER>/reviews \
  --method POST \
  -f commit_id="<HEAD_SHA>" \
  -f event="COMMENT" \
  -f body="No issues found."
```

### Step 5L (local mode): Write the markdown report

Write the report to `<REPO_ROOT>/REVIEW_<BRANCH_SLUG>.md`, overwriting any existing file at that path.

Structure:

```markdown
# Branch Review: <BRANCH_NAME>

**Branch:** <meta.json `branch`> (base: <context.json `base_sha`, short>)
**Head:** <context.json `head_sha`, short>
**Date:** <today's date, YYYY-MM-DD>
**Commits:** <`git rev-list --count <base_sha>..<head_sha>`>
**Files changed:** <context.json `totals.files`> reviewed, <`totals.skipped`> skipped

## Findings

### Critical

- **<the finding's `issue`>** — `<file>:<line>` (`<ID>`)
  - **Why:** <the finding's `comment`>
  - **Fix:** <the finding's `fix`>

### High

...

### Medium

...

### Low

...

## Summary

| # | ID | Sev | Location | Issue | Fix |
|---|----|-----|----------|-------|-----|
| 1 | RUST-ERR-001 | HIGH | service.rs:42 | Error context lost | Preserve source error |
```

Rules for the report:
- One severity section per severity present. Omit empty sections.
- The report uses all three text fields, one per line: `issue` is the bolded headline, `comment`
  fills `**Why:**` (it is the field that carries the reasoning), and `fix` fills `**Fix:**`. This is
  a report rather than a conversation, so the labelled fixed shape is correct here even though
  inline comments drop it. The checklist ID **is** included, since there is no separate
  terminal-only table.
- File paths are repo-relative and include the line number, so they are clickable. Exception: a finding with no `line` (a deleted file, or a file-level `RUST-ARCH-001`) — render just the file path (e.g. `` `gears/foo/src/tests.rs` ``).
- If there are zero findings, write the header plus a single line: `No issues found.`

### Step 6: Print summary

After posting (PR mode) or writing the file (local mode), print a compact summary table to the terminal:

**PR mode:**

```text
## Rust PR Review: #<PR_NUMBER>

| # | ID | Sev | Location | Issue | Fix |
|---|----|-----|----------|-------|-----|
| 1 | RUST-ERR-001 | HIGH | service.rs:42 | Error context lost | Preserve source error |
| 2 | TOOLKIT-SEC-001 | CRIT | handler.rs:18 | Raw DB connection | Use SecureConn |
| 3 | TEST-QUALITY-9 | HIGH | tests.rs | Test deleted, no follow-up | Restore or track |

For a finding with no `line`, the Location column shows just the file path (no `:<line>`), since it was posted as a file-level comment.

Posted <N> inline comments on PR #<PR_NUMBER>.
```

**Local mode:** same table, with the header `## Rust Branch Review: <BRANCH_NAME>` and a final line:

```text
Wrote <N> findings to <REPO_ROOT>/REVIEW_<BRANCH_SLUG>.md
```

---

## Comment formatting rules

**Wording is defined in `docs/toolkit-pr-review/comment-style.md`, and nowhere else.** That file is the
contract: read it rather than relying on a summary here, because a summary is what drifts. The
sub-agents produce the text in the finding's `comment` field. Do not rewrite it here beyond fixing
an outright style violation, and never substitute `issue` + `fix` for it.

The body has exactly two shapes, chosen by severity:

```text
CRITICAL or HIGH   **<SEVERITY>**
                   <blank line>
                   <comment>

MEDIUM or LOW      <comment>
```

The header is dropped for MEDIUM and LOW on purpose: a bold severity tag on every comment is the
clearest signal that a machine wrote it, and severity is still carried by the terminal summary
table (Step 6) and the local-mode report (Step 5L), so nothing is lost for triage.

Do NOT include checklist IDs (e.g. RUST-ERR-001, TOOLKIT-SEC-001) in inline comments. IDs appear
only in the terminal summary table (Step 6) and — in local mode — in the markdown report.

Mechanical rules, which the style file does not cover:
- One issue per comment. If a line has two problems, post two comments.
- Line number must point to an added/modified line that exists in the diff. Do not comment on unchanged lines.
- If you cannot determine the exact line, do not guess — skip that finding. Exception: a finding with no line by design — one on a file whose `status` is `deleted`, or a `RUST-ARCH-001` that no single line represents — posts as a file-level comment (see Step 5). Don't skip it and don't force a line onto it.

---

## What NOT to do

What counts as a finding, how severity is assigned, and the discipline around evidence, clippy
markers and toolchain gating are defined once in `docs/toolkit-pr-review/review-conventions.md`. They are
not restated here. This list covers only what is specific to orchestrating the run:

- Do not approve or request changes — use `event: "COMMENT"` only
- Do not post anything to GitHub in local mode — the markdown report is the only output artifact
- Do not commit the generated `REVIEW_*.md` file
- Do not post comments on lines outside the diff
- Do not drop a `CRITICAL`, `HIGH` or `MEDIUM` finding to shorten the review. Only `LOW` is dropped.
- If there are zero findings, post a single review comment: "No issues found." (PR mode) / write `No issues found.` into the report (local mode)
