#!/usr/bin/env python3
"""Fixture tests for the toolkit-pr-review scripts.

Every assertion here runs against a saved diff with no network and no review agents. A
full review run costs about 1.1M tokens, so the whole point of these tests is that the
deterministic half is provable for free.

    python3 -m unittest discover tools/scripts/toolkit-pr-review/tests
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent
ROOT = SCRIPTS.parents[2]
FIXTURES = HERE / "fixtures"
sys.path.insert(0, str(SCRIPTS))

import classify      # noqa: E402
import diffparse     # noqa: E402
import budget        # noqa: E402
import lint          # noqa: E402

FIXTURE_PRS = sorted(p.name for p in FIXTURES.iterdir() if p.is_dir()) if FIXTURES.exists() else []


def load(pr: str) -> str:
    return (FIXTURES / pr / "diff.patch").read_text(errors="replace")


class TestDiffParse(unittest.TestCase):
    def test_line_counts_match_the_diff_exactly(self):
        """Parsed added/removed counts must equal a naive scan of the patch."""
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                text = load(pr)
                files = diffparse.parse(text)
                plus = sum(1 for l in text.splitlines()
                           if l.startswith("+") and not l.startswith("+++"))
                minus = sum(1 for l in text.splitlines()
                            if l.startswith("-") and not l.startswith("---"))
                self.assertEqual(sum(f.added_lines for f in files.values()), plus)
                self.assertEqual(sum(f.removed_lines for f in files.values()), minus)

    def test_every_section_becomes_a_file(self):
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                text = load(pr)
                self.assertEqual(len(diffparse.parse(text)),
                                 len(re.findall(r"^diff --git ", text, re.M)))

    def test_ranges_exclude_context_lines(self):
        """A range must contain added lines only.

        Deriving ranges from the hunk header instead spans the surrounding context, and
        a finding anchored on an unchanged line then passes validation and is posted
        against code the PR never touched. Measured at 39% of the window on one PR.
        """
        diff = (
            "diff --git a/a.rs b/a.rs\n"
            "--- a/a.rs\n"
            "+++ b/a.rs\n"
            "@@ -1,4 +1,5 @@\n"
            " keep1\n"
            " keep2\n"
            "+added\n"
            " keep3\n"
            " keep4\n"
        )
        f = diffparse.parse(diff)["a.rs"]
        self.assertEqual(f.right, [(3, 3)])
        self.assertEqual(f.left, [])

    def test_deleted_file_has_left_lines_and_no_right(self):
        diff = (
            "diff --git a/gone.rs b/gone.rs\n"
            "deleted file mode 100644\n"
            "--- a/gone.rs\n"
            "+++ /dev/null\n"
            "@@ -1,2 +0,0 @@\n"
            "-fn a() {}\n"
            "-fn b() {}\n"
        )
        f = diffparse.parse(diff)["gone.rs"]
        self.assertEqual(f.status, "deleted")
        self.assertEqual(f.right, [])
        self.assertEqual(f.left, [(1, 2)])

    def test_added_file_is_marked(self):
        diff = ("diff --git a/new.rs b/new.rs\n"
                "--- /dev/null\n"
                "+++ b/new.rs\n"
                "@@ -0,0 +1,2 @@\n"
                "+one\n"
                "+two\n")
        f = diffparse.parse(diff)["new.rs"]
        self.assertEqual(f.status, "added")
        self.assertEqual(f.right, [(1, 2)])

    def test_slice_covers_requested_files_only(self):
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                files = diffparse.parse(load(pr))
                want = sorted(files)[:3]
                sliced = diffparse.slice_diff(files, want)
                got = re.findall(r"^diff --git a/.+ b/(.+)$", sliced, re.M)
                self.assertEqual(len(got), len(want))


class TestClassify(unittest.TestCase):
    def test_manifest_detection(self):
        self.assertTrue(classify.is_manifest("Cargo.toml"))
        self.assertTrue(classify.is_manifest("gears/x/Cargo.toml"))
        self.assertTrue(classify.is_manifest(".cargo/audit.toml"))
        self.assertTrue(classify.is_manifest("deny.toml"))
        # a config.toml anywhere but .cargo/ belongs to something else
        self.assertFalse(classify.is_manifest("config/app/config.toml"))
        self.assertFalse(classify.is_manifest("k8s/values.yaml"))

    def test_out_of_scope_files(self):
        for p in ("k8s/deploy.yaml", "migrations/001.sql", "proto/api.proto", "Dockerfile"):
            self.assertFalse(classify.in_scope(p), p)

    def test_cargo_lock_is_in_scope_but_never_snapshotted(self):
        self.assertTrue(classify.in_scope("Cargo.lock"))
        self.assertFalse(classify.snapshotable("Cargo.lock"))

    def test_group_key(self):
        self.assertEqual(classify.group_key("gears/mini-chat/src/lib.rs"), "gears/mini-chat")
        self.assertEqual(classify.group_key("deny.toml"), "deny.toml")
        # two segments -> the first one, so .cargo/audit.toml and .cargo/config.toml
        # land in the same group
        self.assertEqual(classify.group_key(".cargo/audit.toml"), ".cargo")

    def test_test_sibling_source(self):
        self.assertEqual(classify.test_sibling_source("a/lock_tests.rs"), "a/lock.rs")
        self.assertIsNone(classify.test_sibling_source("a/lock.rs"))
        self.assertIsNone(classify.test_sibling_source("a/integration_test.rs"))

    def test_toolkit_owned_by_path_and_by_symbol(self):
        self.assertTrue(classify.toolkit_owned("gears/foo/src/gear.rs", ""))
        self.assertTrue(classify.toolkit_owned("other/x.rs", "use toolkit_db::Db;"))
        self.assertTrue(classify.toolkit_owned("other/x.rs", "let c: SecureConn = ...;"))
        self.assertFalse(classify.toolkit_owned("other/x.rs", "fn main() {}"))
        self.assertFalse(classify.toolkit_owned("Cargo.toml", None))


class TestSeverityMarker(unittest.TestCase):
    """Criterion-level severity: `- [LEVEL] ...` overrides the rule's `**Severity**`.

    Severity is declared per rule and inherited by every criterion under it, so one label ranks
    a whole family: 22 criteria inherit CRITICAL from RUST-SEC-001, which spans both a leaked
    token and a config field with no length cap. The marker is the escape hatch.
    """

    def test_marker_matches_every_level(self):
        for lvl in ("CRITICAL", "HIGH", "MEDIUM", "LOW"):
            m = lint.SEVERITY_MARKER.match(f"[{lvl}] some trigger text")
            self.assertIsNotNone(m, lvl)
            self.assertEqual(m.group(1), lvl)
            self.assertEqual(m.group(2), "some trigger text")

    def test_marker_needs_the_whole_word_and_a_trigger(self):
        for bad in ("[CRIT] x", "[Critical] x", "[HIGH]", "[HIGH]x", "text [HIGH] mid-line"):
            self.assertIsNone(lint.SEVERITY_MARKER.match(bad), bad)

    def test_a_markdown_checkbox_is_not_a_severity_marker(self):
        """`- [ ]` and `- [x]` must not be read as a malformed level."""
        for box in ("[ ] todo", "[x] done"):
            self.assertIsNone(lint.SEVERITY_MARKER.match(box), box)
            self.assertIsNone(re.match(r"^\[([A-Za-z]{2,})\]\s", box), box)

    def test_an_invalid_level_is_caught_as_a_typo(self):
        """The check that fires on `[BLOCKER]`: the intent is lost silently otherwise."""
        for typo in ("[BLOCKER] x", "[CRIT] x", "[Critical] x"):
            self.assertIsNone(lint.SEVERITY_MARKER.match(typo))
            self.assertIsNotNone(re.match(r"^\[([A-Za-z]{2,})\]\s", typo), typo)

    def test_the_live_corpus_has_no_invalid_markers(self):
        for path in sorted((ROOT / "docs/toolkit-pr-review/rules").glob("*.md")):
            for n, body in lint.criteria_of(path.read_text(encoding="utf-8")):
                bad = re.match(r"^\[([A-Za-z]{2,})\]\s", body)
                if bad:
                    self.assertIn(bad.group(1), lint.SEVERITIES,
                                  f"{path.name}:{n} has a non-severity bracket marker")

    def test_markers_survive_rendering(self):
        """render-rules strips rationale and gated criteria; it must not eat the marker."""
        src = ROOT / "docs/toolkit-pr-review/rules"
        out = ROOT / "docs/toolkit-pr-review/agent-rules"
        for path in sorted(src.glob("*.md")):
            rendered = out / path.name
            if not rendered.exists():
                continue
            kept = {b for _, b in lint.criteria_of(rendered.read_text(encoding="utf-8"))
                    if lint.SEVERITY_MARKER.match(b)}
            for _, body in lint.criteria_of(path.read_text(encoding="utf-8")):
                m = lint.SEVERITY_MARKER.match(body)
                # a marked criterion is either dropped by a version gate or kept verbatim
                if m and body in {b for _, b in lint.criteria_of(rendered.read_text(encoding="utf-8"))}:
                    self.assertIn(body, kept)

    def test_conventions_and_the_agent_prompt_both_document_the_override(self):
        """An undocumented marker is inert: the agent reads the rule's level and moves on."""
        conv = (ROOT / "docs/toolkit-pr-review/review-conventions.md").read_text(encoding="utf-8")
        subj = (ROOT / "docs/toolkit-pr-review/agents/subject.md").read_text(encoding="utf-8")
        self.assertIn("[MEDIUM]", conv)
        self.assertIn("override", conv.lower())
        self.assertIn("[MEDIUM]", subj)
        self.assertIn("wins over the rule", subj)


