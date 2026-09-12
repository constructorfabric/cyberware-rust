#!/usr/bin/env python3
"""Consistency checks for the toolkit-pr-review rule set.

The rules live in the modules under docs/toolkit-pr-review/rules/, one file per subject, plus
RUST-ARCH-001 in the architecture agent prompt. Nothing else defines them, so the
failure mode is not drift between two copies (there is only one) but drift between a
rule and the things that must agree with it: the orchestrator's routing table, the
workspace lint config, and the pinned toolchain.

Review work is split by subject, not by file: each module is read by exactly one agent,
and that agent reads no other module. So a module owning a rule is also the agent that
applies it, and a module no stub points at can never fire.

Usage:
    python3 tools/scripts/toolkit-pr-review/lint.py                # check, exit 1 on failure
    python3 tools/scripts/toolkit-pr-review/lint.py --write-index  # also regenerate RULES.md

Also runs in CI (.github/workflows/ci.yml) and as `make pr-review-lint`.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
RULES_DIR = ROOT / "docs/toolkit-pr-review/rules"
AGENT_DIR = ROOT / "docs/toolkit-pr-review/agents"
RENDERED_DIR = ROOT / "docs/toolkit-pr-review/agent-rules"
SUBJECT_AGENT = AGENT_DIR / "subject.md"
STUB_DIR = ROOT / ".claude/agents"
ARCH_AGENT = AGENT_DIR / "architecture.md"
SKILL = ROOT / ".claude/skills/toolkit-pr-review/SKILL.md"
DEVIN = ROOT / ".devin/workflows/toolkit-pr-review.md"
CONVENTIONS = ROOT / "docs/toolkit-pr-review/review-conventions.md"
STYLE = ROOT / "docs/toolkit-pr-review/comment-style.md"
CARGO = ROOT / "Cargo.toml"
TOOLCHAIN = ROOT / "rust-toolchain.toml"
INDEX = ROOT / "docs/toolkit-pr-review/RULES.md"

RULE_ID = r"(?:RUST|TOOLKIT)-[A-Z]+-\d{3}|TEST-QUALITY-\d+"
SEVERITIES = ("CRITICAL", "HIGH", "MEDIUM", "LOW")

failures: list[str] = []
notes: list[str] = []


def fail(check: str, msg: str) -> None:
    failures.append(f"[{check}] {msg}")


def note(msg: str) -> None:
    notes.append(f"  note: {msg}")


def rule_files() -> list[Path]:
    """Files that define rules: the six subject modules plus the architecture agent."""
    return sorted(RULES_DIR.glob("*.md")) + ([ARCH_AGENT] if ARCH_AGENT.exists() else [])


def module_name(p: Path) -> str:
    return p.stem


def parse_rules() -> dict[str, dict]:
    """rule id -> {module, severity, title, line}. Also records severity-less headings."""
    rules: dict[str, dict] = {}
    for path in rule_files():
        who = module_name(path)
        lines = path.read_text(encoding="utf-8").splitlines()
        for i, line in enumerate(lines):
            m = re.match(rf"^###\s+(?:\d+\.\s+)?({RULE_ID})\s*(?:[—-]\s*(.*))?$", line)
            if not m:
                continue
            rid, title = m.group(1), (m.group(2) or "").strip()
            sev = None
            for nxt in lines[i + 1 : i + 4]:
                sm = re.match(r"^\*\*Severity\*\*:\s*(\w+)", nxt)
                if sm:
                    sev = sm.group(1).upper()
                    break
            if rid in rules:
                fail("duplicate-rule",
                     f"{rid} defined in both {rules[rid]['module']} and {who}; a rule must be written down once")
                continue
            rules[rid] = {"module": who, "severity": sev, "title": title, "line": i + 1, "path": path}
    return rules


def check_severity(rules: dict[str, dict]) -> None:
    for rid, r in sorted(rules.items()):
        if r["severity"] is None:
            fail("severity", f"{rid} ({r['module']}) has no `**Severity**:` line")
        # RUST-ARCH-001 is the one rule whose severity is genuinely per-finding: the same
        # structural defect can be CRITICAL or MEDIUM depending on what it reaches. Its line
        # reads "judge per finding, usually HIGH", so accept that form for it alone.
        elif r["severity"] == "JUDGE" and rid == "RUST-ARCH-001":
            pass
        elif r["severity"] not in SEVERITIES:
            fail("severity", f"{rid} ({r['module']}) has severity {r['severity']!r}, expected one of {'/'.join(SEVERITIES)}")


ROUTING_OPEN = "<!-- pr-review:routing-table -->"
ROUTING_CLOSE = "<!-- /pr-review:routing-table -->"


def check_routing(rules: dict[str, dict]) -> None:
    """Every rule must appear in SKILL.md's routing table, and vice versa.

    The table is delimited by explicit marker comments rather than by a step heading.
    Heading-anchored matching failed open: renaming or renumbering a step silently
    disabled this check instead of breaking it, and the check ran outside CI, so
    nothing would have noticed.
    """
    if not SKILL.exists():
        fail("routing", f"{SKILL} not found")
        return
    text = SKILL.read_text(encoding="utf-8")
    start = text.find(ROUTING_OPEN)
    end = text.find(ROUTING_CLOSE)
    if start == -1 or end == -1 or end < start:
        fail("routing",
             f"could not locate the routing table in SKILL.md: expected {ROUTING_OPEN} ... "
             f"{ROUTING_CLOSE}. Without those markers every rule below is unchecked.")
        return
    block = text[start + len(ROUTING_OPEN):end]
    listed = set(re.findall(RULE_ID, block))

    # Ranges like "TOOLKIT-CORE-001..003" and "TEST-QUALITY-1 through TEST-QUALITY-10"
    for fam, lo, hi in re.findall(r"((?:RUST|TOOLKIT)-[A-Z]+)-(\d{3})\.\.(\d{3})", block):
        listed.update(f"{fam}-{n:03d}" for n in range(int(lo), int(hi) + 1))
    for lo, hi in re.findall(r"TEST-QUALITY-(\d+)\s+through\s+TEST-QUALITY-(\d+)", block):
        listed.update(f"TEST-QUALITY-{n}" for n in range(int(lo), int(hi) + 1))
    for lo, hi in re.findall(r"TEST-QUALITY-(\d+)\.\.(\d+)", block):
        listed.update(f"TEST-QUALITY-{n}" for n in range(int(lo), int(hi) + 1))

    defined = set(rules)
    for rid in sorted(defined - listed):
        fail("routing", f"{rid} is defined in module '{rules[rid]['module']}' but not listed in SKILL.md's routing table")
    for rid in sorted(listed - defined):
        fail("routing", f"{rid} is routed in SKILL.md but no rule module defines it")


# Lints reached through a denied group rather than named in Cargo.toml. Verified by
# compiling a triggering snippet under `#![deny(clippy::pedantic)]` with clippy-driver
# and reading the "implied by" note. Extend this only with the same evidence.
GROUP_MEMBERS = {
    "cast_lossless": "pedantic",
    "ptr_as_ptr": "pedantic",
    "fn_params_excessive_bools": "pedantic",
    "must_use_candidate": "pedantic",
}


def denied_lints() -> tuple[set[str], set[str]]:
    """(individually denied lint names, denied group names) from Cargo.toml [workspace.lints.*]."""
    if not CARGO.exists():
        return set(), set()
    text = CARGO.read_text(encoding="utf-8")
    lints, groups = set(), set()
    for block in re.findall(r"^\[workspace\.lints\.[a-z]+\]\n(.*?)(?=^\[|\Z)", text, re.S | re.M):
        for line in block.splitlines():
            m = re.match(r'^\s*([a-z_]+)\s*=\s*(.+)$', line)
            if not m:
                continue
            name, val = m.group(1), m.group(2)
            if '"deny"' not in val and '"forbid"' not in val and "level = \"deny\"" not in val:
                continue
            (groups if name in {"all", "pedantic", "nursery", "cargo", "complexity",
                                "correctness", "perf", "style", "suspicious"} else lints).add(name)
    return lints, groups


def check_enforcement(rules: dict[str, dict]) -> None:
    """Every `Enforcement: clippy <lint> (deny)` must name a lint the build actually denies."""
    lints, groups = denied_lints()
    if not lints and not groups:
        fail("enforcement", "could not read any denied lints from Cargo.toml [workspace.lints]")
        return
    seen = 0
    for path in rule_files():
        who = module_name(path)
        for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for m in re.finditer(r"`?Enforcement: (?:clippy|rustc) ([^`(]+?)\s*\((deny|forbid)\)", line):
                named = [x.strip().strip('`') for x in re.split(r"[,/]| and ", m.group(1)) if x.strip()]
                for lint in named:
                    seen += 1
                    if lint in lints or lint in groups:
                        continue
                    grp = GROUP_MEMBERS.get(lint)
                    if grp and grp in groups:
                        continue  # reached through a denied group; membership verified
                    if grp:
                        fail("enforcement",
                             f"{who}:{n} claims `{lint}` is denied via the `{grp}` group, "
                             f"but that group is not denied in Cargo.toml")
                    else:
                        # Unverified is a failure, not a note. A denied group being present is not
                        # evidence that this particular lint is in it, and a note nobody acts on
                        # lets a marker drift away from the build silently.
                        extra = (f" A denied group is present ({sorted(groups)}): if the lint really "
                                 f"comes from it, verify with clippy-driver and add it to GROUP_MEMBERS."
                                 if groups else "")
                        fail("enforcement",
                             f"{who}:{n} claims `{lint}` is denied, but Cargo.toml does not deny it "
                             f"and it is not a verified member of a denied group.{extra}")
    if seen == 0:
        fail("enforcement", "no Enforcement markers found in any rule module; expected several")


def check_versions() -> None:
    """Version markers must parse, and dead ones (above the pin) are reported."""
    pin = None
    if TOOLCHAIN.exists():
        m = re.search(r'channel\s*=\s*"([0-9.]+)"', TOOLCHAIN.read_text(encoding="utf-8"))
        if m:
            pin = tuple(int(x) for x in m.group(1).split("."))
    if pin is None:
        fail("versions", "could not read the toolchain pin from rust-toolchain.toml")
        return
    dead = 0
    for path in rule_files():
        who = module_name(path)
        for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for kind, ver in re.findall(r"(?:Requires|needs)\s+(Rust|Clippy)\s*>=\s*([0-9.]+)", line):
                parts = tuple(int(x) for x in ver.split("."))
                if kind == "Rust" and parts > pin[: len(parts)]:
                    dead += 1
    if dead:
        note(f"{dead} criteria are gated above the pinned toolchain "
             f"{'.'.join(map(str, pin))} and cannot fire today")


def check_agents_and_modules(rules: dict[str, dict]) -> None:
    """The two agent prompts carry the contract and the walk; the modules carry scope."""
    for path in (SUBJECT_AGENT, ARCH_AGENT):
        if not path.exists():
            fail("agents", f"{path.relative_to(ROOT)} is missing")
            continue
        who = path.stem
        text = path.read_text(encoding="utf-8")
        for field in ("comment", "issue", "fix"):
            if f'`"{field}"`' not in text:
                fail("contract", f"agent {who} does not document the `{field}` field")
        for shared in ("review-conventions.md", "comment-style.md"):
            if shared not in text:
                fail("shared", f"agent {who} does not point at {shared}")

    if SUBJECT_AGENT.exists():
        text = SUBJECT_AGENT.read_text(encoding="utf-8")
        if "one at a time" not in text:
            fail("walk", "the subject agent is missing the per-file walk instruction")

    # Every module must be claimed by exactly one registration stub, or its rules never
    # run. Under the file-sharding design this check asked whether one agent named all six
    # modules; now each agent names one, so the question is whether every module has an
    # agent. A module nobody points at is silently dead, which is how a whole subject
    # stops being reviewed without anything failing.
    for mod in sorted(RULES_DIR.glob("*.md")):
        stub = STUB_DIR / f"toolkit-pr-review-{mod.stem}.md"
        if not stub.exists():
            fail("modules", f"no agent stub {stub.relative_to(ROOT)} claims rules/{mod.name}, "
                            f"so the rules in it can never fire")
        elif f"docs/toolkit-pr-review/agent-rules/{mod.name}" not in stub.read_text(encoding="utf-8"):
            fail("modules", f"{stub.relative_to(ROOT)} does not name agent-rules/{mod.name}")
        rendered = RENDERED_DIR / mod.name
        if not rendered.exists():
            fail("modules", f"agent-rules/{mod.name} is missing; run `review.py render-rules`")

    for mod in sorted(RULES_DIR.glob("*.md")):
        text = mod.read_text(encoding="utf-8")
        if "## Scope of this module" not in text:
            fail("modules", f"rules/{mod.name} has no `## Scope of this module` section, "
                            f"so its agent cannot tell which files the rules apply to")

    # RUST-ARCH-001 is the architecture agent's alone; nothing else may claim it.
    arch = [rid for rid, r in rules.items() if r["module"] == "architecture"]
    if arch != ["RUST-ARCH-001"]:
        fail("agents", f"the architecture agent must define exactly RUST-ARCH-001, found {arch or 'nothing'}")


# The corpus must never lose a criterion. v1 carried 313; the current modules carry this
# many. Compression moves rationale off the trigger line, it does not delete criteria, so
# this number may rise and must never fall. Lower it only alongside a stated reason.
CRITERIA_FLOOR = 312

SEVERITY_MARKER = re.compile(r"^\[(CRITICAL|HIGH|MEDIUM|LOW)\]\s+(\S.*)$")


def criteria_of(text: str) -> list[tuple[int, str]]:
    """(line number, text) for every criterion bullet under Check IDs.

    Nested bullets count too: the four SSRF sub-items under RUST-SEC-001 are criteria in
    their own right, each naming a distinct check. `why:` continuation lines never start
    with `-`, so rationale is excluded by construction.
    """
    if "## Check IDs to Apply" not in text:
        return []
    head = text.index("## Check IDs to Apply")
    out = []
    for i, line in enumerate(text[head:].splitlines(), text[:head].count("\n") + 1):
        m = re.match(r"^\s*-\s+(\S.*)$", line)
        if m:
            out.append((i, m.group(1)))
    return out


def check_criteria(rules: dict[str, dict]) -> None:
    """Criteria are never lost, every trigger says something, and `why:` stays rationale."""
    total = 0
    for path in sorted(RULES_DIR.glob("*.md")):
        text = path.read_text(encoding="utf-8")
        who = path.stem
        crit = criteria_of(text)
        total += len(crit)
        for n, body in crit:
            m = SEVERITY_MARKER.match(body)
            trigger = m.group(2) if m else body
            if not trigger.strip(" .-*`"):
                fail("criteria", f"{who}:{n} criterion has no trigger text after its markers")
            # A bracketed word where the override goes but not a valid level: the criterion
            # silently keeps its rule's severity and the marker reads as trigger prose, so the
            # author's intent is lost without anything failing.
            # Two letters minimum: `- [ ]` and `- [x]` are markdown checkboxes, not a botched
            # level, and flagging those would be a false positive in the linter itself.
            bad = re.match(r"^\[([A-Za-z]{2,})\]\s", body)
            if bad and not m:
                fail("criteria",
                     f"{who}:{n} criterion starts with `[{bad.group(1)}]`, which is not a severity "
                     f"level. Use one of {'/'.join(SEVERITIES)} or drop the brackets.")
        for i, line in enumerate(text.splitlines(), 1):
            stripped = line.strip()
            if not stripped.startswith("why:"):
                continue
            if not line.startswith("  "):
                fail("criteria", f"{who}:{i} `why:` must be an indented continuation of a criterion")
            if re.match(r"^\s*why:\s*-\s", line):
                fail("criteria", f"{who}:{i} `why:` line starts a bullet; a criterion cannot hide in rationale")

    report_severity_spread()

    if total < CRITERIA_FLOOR:
        fail("criteria",
             f"{total} criteria across the rule modules, below the floor of {CRITERIA_FLOOR}. "
             f"Compression moves rationale off the trigger line; it must not delete criteria. "
             f"If a criterion was deliberately merged or dropped, lower CRITERIA_FLOOR in this "
             f"file in the same commit and say why.")
    else:
        note(f"{total} criteria (floor {CRITERIA_FLOOR})")


def report_severity_spread() -> None:
    """How many criteria carry each severity, once inheritance is resolved.

    Severity is declared per rule and every criterion under it inherits, so one rule's label
    decides how a whole family of findings is ranked: 22 criteria inherit CRITICAL from
    RUST-SEC-001 alone, which covers both a token written to a log and a config field with no
    length cap. A criterion-level `[LEVEL]` marker overrides it. Printing the spread every run
    is what makes a lopsided corpus visible without anyone going looking.
    """
    spread: dict[str, int] = dict.fromkeys(SEVERITIES, 0)
    overrides = 0
    for path in sorted(RULES_DIR.glob("*.md")):
        text = path.read_text(encoding="utf-8")
        if "## Check IDs to Apply" not in text:
            continue
        head = text.index("## Check IDs to Apply")
        current = None
        for line in text[head:].splitlines():
            m = re.match(rf"^###\s+(?:\d+\.\s+)?({RULE_ID})", line)
            if m:
                current = None
                continue
            m = re.match(r"^\*\*Severity\*\*:\s*(\w+)", line)
            if m:
                current = m.group(1)
                continue
            m = re.match(r"^\s*-\s+(\S.*)$", line)
            if not m:
                continue
            mark = SEVERITY_MARKER.match(m.group(1))
            if mark:
                overrides += 1
                spread[mark.group(1)] = spread.get(mark.group(1), 0) + 1
            elif current in spread:
                spread[current] += 1
    shown = ", ".join(f"{s.lower()}={spread[s]}" for s in SEVERITIES)
    note(f"criterion severity: {shown} ({overrides} overridden at criterion level)")


# Markers of a harness that has gone back to doing prepare's work in prose. Each one names
# a scheme `prepare` deliberately replaced, so its presence is a re-drift, not a style choice.
REDRIFT = {
    "deletion_anchors": "prepare emits ranges.left instead; anchor removed code with side=LEFT",
    "changed_ranges": "prepare emits files[path].ranges.right",
    "__service.rs": "snapshot paths mirror the repo tree; `/`->`__` escaping is not injective",
}


def check_harnesses_call_prepare() -> None:
    """Both harnesses must invoke `prepare`, not re-implement it.

    This is the check that would have caught the state this replaced: review.py existed,
    was tested and ran in CI, and neither harness called it. The prose copies had already
    drifted on five behaviours, two of them defects that silently posted comments on the
    wrong lines. A script nothing calls is not a safeguard.
    """
    for f in (SKILL, DEVIN):
        if not f.exists():
            fail("harness", f"{f.relative_to(ROOT)} is missing")
            continue
        if "review.py prepare" not in f.read_text(encoding="utf-8"):
            fail("harness",
                 f"{f.relative_to(ROOT)} never invokes `review.py prepare`, so it is doing the "
                 f"diff parsing, classification and snapshotting by hand again")

    # The agent prompts and rule modules name context.json fields too, and an agent told to
    # read a field prepare does not emit simply finds nothing and reports nothing.
    docs = [SKILL, DEVIN, SUBJECT_AGENT, ARCH_AGENT]
    docs += sorted(RULES_DIR.glob("*.md")) + sorted(RENDERED_DIR.glob("*.md"))
    for f in docs:
        if not f.exists():
            continue
        text = f.read_text(encoding="utf-8")
        for marker, why in REDRIFT.items():
            if marker in text:
                fail("harness", f"{f.relative_to(ROOT)} still refers to `{marker}`: {why}")


def check_shared_files() -> None:
    for f in (CONVENTIONS, STYLE):
        if not f.exists():
            fail("shared", f"{f.relative_to(ROOT)} is missing")
    if DEVIN.exists():
        t = DEVIN.read_text(encoding="utf-8")
        if "review-conventions.md" not in t:
            fail("harness", "the Devin workflow does not mention review-conventions.md")


def write_index(rules: dict[str, dict]) -> None:
    order = {s: i for i, s in enumerate(SEVERITIES)}
    rows = sorted(rules.items(), key=lambda kv: (order.get(kv[1]["severity"], 9), kv[0]))
    out = [
        "# Rule index",
        "",
        "**Generated by `tools/scripts/toolkit-pr-review/lint.py --write-index`. Do not edit by hand.**",
        "",
        "Not authoritative: each rule is defined in its owning module under",
        "`docs/toolkit-pr-review/rules/`, and that definition is the one the review applies.",
        "Each module is read by exactly one agent, so the module says both where a rule is",
        "written down and who applies it.",
        "",
        "`RUST-ARCH-001` is the one exception: there is no `rules/architecture.md`. It is",
        "defined in `docs/toolkit-pr-review/agents/architecture.md`, the architecture agent's",
        "own guidance, because the single agent that applies it is also the only reader of it.",
        "",
        f"{len(rules)} rules across {len({r['module'] for r in rules.values()})} modules.",
        "",
        "| Rule | Severity | Module | Title |",
        "|---|---|---|---|",
    ]
    for rid, r in rows:
        out.append(f"| `{rid}` | {r['severity'] or '—'} | {r['module']} | {r['title'] or ''} |")
    INDEX.write_text("\n".join(out) + "\n", encoding="utf-8")
    print(f"  wrote {INDEX.relative_to(ROOT)} ({len(rules)} rules)")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--write-index", action="store_true", help="regenerate docs/toolkit-pr-review/RULES.md")
    args = ap.parse_args()

    rules = parse_rules()
    if not rules:
        fail("parse", f"no rules found under {RULES_DIR.relative_to(ROOT)}")

    check_severity(rules)
    check_routing(rules)
    check_enforcement(rules)
    check_versions()
    check_agents_and_modules(rules)
    check_criteria(rules)
    check_harnesses_call_prepare()
    check_shared_files()

    by_module: dict[str, int] = {}
    for r in rules.values():
        by_module[r["module"]] = by_module.get(r["module"], 0) + 1
    print(f"  {len(rules)} rules: " + ", ".join(f"{k}={v}" for k, v in sorted(by_module.items())))

    if args.write_index:
        write_index(rules)

    for n in notes:
        print(n)
    if failures:
        print(f"\nFAILED ({len(failures)}):")
        for f in failures:
            print(f"  {f}")
        return 1
    print("\nOK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
