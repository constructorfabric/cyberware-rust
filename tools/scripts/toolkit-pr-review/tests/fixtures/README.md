# Fixtures

Saved `gh pr diff` output plus the matching `gh pr view --json` metadata, so `prepare`
can be exercised with no network and no review agents. A real review run costs about
1.1M tokens; everything here runs in about a second.

| Fixture | What it is there for |
|---|---|
| `pr-4676` | Smallest real case: two files, one shard |
| `pr-4738` | A messy real diff: 24 files in scope, 3 shards, one file out of scope |
| `pr-synthetic-edges` | Deleted, added and renamed files, a binary file, `Cargo.lock`, and out-of-scope YAML, hand-written so each case is unambiguous |

Deliberately **not** kept: a very large PR diff. The 53-file PR 4777 came to 110 KB
compressed and added almost nothing over `pr-4738` — packing behaviour at scale is
covered by the unit tests on `shard.pack` and `shard.waves`, which build their own
inputs and cost nothing.

To add one:

```bash
gh pr diff <N> --repo <owner/name> > pr-<N>/diff.patch
gh pr view <N> --repo <owner/name> \
  --json number,title,headRefOid,baseRefOid,baseRefName,headRefName,isCrossRepository \
  > pr-<N>/meta.json
```