class TestBudget(unittest.TestCase):
    def test_est_tokens_is_monotone_and_rounds_up(self):
        self.assertEqual(budget.est_tokens(0), 0)
        self.assertEqual(budget.est_tokens(1), 1)
        self.assertEqual(budget.est_tokens(4), 1)
        self.assertEqual(budget.est_tokens(5), 2)

    def test_every_module_has_a_rule_file(self):
        """budget.MODULES drives the spawn list; a name with no rule file spawns a blind agent."""
        for m in budget.MODULES:
            self.assertTrue((budget.RULES_DIR / f"{m}.md").exists(),
                            f"agent-rules/{m}.md is missing for module {m}")

    def test_instruction_tokens_counts_the_module_and_the_shared_docs(self):
        """The estimate must move when a rule module grows, or it silently understates cost."""
        for m in budget.MODULES:
            self.assertGreater(budget.instruction_tokens(m), budget.PROMPT_TOKENS,
                               f"{m} instructions estimated at the bare prompt size")

    def test_every_subject_agent_is_charged_for_the_whole_corpus(self):
        """The cost of this design is that the diff is read once per subject, not once.

        A model that charged the corpus a single time would understate a large PR roughly
        sixfold, which is the number that decides whether a review is affordable.
        """
        costs = budget.agent_costs(corpus_tokens=100_000, diff_tokens=10_000)
        self.assertEqual(len(costs), len(budget.MODULES) + 1)
        for m in budget.MODULES:
            self.assertGreater(costs[m], 110_000)
        # The architecture pass works from the diff alone, so it must be the cheapest.
        self.assertLess(costs["architecture"], min(costs[m] for m in budget.MODULES))

    def test_total_cost_grows_with_the_pr(self):
        small = budget.total_cost(10_000, 1_000)
        big = budget.total_cost(200_000, 20_000)
        self.assertGreater(big, small * 5)


