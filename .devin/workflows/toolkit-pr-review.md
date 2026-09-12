---
description: Review a GitHub PR for Rust + ToolKit compliance. Runs one agent per rule module in parallel over the whole diff, plus one PR-level architecture pass, then posts inline comments.
---

# ToolKit PR Review

Review a GitHub PR for Rust + ToolKit compliance and post inline comments.

**Usage**: `/toolkit-pr-review <PR_NUMBER> [--repo <owner/repo>]`

The rules live in six modules under `docs/toolkit-pr-review/rules/`, rendered for agents into
`docs/toolkit-pr-review/agent-rules/`. The work is split by **subject**, not by file: one agent per
module (Step 1), each reading the whole diff, so a file is read once per subject. One extra agent
runs over the whole diff and owns `RUST-ARCH-001`. Agent prompts live in `docs/toolkit-pr-review/agents/`. The shared
conventions every agent follows live in `docs/toolkit-pr-review/review-conventions.md` (severity, criterion
markers, reporting discipline) and `docs/toolkit-pr-review/comment-style.md` (comment wording).

---

## Table of Contents

- [Step 1: Prepare the review](#step-1-prepare-the-review)
- [Step 2: Read context.json](#step-2-read-contextjson)
- [Step 3: Run review agents in parallel](#step-3-run-review-agents-in-parallel)
- [Step 4: Collect and merge findings](#step-4-collect-and-merge-findings)
- [Step 5: Post inline review comments](#step-5-post-inline-review-comments)
- [Step 6: Print summary table](#step-6-print-summary-table)

---

## Step 1: Prepare the review

One command resolves the repository, fetches the PR metadata and diff, parses the diff, classifies
every file, snapshots the sources and decides which agents to spawn. Do not do any of that by hand.

// turbo
```bash
export PR_NUMBER=<PR_NUMBER>
D="/tmp/toolkit-pr-review-${PR_NUMBER}"
python3 tools/scripts/toolkit-pr-review/review.py prepare --pr "${PR_NUMBER}" --work-dir "${D}"
```

Add `--repo owner/name` when the PR is not in the repository the working copy points at.

Exit code `2` means the run would cost more than `--max-total-tokens`; nothing was written and the
message names the heaviest files. Raise the limit deliberately or narrow the review. Do not work
around it by reviewing a subset by hand.

What it writes under `${D}`:

```text
context.json          file lists, per-file line ranges, the agent spawn list
meta.json             gh pr view output: title, body, headRefOid, baseRefOid, refs
diff.patch            the full diff
files/<repo/path>     full source snapshots, repo tree mirrored (no filename escaping)
out/                  where the agents write their JSON
```

**Why this is a script and not instructions.** This workflow and the Claude skill used to carry two
prose copies of these steps and had already drifted on five behaviours, so the same PR got a
different review depending on which harness ran it. Two of those divergences were defects: line
ranges taken from `@@` hunk headers include the context lines around a change, and on one measured
PR 39% of that window was context, so a finding anchored there posts against code the PR never
touched; and escaping `/` to `__` for snapshot filenames is not injective, so
`gears/mini__chat/src/lib.rs` and `gears/mini/chat/src/lib.rs` collide and one snapshot silently
overwrites the other. `prepare` walks hunk bodies line by line and mirrors the repo tree instead,
and its behaviour is pinned by fixture tests in `tools/scripts/toolkit-pr-review/tests/`.

## Step 2: Read context.json

`context.json` (schema 3) is the contract between `prepare` and every step below.

// turbo
```bash
export PR_NUMBER=<PR_NUMBER>
D="/tmp/toolkit-pr-review-${PR_NUMBER}"
jq '{files: .totals.files, skipped: .totals.skipped, agents: [.agents[].name],
     est_tokens: .totals.est_tokens, toolkit: (.toolkit_files | length),
     manifests: .manifest_files}' "${D}/context.json"
```

The parts that matter downstream:

- **`files["<path>"].ranges.right`** — added lines in the head file, and nothing else. The valid
  targets for an ordinary comment.
- **`files["<path>"].ranges.left`** — removed lines in the base file. A finding about code the PR
  deleted anchors here with `"side": "LEFT"` and lands on the removed line itself. There is no
  separate deletion anchor: that scheme pointed at whichever line happened to survive next to the
  deletion.
- **`files["<path>"].status == "deleted"`** — the file is gone at the head. Its snapshot was taken
  from the base commit, so an agent can still see what was removed, and a finding on it carries no
  `line` and posts as a file-level comment (Step 5).
- **`agents`** — the spawn list for Step 3, already scoped. `toolkit` carries only ToolKit-owned
  files and is absent entirely when there are none; only `security` carries `manifest_files`.
- **`skipped_files`** — files in the diff that no rule covers. Say so in the summary rather than
  implying they were reviewed.

`Cargo.lock` is deliberately never snapshotted: it is generated, routinely over 300 KB, and
everything `RUST-DEP-001` needs from it is already visible in `diff.patch`. It stays in
`manifest_files` so a finding can still anchor on a changed line.

## Step 3: Run review agents in parallel

Spawn one agent per rule module plus one architecture agent, all as parallel background processes.
Each reads its inputs from `/tmp/toolkit-pr-review-${PR_NUMBER}/` and writes a JSON array to its own
output file.

Every subject agent reads **one** module and **all** the files. Do not hand an agent the other five
modules, and use `agent-rules/` rather than the authored `rules/`: the generated copy drops the
rationale an agent does not act on and the criteria the pinned toolchain cannot trigger.

The per-file scoping lives inside the rule modules: the `toolkit` agent runs only on
`toolkit_files`, and `RUST-DEP-001` only on `manifest_files`, which the `security` agent owns.

// turbo
```bash
PR_NUMBER=<PR_NUMBER>
D="/tmp/toolkit-pr-review-${PR_NUMBER}"
AGENTS_DIR="docs/toolkit-pr-review/agents"
RULES_DIR="docs/toolkit-pr-review/agent-rules"
strip() { sed '/^---$/,/^---$/d;1{/^---/d}' "$1"; }

SUBJECT_PROMPT="$(strip ${AGENTS_DIR}/subject.md)"

for m in $(jq -r '.agents[] | select(.name != "architecture") | .name' "${D}/context.json"); do
  FILES=$(jq -r --arg m "$m" '.agents[] | select(.name == $m) | .files[] | "  " + .' "${D}/context.json")
  claude -p "${SUBJECT_PROMPT}

You are the ${m} review agent for PR ${PR_NUMBER}.
Read ${D}/context.json first.

Your rule module, the only one you read:
  ${RULES_DIR}/${m}.md

The files you review:
${FILES}

You hold one module, so go deep: a second and third finding in the same file is expected." \
    > "${D}/out-${m}.json" 2>&1 &
done

claude -p "$(strip ${AGENTS_DIR}/architecture.md)

You are the PR-level architecture pass for PR ${PR_NUMBER}.
Read ${D}/context.json first. Work from ${D}/diff.patch; do not read whole files from files/.
Emit only RUST-ARCH-001." > "${D}/out-architecture.json" 2>&1 &

wait
echo "All agents finished."
```

**Fallback (no `claude` CLI)**: If `claude` CLI is not available, read
`docs/toolkit-pr-review/agents/subject.md` and work through the modules yourself, **one module at a
time over all the files** — not one file at a time over all the modules. Holding a single module and
sweeping the whole PR with it is the arrangement being reproduced here; doing it the other way round
is the design this replaced. Then do the architecture pass from
`docs/toolkit-pr-review/agents/architecture.md` over the whole diff.

## Step 4: Collect and merge findings

```bash
export PR_NUMBER=<PR_NUMBER>
python3 - << 'PY'
import json, glob, sys, os, subprocess

PR_NUMBER = os.environ["PR_NUMBER"]
ctx = json.load(open(f"/tmp/toolkit-pr-review-{PR_NUMBER}/context.json"))

# Subject agents in module order, then the architecture pass. `allowed` is the file set the
# agent was given; only `toolkit` is narrower than the whole PR, and only it can file a
# finding outside its scope.
sources = [(a["name"], set(a["files"])) for a in ctx["agents"] if a["name"] != "architecture"]
sources.append(("architecture", None))

combined, out_of_scope = [], 0
for name, allowed in sources:
    path = f"/tmp/toolkit-pr-review-{PR_NUMBER}/out-{name}.json"
    try:
        text = open(path).read()
        start, end = text.index("["), text.rindex("]") + 1
        found = json.loads(text[start:end])
    except Exception as e:
        print(f"WARNING: {name}: {e}", file=sys.stderr)
        continue
    for f in found:
        # An agent that reviews outside the file list it was given ignored its scope.
        if allowed is not None and f.get("file") not in allowed:
            out_of_scope += 1
            continue
        combined.append(f)
if out_of_scope:
    print(f"WARNING: dropped {out_of_scope} findings filed outside the agent's file list",
          file=sys.stderr)

# Per-file line ranges and status, for validating a finding against the side it claims
files_meta = ctx["files"]
deleted_files = {p for p, r in files_meta.items() if r["status"] == "deleted"}

def in_range(file, line, side):
    """A finding is valid on the side it claims, and only there.

    RIGHT lines are added lines in the head file; LEFT lines are removed lines in the
    base file. The two numbering schemes are unrelated, so a LEFT line validated against
    the RIGHT ranges (or posted without side="LEFT") lands on an unrelated line that
    GitHub accepts without complaint.
    """
    if line is None:
        return False
    rec = files_meta.get(file)
    if rec is None:
        return False
    key = "left" if side == "LEFT" else "right"
    return any(start <= line <= end for start, end in rec["ranges"][key])

# Drop findings whose `issue` is speculative rather than evidenced: a hypothetical
# problem, not an observed one. Judged on `issue` only -- NOT on `comment`, where
# hedged phrasing is intentional when the finding itself is uncertain (see
# docs/toolkit-pr-review/comment-style.md).
SPECULATIVE = ("might", "could consider", "may want to", "it would be nice")

def speculative(f):
    issue = (f.get("issue") or "").lower()
    return any(t in issue for t in SPECULATIVE)

# Deduplicate by (file, line, id), validate line in diff.
# A finding on a fully deleted file has no line at all (it posts as a
# file-level comment in Step 5) and skips the line check entirely.
seen, filtered, malformed = set(), [], 0
for f in combined:
    # An agent writes this JSON, so a record can arrive without the fields the key is
    # built from. Indexing straight into it raised KeyError and killed the whole merge,
    # losing every finding after the bad one; drop the record and keep going instead.
    if not isinstance(f, dict) or not f.get("file") or not f.get("id"):
        malformed += 1
        continue
    key = (f["file"], f.get("line"), f["id"])
    if key in seen:
        continue
    # No line at all is legal in two cases: a wholly deleted file, and a PR-level
    # RUST-ARCH-001 finding that no single line represents. Both post file-level.
    lineless_ok = f["file"] in deleted_files or (
        f.get("line") is None and f.get("id") == "RUST-ARCH-001"
    )
    if not lineless_ok and not in_range(f["file"], f.get("line"), f.get("side")):
        continue
    if speculative(f):
        continue
    seen.add(key)
    filtered.append(f)
if malformed:
    print(f"WARNING: dropped {malformed} findings missing a file or id", file=sys.stderr)

RANK = {"CRITICAL": 0, "HIGH": 1, "MEDIUM": 2, "LOW": 3}

# Second pass: drop what the PR already carries a comment about. A PR under review has
# usually been reviewed before -- by a human, by another bot, or by an earlier run of
# this tool -- and reposting buries whatever is new. Only top-level comments count:
# thread replies and RESOLVED follow-ups are not findings.
#
# This runs BEFORE the same-line collapse, and the order is load-bearing. On PR 4785 one
# collapse group held two duplicates of existing comments and one novel finding; with the
# collapse first a duplicate won the severity tie on agent order and the only finding
# worth posting was dropped.
#
# Line numbers are a first cut, not the last word: on PR 4747 the existing review was
# written against an earlier commit, so 2 of 20 findings matched positionally while 10
# were the same defect at a shifted line. Anything this leaves behind is caught by the
# topic read in the checklist below.
existing = []
try:
    raw = subprocess.run(
        ["gh", "api", "--paginate",
         f"repos/{ctx['repo']}/pulls/{PR_NUMBER}/comments?per_page=100",
         "--jq", '.[] | select(.in_reply_to_id==null) | {path, line, head: (.body | split("\n")[0])}'],
        capture_output=True, text=True, check=False)
    existing = [json.loads(l) for l in raw.stdout.splitlines() if l.strip()]
except Exception as e:
    print(f"WARNING: could not read existing PR comments: {e}", file=sys.stderr)

already = {(c.get("path"), c.get("line")) for c in existing}
before = len(filtered)
filtered = [f for f in filtered if (f["file"], f.get("line")) not in already]
if existing:
    print(f"{len(existing)} comments already on the PR; dropped {before - len(filtered)} "
          f"findings on those exact lines.", file=sys.stderr)
    print("REVIEW BY TOPIC before posting: a finding whose defect an existing comment "
          "already names must be dropped even when the line differs.", file=sys.stderr)
    for c in existing:
        print(f"  existing: {c.get('path')}:{c.get('line')} {c.get('head','')[:100]}",
              file=sys.stderr)

# Third pass: two findings on one line. Collapsing every collision is wrong -- across PRs
# 4785, 4747 and 4711 (80 findings) 22 landed on an occupied line and 10 of those were a
# different defect than the survivor, so a flat collapse discarded 12.5% of all findings.
# Group on `issue` as well, so equivalent findings still collapse to the highest severity
# while distinct defects on one line both survive. GitHub accepts two comments on a line
# and human reviewers here post them. Lineless findings are left alone.
#
# Known gap: the key is exact, so one defect reported a line apart by two agents survives
# as two findings and has to be folded by hand.
def issue_key(f):
    return " ".join((f.get("issue") or "").lower().split())

best = {}
collapsed = []
for f in filtered:
    if f.get("line") is None:
        collapsed.append(f)
        continue
    k = (f["file"], f["line"], issue_key(f))
    if k not in best or RANK.get(f["severity"], 4) < RANK.get(best[k]["severity"], 4):
        best[k] = f
collapsed.extend(best.values())
filtered = collapsed

# Two distinct defects on one line both post; beyond that the line is telling you the
# change is doing too much, which is RUST-ARCH-001 territory rather than four comments.
per_line = {}
for f in filtered:
    per_line.setdefault((f["file"], f.get("line")), []).append(f)

capped = []
for (path, line), group in per_line.items():
    if line is None or len(group) <= 2:
        capped.extend(group)
        continue
    group.sort(key=lambda x: RANK.get(x["severity"], 4))
    print(f"WARNING: {len(group)} findings on {path}:{line}; keeping the 2 most severe",
          file=sys.stderr)
    capped.extend(group[:2])
filtered = capped

# Sort CRITICAL > HIGH > MEDIUM > LOW
order_map = {"CRITICAL": 0, "HIGH": 1, "MEDIUM": 2, "LOW": 3}
filtered.sort(key=lambda x: order_map.get(x["severity"], 4))

# Drop LOW only. CRITICAL, HIGH and MEDIUM all post, however many there are.
#
# This replaces a flat cap of 30. Measured on PR 4705: the review produced 76 findings, the
# cap posted 30 and cut 46, and since severity is the sort key everything posted was
# CRITICAL or HIGH while 30 HIGH and 15 MEDIUM fell off the end. Two of the cut findings
# were defects the previous version of this tool had posted and a human had acted on.
# Severity is also a poor sort key today (24 of those 76 were marked CRITICAL, which is not
# credible for one PR), so cutting by rank cuts close to arbitrarily. LOW is the only line
# that means "explicitly not worth a reviewer's attention".
low = [f for f in filtered if f["severity"] == "LOW"]
filtered = [f for f in filtered if f["severity"] != "LOW"]
if low:
    print(f"Dropped {len(low)} LOW findings; posting {len(filtered)}.", file=sys.stderr)

json.dump(filtered, open(f"/tmp/toolkit-pr-review-{PR_NUMBER}/findings.json", "w"), indent=2)
print(f"Total findings: {len(filtered)}")
PY
```

## Step 5: Post inline review comments

```bash
export PR_NUMBER=<PR_NUMBER>
D="/tmp/toolkit-pr-review-${PR_NUMBER}"
export HEAD_SHA=$(jq -r .headRefOid ${D}/meta.json)

python3 - << 'PY'
import json, os

PR_NUMBER = os.environ["PR_NUMBER"]
HEAD_SHA = os.environ["HEAD_SHA"]
# Every artefact of this review lives under D. Never write a payload to a bare /tmp
# filename: it is shared with every concurrent review on the machine, including reviews
# of other PRs, so one run can overwrite another's payload and post it to the wrong PR.
D = f"/tmp/toolkit-pr-review-{PR_NUMBER}"
findings = json.load(open(f"{D}/findings.json"))
ctx = json.load(open(f"{D}/context.json"))
deleted_files = set(ctx.get("deleted_files", []))

# A finding with no line can't go in the batch review's `comments` array (which
# requires line+side), so it posts individually via the single-comment endpoint with
# subject_type="file". Two cases produce that: a wholly deleted file, which has no
# line on either side, and a RUST-ARCH-001 finding that no single line represents.
line_findings = [f for f in findings if f.get("line") is not None and f["file"] not in deleted_files]
file_findings = [f for f in findings if f.get("line") is None or f["file"] in deleted_files]

# The comment body comes from the `comment` field, which the sub-agents write in the
# voice defined by docs/toolkit-pr-review/comment-style.md. `issue` and `fix` are for the
# Step 6 summary table only and are never posted. The severity header is added for
# CRITICAL and HIGH only: a bold tag on every comment is the clearest sign a machine
# wrote it, and severity still shows in the summary table.
def body_of(f):
    text = f.get("comment") or f["issue"]
    if f["severity"] in ("CRITICAL", "HIGH"):
        return f"**{f['severity']}**\n\n{text}"
    return text

# `side` comes from the finding, never from a default: a LEFT finding carries a base-file
# line number, so posting it as RIGHT points the comment at an unrelated line in the head
# file, which GitHub accepts silently.
comments = [
    {
        "path": f["file"],
        "line": f["line"],
        "side": f.get("side", "RIGHT"),
        "body": body_of(f)
    }
    for f in line_findings
]
payload = {
    "commit_id": HEAD_SHA,
    "event": "COMMENT",
    "body": "",
    "comments": comments
}
json.dump(payload, open(f"{D}/review-payload.json", "w"))
json.dump(file_findings, open(f"{D}/file-level-findings.json", "w"))
print(f"Prepared {len(comments)} line comments, {len(file_findings)} file-level comments")
PY

LINE_COUNT=$(jq '.comments | length' ${D}/review-payload.json)
FILE_COUNT=$(jq 'length' ${D}/file-level-findings.json)

if [ "$LINE_COUNT" -eq 0 ] && [ "$FILE_COUNT" -eq 0 ]; then
  gh api repos/${REPO}/pulls/${PR_NUMBER}/reviews \
    --method POST \
    -f commit_id="${HEAD_SHA}" \
    -f event="COMMENT" \
    -f body="No issues found."
else
  if [ "$LINE_COUNT" -gt 0 ]; then
    gh api repos/${REPO}/pulls/${PR_NUMBER}/reviews \
      --method POST \
      --input ${D}/review-payload.json
  fi
  # File-level comments: a deleted file has no line on either side to anchor on.
  jq -c '.[]' ${D}/file-level-findings.json | while read -r finding; do
    path=$(echo "$finding" | jq -r '.file')
    # `//` alone would keep an empty-string comment (jq treats "" as truthy), which
    # would post a blank comment. Fall back to .issue when it is empty or null, so this
    # matches body_of() above, where Python's `or` already treats "" as absent.
    body=$(echo "$finding" | jq -r '(if ((.comment // "") == "") then .issue else .comment end) as $t
      | if (.severity == "CRITICAL" or .severity == "HIGH")
        then "**" + .severity + "**\n\n" + $t
        else $t end')
    gh api repos/${REPO}/pulls/${PR_NUMBER}/comments \
      --method POST \
      -f commit_id="${HEAD_SHA}" \
      -f path="${path}" \
      -f subject_type="file" \
      -f body="${body}"
  done
fi
```

## Step 6: Print summary table

```bash
export PR_NUMBER=<PR_NUMBER>
python3 - << 'PY'
import json, os
PR_NUMBER = os.environ["PR_NUMBER"]
findings = json.load(open(f"/tmp/toolkit-pr-review-{PR_NUMBER}/findings.json"))
print(f"\n## Rust PR Review: #{PR_NUMBER}\n")
print("| # | ID | Sev | Location | Issue | Fix |")
print("|---|----|-----|----------|-------|-----|")
for i, f in enumerate(findings, 1):
    name = f['file'].split('/')[-1]
    loc = f"{name}:{f['line']}" if f.get('line') else name
    print(f"| {i} | {f['id']} | {f['severity']} | {loc} | {f['issue'][:60]} | {f['fix'][:60]} |")
print(f"\nPosted {len(findings)} inline comments on PR #{PR_NUMBER}.")
PY
```
