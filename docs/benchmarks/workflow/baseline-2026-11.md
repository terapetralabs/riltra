# Baseline 2026-11: workflow query task set (Riltra tuning split)

Measured with `harness.py run --split tune` on the frozen Riltra manifest (`tasks-riltra.jsonl`,
digest `84598bd71c1b6eef...`). Rules version `pr8-rules-1`. The
riltra binary was `riltra 0.2.0-rc.1` (sha256 `5ee072d6089b61b3...`). Two runs
produced byte-identical output (no timings are printed). Counts only; no percentages and no
comparison or savings claim. The tuning split is 81 tasks, and that sample is too small for any
threshold.

## Repositories

- Riltra repository: master tip `5a1aac537b026ea60fb92f92690802476dceeb5d`.
- Second local repository: measured locally; its results are not published until the owner approves its aggregates.

## Rule 1 and Rule 2 exclusion counts (Riltra)

| step | count |
|---|---|
| non-merge commits from the tip | 339 |
| merge commits (counted, not tasks) | 72 |
| more than 8 changed files | 75 |
| no changed file in a supported language | 137 |
| no gold file after Rule 2 | 10 |
| kept tasks | 117 |
| tune split (oldest) | 81 |
| held-out split (count only; not run) | 36 |

Check: 75 + 137 + 10 + 117 = 339.

## Tuning-split results (81 tasks)

B3 status: `ok` 81, `index-failed` 0, `search-error` 0 (no B3 failures). B1 and B2 have no failure status.

Aggregate per baseline (a task counts as a hit when at least one gold file is in the top 10):

- B1 `git grep`: 38 of 81 measured tasks; 0 failed of 81 tuning tasks.
- B2 BM25 over paths: 37 of 81 measured tasks; 0 failed of 81 tuning tasks.
- B3 current `search`: 23 of 81 measured tasks; 0 failed of 81 tuning tasks.

Win/loss per pair, rule frozen in the brief: the higher found@10 wins, the lower loses, equal is a tie,
over tasks where both baselines are `ok` (sample size 81 for every pair). `found@5` and
`first_gold_rank` are reported per task below and are not part of the verdict.

| pair | both ok | first wins | second wins | ties |
|---|---|---|---|---|
| B1 vs B2 | 81 | 21 | 15 | 45 |
| B1 vs B3 | 81 | 23 | 6 | 52 |
| B2 vs B3 | 81 | 25 | 12 | 44 |

## Per-task table (counts only; short shas; no query text)

"f@5" and "f@10" are gold files in the top 5 and top 10. "first" is the rank of the first gold file
(`-` when none is returned). `n` is the number of scored gold files.

