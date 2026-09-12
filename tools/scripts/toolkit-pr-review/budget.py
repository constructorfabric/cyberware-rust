#!/usr/bin/env python3
"""Cost estimation for toolkit-pr-review.

Review work is split **by subject, not by file**: six agents, one rule module each, and
every one of them reads the whole diff. This module estimates what that costs before the
run starts, so an oversized PR is refused at `prepare` rather than discovered on the bill.

Why this replaces the file-sharding packer that used to live here, measured on the five
PRs the original skill reviewed (4676, 4705, 4738, 4777, 4778):

| design                | agents | rules per agent | findings | reproduced the original |
|-----------------------|--------|-----------------|----------|-------------------------|
| one agent per shard   |   27   |  55-94 KB       |   156    |  44%                    |
| two agents per shard   |   49   |  46 KB          |   241    |  53%                    |
| six subject agents    |   35   |  11-21 KB       |   197    |  58%                    |

Splitting by file forces every agent to carry every rule, because an agent that owns only
some files must still apply all rules to them. Splitting by subject is what keeps a single
agent's rule load small, and that load is the variable that moved the numbers.

Subject agents cost more than file shards (+55% tokens here) because the diff is read six
times instead of once. That is the trade this design makes deliberately: the duplicated
reads are what let one agent see a defect and its consequence in two different files.
"""

from __future__ import annotations

import math
import os
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

# Calibrated against two measured diffs: 718 KB -> ~179k tokens (4.01 B/token) and
# 547 KB -> ~136k (4.02). Source code sits close enough to 4 bytes per token for
# budgeting; this is not a tokenizer and must not be used as one.
BYTES_PER_TOKEN = 4.0

ROOT = Path(__file__).resolve().parents[3]
DOCS = ROOT / "docs/toolkit-pr-review"
RULES_DIR = DOCS / "agent-rules"
AGENT_DIR = DOCS / "agents"

# The six subject agents, in the order `prepare` reports them. Each name is both the
# rule module's filename stem and the agent's identity in the spawn prompt.
MODULES = ["errors", "security", "async", "design", "tests", "toolkit"]

# Read alongside a module by every subject agent.
SHARED_DOCS = ["review-conventions.md", "comment-style.md"]

# The spawn prompt itself: file list, coordinates note, scope rules. Measured at roughly
# 1.5k tokens on a 56-file PR and it grows with the file list, so it is charged per agent.
PROMPT_TOKENS = 2_000

DEFAULT_MAX_TOTAL_TOKENS = 4_000_000


def est_tokens(n_bytes: int) -> int:
    return math.ceil(n_bytes / BYTES_PER_TOKEN)


def _size(p: Path) -> int:
    try:
        return p.stat().st_size
    except OSError:
        return 0


def instruction_tokens(module: str) -> int:
    """What one subject agent reads before it opens a single source file.

    Measured from the files on disk rather than hardcoded, so growing a rule module shows
    up in the estimate instead of silently inflating every run.
    """
    b = _size(RULES_DIR / f"{module}.md") + _size(AGENT_DIR / "subject.md")
    b += sum(_size(DOCS / d) for d in SHARED_DOCS)
    return est_tokens(b) + PROMPT_TOKENS


def architecture_tokens() -> int:
    b = _size(AGENT_DIR / "architecture.md") + sum(_size(DOCS / d) for d in SHARED_DOCS)
    return est_tokens(b) + PROMPT_TOKENS


def agent_costs(corpus_tokens: int, diff_tokens: int,
                per_agent_corpus: dict[str, int] | None = None) -> dict[str, int]:
    """Estimated input tokens per agent.

    Every subject agent reads the whole diff. Five of them also read every changed file;
    `toolkit` reads only the ToolKit-owned subset, so `per_agent_corpus` overrides the
    corpus for any agent whose file list is narrower. The architecture agent reads the
    diff alone, since RUST-ARCH-001 is about the shape of the change rather than the
    contents of any one file.
    """
    over = per_agent_corpus or {}
    out = {m: instruction_tokens(m) + over.get(m, corpus_tokens) + diff_tokens
           for m in MODULES}
    out["architecture"] = architecture_tokens() + diff_tokens
    return out


def total_cost(corpus_tokens: int, diff_tokens: int,
               per_agent_corpus: dict[str, int] | None = None) -> int:
    return sum(agent_costs(corpus_tokens, diff_tokens, per_agent_corpus).values())