class TestPrepareOffline(unittest.TestCase):
    """End-to-end `prepare` against saved diffs: no network, no agents."""

    def _prepare(self, pr: str) -> tuple[Path, dict]:
        work = Path(tempfile.mkdtemp(prefix="prreview-test."))
        meta = FIXTURES / pr / "meta.json"
        cmd = [sys.executable, str(SCRIPTS / "review.py"), "prepare",
               "--from-diff", str(FIXTURES / pr / "diff.patch"),
               "--work-dir", str(work)]
        if meta.exists():
            cmd += ["--from-meta", str(meta)]
        p = subprocess.run(cmd, capture_output=True, text=True)
        self.assertEqual(p.returncode, 0, p.stderr)
        return work, json.loads((work / "context.json").read_text())

    def test_every_module_gets_an_agent_over_every_file(self):
        """Six subject agents, each holding one module, each seeing the whole PR.

        The design this replaced handed each agent a slice of the files and all six
        modules. Measured on five real PRs it reproduced 44% of a careful reviewer's
        findings against 58% here, so the partition being tested is over rules, not files.
        """
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                _, ctx = self._prepare(pr)
                by_name = {a["name"]: a for a in ctx["agents"]}
                expected = [m for m in budget.MODULES
                            if m != "toolkit" or ctx["toolkit_files"]] + ["architecture"]
                self.assertEqual(sorted(by_name), sorted(expected))
                for m in budget.MODULES:
                    if m == "toolkit":
                        continue
                    self.assertEqual(sorted(by_name[m]["files"]), sorted(ctx["all_files"]),
                                     f"{m} was not given every file")

    def test_the_toolkit_agent_is_scoped_to_toolkit_owned_files(self):
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                _, ctx = self._prepare(pr)
                tk = next((a for a in ctx["agents"] if a["name"] == "toolkit"), None)
                if not ctx["toolkit_files"]:
                    self.assertIsNone(tk, "toolkit agent spawned with nothing to review")
                    continue
                self.assertEqual(tk["files"], ctx["toolkit_files"])
                self.assertTrue(set(tk["files"]) <= set(ctx["all_files"]))

    def test_only_the_security_agent_carries_the_manifests(self):
        """RUST-DEP-001 lives in security.md and nowhere else; the list must not leak."""
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                _, ctx = self._prepare(pr)
                for a in ctx["agents"]:
                    if a["name"] == "security":
                        self.assertEqual(a["manifest_files"], ctx["manifest_files"])
                    else:
                        self.assertEqual(a["manifest_files"], [])

    def test_no_agent_gets_a_diff_slice(self):
        """Every agent reads the whole diff; a per-agent slice would be the old design."""
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                work, ctx = self._prepare(pr)
                self.assertTrue((work / "diff.patch").exists())
                self.assertFalse((work / "shards").exists(),
                                 "prepare still writes per-shard diff slices")

    def test_out_of_scope_files_are_recorded_not_reviewed(self):
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                _, ctx = self._prepare(pr)
                for p in ctx["skipped_files"]:
                    self.assertFalse(classify.in_scope(p), p)
                    self.assertNotIn(p, ctx["all_files"])

    def test_snapshot_paths_are_injective(self):
        """Two distinct repo paths must never share a snapshot file.

        The `/` -> `__` escaping this replaces collided: `gears/mini__chat/src/lib.rs`
        and `gears/mini/chat/src/lib.rs` produced one name, so a shard silently reviewed
        the wrong file's contents under the right file's name.
        """
        colliding = ["gears/mini__chat/src/lib.rs", "gears/mini/chat/src/lib.rs"]
        escaped = {p.replace("/", "__") for p in colliding}
        self.assertEqual(len(escaped), 1, "the old scheme was expected to collide")
        mirrored = {f"files/{p}" for p in colliding}
        self.assertEqual(len(mirrored), 2)

    def test_context_records_one_entry_per_in_scope_file(self):
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                _, ctx = self._prepare(pr)
                self.assertEqual(sorted(ctx["files"]), sorted(ctx["all_files"]))
                for rec in ctx["files"].values():
                    self.assertIn("ranges", rec)
                    self.assertIn("right", rec["ranges"])
                    self.assertIn("left", rec["ranges"])

    def test_edge_cases_fixture(self):
        """One fixture covering deleted, added, renamed, binary and out-of-scope files."""
        if "pr-synthetic-edges" not in FIXTURE_PRS:
            self.skipTest("edge fixture missing")
        _, ctx = self._prepare("pr-synthetic-edges")
        f = ctx["files"]

        gone = f["gears/foo/src/gone.rs"]
        self.assertEqual(gone["status"], "deleted")
        self.assertEqual(gone["snapshot_ref"], "base",
                         "a deleted file exists only in the base commit")
        self.assertEqual(gone["ranges"]["right"], [],
                         "a deleted file has no RIGHT side at all")
        self.assertEqual(gone["ranges"]["left"], [[1, 3]])

        # Git quotes a header path holding a non-ASCII byte. The header regex used to
        # anchor on a literal `a/`, so this file matched nothing and left the review
        # without a warning.
        quoted = f["gears/foo/src/fr\u00fch.rs"]
        self.assertEqual(quoted["status"], "modified")
        self.assertEqual(quoted["ranges"]["right"], [[3, 3]])
        self.assertIn("gears/foo/src/fr\u00fch.rs", ctx["all_files"],
                      "a quoted-path file stays in review scope")

        added = f["gears/foo/src/added.rs"]
        self.assertEqual(added["status"], "added")
        self.assertEqual(added["ranges"]["right"], [[1, 2]])

        renamed = f["gears/foo/src/new_name.rs"]
        self.assertEqual(renamed["status"], "renamed")
        self.assertEqual(renamed["old_path"], "gears/foo/src/old_name.rs")

        self.assertTrue(f["Cargo.lock"]["manifest"])
        self.assertIsNone(f["Cargo.lock"]["snapshot"],
                          "Cargo.lock is tracked for anchoring but never snapshotted")

        self.assertEqual(sorted(ctx["skipped_files"]),
                         ["assets/logo.png", "k8s/values.yaml"])

    def test_a_source_and_its_tests_reach_the_same_agent(self):
        """The `lock.rs` / `lock_tests.rs` split that lost a finding cannot recur.

        On PR 4778 the packer put them in different shards, so the agent holding the
        source could not establish that a test was missing and correctly declined to
        report. With every agent holding every file the question cannot arise, and this
        test states that as an invariant rather than leaving it implicit.
        """
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                _, ctx = self._prepare(pr)
                tests_agent = next(a for a in ctx["agents"] if a["name"] == "tests")
                seen = set(tests_agent["files"])
                for f in ctx["all_files"]:
                    src = classify.test_sibling_source(f)
                    if src and src in set(ctx["all_files"]):
                        self.assertIn(src, seen)
                        self.assertIn(f, seen)

    def test_meta_json_is_written_in_every_mode(self):
        """The harnesses read the summary header from meta.json.

        It used to be written only in PR mode, which meant every harness needed a local-mode
        fallback that no test and no normal run ever exercised.
        """
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                work, _ = self._prepare(pr)
                self.assertTrue((work / "meta.json").exists())
                json.loads((work / "meta.json").read_text())

    def test_prepare_creates_the_directories_the_harnesses_write_into(self):
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                work, _ = self._prepare(pr)
                for d in ("files", "out"):
                    self.assertTrue((work / d).is_dir(), f"{d}/ missing")

    def test_snapshots_mirror_the_repo_tree(self):
        """No filename escaping: `/`->`__` is not injective and collided silently."""
        for pr in FIXTURE_PRS:
            with self.subTest(pr=pr):
                work, ctx = self._prepare(pr)
                for path, rec in ctx["files"].items():
                    if rec["snapshot"] is None:
                        continue
                    self.assertEqual(rec["snapshot"], f"files/{path}")
                    self.assertTrue((work / rec["snapshot"]).exists())
                    self.assertNotIn("__", Path(rec["snapshot"]).name.replace(Path(path).name, ""))

    def test_max_total_tokens_refuses_instead_of_degrading(self):
        pr = FIXTURE_PRS[0]
        work = Path(tempfile.mkdtemp(prefix="prreview-test."))
        p = subprocess.run(
            [sys.executable, str(SCRIPTS / "review.py"), "prepare",
             "--from-diff", str(FIXTURES / pr / "diff.patch"),
             "--work-dir", str(work), "--max-total-tokens", "1"],
            capture_output=True, text=True)
        self.assertEqual(p.returncode, 2)
        self.assertIn("max-total-tokens", p.stderr)


if __name__ == "__main__":
    unittest.main()