| task | gold n | B1 f@5 | B1 f@10 | B1 first | B2 f@5 | B2 f@10 | B2 first | B3 f@5 | B3 f@10 | B3 first |
|---|---|---|---|---|---|---|---|---|---|---|
| 1dcedba95c34 | 3 | 2 | 2 | 3 | 0 | 0 | - | 3 | 3 | 2 |
| d675690a199d | 3 | 0 | 0 | - | 1 | 1 | 1 | 0 | 0 | - |
| d6c642dd480b | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| b212090d2384 | 3 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 1d3b2ffc6301 | 1 | 1 | 1 | 1 | 0 | 0 | - | 1 | 1 | 1 |
| 6878ba1d27b0 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 0a46bfc08a71 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| c596da8cf16d | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 72a9b8eb3eb4 | 4 | 2 | 2 | 2 | 1 | 1 | 1 | 1 | 1 | 4 |
| f3e4975ed42b | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| edc726f9d8ad | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 7faf2de52aaa | 2 | 1 | 1 | 1 | 0 | 0 | - | 0 | 0 | - |
| 360e7fb60565 | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 712ed05e2082 | 1 | 0 | 0 | - | 1 | 1 | 1 | 0 | 0 | - |
| cf3a62689a93 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| cc4138d5e7f9 | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| d715afbd2af4 | 1 | 1 | 1 | 4 | 0 | 0 | - | 1 | 1 | 4 |
| ceda5d468baf | 1 | 0 | 0 | - | 1 | 1 | 2 | 0 | 0 | - |
| 4c9a0bdfeb37 | 7 | 0 | 0 | - | 2 | 5 | 1 | 0 | 0 | - |
| fd7dcae82eba | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| e6f4f33aae6b | 3 | 1 | 1 | 1 | 0 | 0 | - | 0 | 0 | - |
| 47b9980993ba | 3 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 2c8abdb8c1ba | 3 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 2f57cee33a97 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 517e2a97ca6a | 1 | 0 | 0 | - | 1 | 1 | 1 | 0 | 0 | - |
| 99f3e768f59b | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 684e3c817927 | 4 | 1 | 1 | 1 | 0 | 0 | - | 2 | 2 | 1 |
| 8ce51f28c518 | 1 | 1 | 1 | 1 | 0 | 0 | - | 1 | 1 | 1 |
| 87244f08bb50 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| d3cf765359fe | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 6c21bd63cca4 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 62845bf40320 | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| b7aefee75f94 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| f7ccbc86efee | 2 | 0 | 1 | 6 | 0 | 0 | - | 0 | 0 | - |
| f8a040e147c6 | 2 | 0 | 0 | - | 0 | 0 | - | 2 | 2 | 1 |
| 7216b233643d | 1 | 0 | 0 | - | 0 | 0 | - | 1 | 1 | 2 |
| 07ea42e2c317 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 25677baa259b | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 6f98ccfb4630 | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 0e61509604cd | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 8e86a6e7b298 | 2 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 4592d61590e8 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 3b6b06804baa | 2 | 0 | 0 | - | 1 | 1 | 5 | 0 | 0 | - |
| 40d278050c11 | 2 | 2 | 2 | 2 | 1 | 1 | 4 | 0 | 0 | - |
| 70f631af6301 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 0 | 0 | - |
| 9c96313fb8e2 | 4 | 1 | 1 | 1 | 3 | 3 | 1 | 0 | 0 | - |
| 11b4caa1ff6b | 3 | 0 | 0 | - | 2 | 2 | 1 | 0 | 0 | - |
| 46f61848907b | 3 | 0 | 0 | - | 2 | 2 | 1 | 0 | 0 | - |
| 389c746d3c21 | 4 | 0 | 0 | - | 2 | 2 | 1 | 0 | 0 | - |
| 1ddaea3bf04a | 2 | 2 | 2 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| 34c4ee37a15b | 1 | 0 | 0 | - | 1 | 1 | 1 | 0 | 0 | - |
| 00029b3a18b6 | 3 | 1 | 1 | 1 | 1 | 1 | 1 | 0 | 0 | - |
| ee14612e1ac3 | 1 | 1 | 1 | 1 | 0 | 0 | - | 0 | 0 | - |
| 7064773e59a4 | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 0 | 0 | - |
| 7736d6ef8745 | 2 | 2 | 2 | 1 | 2 | 2 | 1 | 1 | 2 | 1 |
| fb8d1090f9ea | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 40a31685da7f | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 1 | 1 | 1 |
| 43a5ee6d521a | 1 | 1 | 1 | 1 | 0 | 0 | - | 1 | 1 | 1 |
| 303f7bc5f6e1 | 1 | 1 | 1 | 4 | 1 | 1 | 2 | 0 | 1 | 10 |
| 6e1b73c95c62 | 2 | 2 | 2 | 1 | 2 | 2 | 1 | 0 | 0 | - |
| a505271adafb | 1 | 0 | 0 | 13 | 1 | 1 | 3 | 1 | 1 | 2 |
| 226c88c7523a | 2 | 2 | 2 | 2 | 1 | 1 | 2 | 0 | 0 | - |
| 3cc6ed31cfb9 | 3 | 1 | 1 | 3 | 2 | 2 | 1 | 0 | 0 | - |
| c61ea50ca274 | 3 | 2 | 2 | 1 | 2 | 2 | 2 | 3 | 3 | 1 |
| d1ce68fa62de | 1 | 1 | 1 | 3 | 1 | 1 | 1 | 0 | 0 | - |
| 2293167542dc | 1 | 1 | 1 | 2 | 1 | 1 | 4 | 0 | 0 | - |
| 92ee6204eb46 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 0 | 0 | 20 |
| c97ee7a44360 | 2 | 2 | 2 | 1 | 1 | 1 | 3 | 0 | 0 | - |
| cee1dd2efc60 | 1 | 0 | 0 | - | 1 | 1 | 1 | 0 | 0 | - |
| e71a429957a7 | 2 | 0 | 0 | 20 | 1 | 1 | 1 | 0 | 0 | - |
| 54c7d26497f3 | 1 | 0 | 0 | - | 0 | 0 | - | 0 | 0 | - |
| 22bc7b721b96 | 4 | 2 | 2 | 1 | 0 | 2 | 8 | 0 | 0 | - |
| 9847ae08045d | 2 | 1 | 1 | 4 | 0 | 1 | 9 | 0 | 1 | 10 |
| 1de6b9632321 | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 1 | 1 | 2 |
| 37b9dd307181 | 6 | 2 | 3 | 2 | 0 | 0 | - | 3 | 3 | 1 |
| 80f82c275fe5 | 2 | 1 | 1 | 4 | 0 | 0 | - | 1 | 1 | 3 |
| 4bc8ed5bedb9 | 1 | 1 | 1 | 3 | 0 | 0 | - | 0 | 0 | - |
| f9eed4858ced | 2 | 1 | 2 | 2 | 1 | 1 | 2 | 1 | 1 | 3 |
| 619636ef0add | 2 | 1 | 2 | 5 | 0 | 1 | 9 | 1 | 1 | 2 |
| e3f25d1816a2 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| e9110524b68f | 2 | 2 | 2 | 1 | 0 | 0 | - | 1 | 1 | 1 |

## Not measured

- The held-out split (36 tasks) is not run. PR 9 is the only consumer; it runs the held-out split
  once, together with the ranker.
- The second local repository's results (only the placeholder above until the owner approves).
- Any ranker, any `riltra query` output, and any MCP tool output.

## Limits

- Sample sizes are small (81 tuning tasks; 36 held-out, not run). No threshold or significance claim is made.
- The newest tasks are mostly this plan's own PRs, so the held-out split is not representative of
  the older history. The tuning split is the older part of the history.
- A history rewrite (owner-gated) invalidates the manifest; regenerate it with the same rules
  and `rules_version` before reusing these numbers.
- The first tune attempts failed on Windows path length while extracting parent trees (a long
  scratch path). The fix is ruling 12 in `README.md` plus a short scratch root. These numbers come
  from the rerun with the short scratch root.
