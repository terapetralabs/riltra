# Lexical ranker R0: the held-out run (one run, 2026-11-16)

Status: the held-out split of the Riltra task set was run **once**, with ranker R0 unchanged, on the frozen
commit `77efc9e` (the merge of PR 71). The owner authorized the run on 2026-11-16, after PR 71 was merged with
green CI. It is not rerun and nothing was tuned afterwards. That the authorization was given, and that the run was
done once, are statements of the operator recorded here; the output file alone does not prove either. The result
applies to the **lexical ranker only**:
edge expansion along proven edges (`via`) is a separate later PR, so plan Task 8 is not complete. Numbers are
counts on a small sample; no comparison, significance or savings claim is made.

## What was run

- Binary: built with `cargo build --locked` from `77efc9e` in a clean checkout. `riltra_version`
  `riltra 0.2.0-rc.1`, binary sha256 `7973f262618245812036ea4f2ad0b3217bbc1e0368e6135e52161f478b31f898`.
- Command: `python -I docs/benchmarks/workflow/harness.py run --split heldout --final` on `tasks-riltra.jsonl`
  (manifest digest `84598bd71c1b6eef296d1e6531c4dec2d91e2556dfdf5c46da5ae3980894e88c`), `rules_version` `pr8-rules-1`, `measure_version` `pr9b-measure-1`.
- Output: 36 task lines and one summary line, exit 0; every task is `ok` for every baseline (0 failed, 0 R
  abstentions). sha256 of the output file: `e2aea7d23e02c22ee11943fe6c0b8b2b47ee0000d3122ac047328386e7f78b7a`.
- Verification of the numbers below: every summary value was recomputed from the per-task rows of its output
  file (hits, first-rank blocks, medians, pairs, strata) and equals the value the harness printed.

### Tune check before the run

The tune split was run again with the same binary. The task lines and the summary line are byte-identical
to the PR 9b tune output: the per-task metrics and summary are unchanged. Full result ordering was not recorded
or compared. The run header differs in one field,
`riltra_sha256`: `7edf42b3e6d530175b9fa57e11a79f2b262fbcf8fca87c6f2f792243fb5e8677` in the PR 9b output and
`7973f262618245812036ea4f2ad0b3217bbc1e0368e6135e52161f478b31f898` here. The sources (`crates`, `Cargo.toml`, `Cargo.lock`) do not differ between the PR 9b head
and `77efc9e`. The cause of the hash difference was not established (a different checkout path and build time
are candidates; an embedded-path search of the binary found nothing), and the earlier binary was overwritten.

## Held-out result (36 tasks)

A task is a hit at 5 or at 10 when at least one gold file is in the top 5 or the top 10. The first-gold-rank block
is `found` / `none` / `median` (lower median of the ranks) / `rank1` / `le3`.

| baseline | ok | failed | hits at 5 | hits at 10 | first gold rank: found / none / median / rank1 / le3 | returned median |
|---|---|---|---|---|---|---|
| R (lexical concept search, R0) | 36 | 0 | 32 | 36 | 36 / 0 / 1 / 19 / 29 | 32 |
| B1 `git grep` | 36 | 0 | 11 | 15 | 15 / 21 / 1 / 8 / 10 | 0 |
| B2 BM25 over paths | 36 | 0 | 23 | 24 | 24 / 12 / 1 / 18 / 20 | 6 |
| B3 current `search` | 36 | 0 | 7 | 7 | 8 / 28 / 1 / 6 / 7 | 0 |
| S size control (not a competitor) | 36 | 0 | 11 | 24 | 36 / 0 / 8 / 1 / 7 | 101 |

### Tune and held-out side by side (counts only)

| baseline | tune hits at 5 (of 81) | tune hits at 10 | held-out hits at 5 (of 36) | held-out hits at 10 |
|---|---|---|---|---|
| R (lexical concept search, R0) | 73 | 78 | 32 | 36 |
| B1 `git grep` | 37 | 38 | 11 | 15 |
| B2 BM25 over paths | 34 | 37 | 23 | 24 |
| B3 current `search` | 21 | 23 | 7 | 7 |
| S size control (not a competitor) | 58 | 71 | 11 | 24 |

### Pair win/loss/tie (frozen rule: higher found@10 wins; over tasks where both are ok)

| pair | held-out both ok | first wins | second wins | ties | tune first / second / ties (of 81) |
|---|---|---|---|---|---|
| R vs B1 | 36 | 22 | 4 | 10 | 50 / 1 / 30 |
| R vs B2 | 36 | 16 | 2 | 18 | 55 / 1 / 25 |
| R vs B3 | 36 | 30 | 0 | 6 | 64 / 1 / 16 |
| R vs S | 36 | 19 | 2 | 15 | 14 / 9 / 58 |
| B1 vs B2 | 36 | 5 | 15 | 16 | 21 / 15 / 45 |
| B1 vs B3 | 36 | 10 | 0 | 26 | 23 / 6 / 52 |
| B2 vs B3 | 36 | 21 | 2 | 13 | 25 / 12 / 44 |

### Mention classes and strata (M1; held-out)

Classes: `named` 0, `stem` 13, `none` 23 tasks. `none` means "no mention that M1 accepts"; see the README for
its limits. The `named` class is empty in this split, and the other two are small: no inference is drawn.

#### `stem` (13 tasks)

| baseline | ok | failed | hits at 5 | hits at 10 | first gold rank: found / none / median / rank1 / le3 |
|---|---|---|---|---|---|
| R (lexical concept search, R0) | 13 | 0 | 13 | 13 | 13 / 0 / 1 / 9 / 13 |
| B1 `git grep` | 13 | 0 | 5 | 8 | 8 / 5 / 2 / 3 / 5 |
| B2 BM25 over paths | 13 | 0 | 13 | 13 | 13 / 0 / 1 / 11 / 13 |
| B3 current `search` | 13 | 0 | 4 | 4 | 5 / 8 / 1 / 4 / 4 |
| S size control (not a competitor) | 13 | 0 | 3 | 10 | 13 / 0 / 8 / 1 / 3 |

#### `none` (23 tasks)

| baseline | ok | failed | hits at 5 | hits at 10 | first gold rank: found / none / median / rank1 / le3 |
|---|---|---|---|---|---|
| R (lexical concept search, R0) | 23 | 0 | 19 | 23 | 23 / 0 / 2 / 10 / 16 |
| B1 `git grep` | 23 | 0 | 6 | 7 | 7 / 16 / 1 / 5 / 5 |
| B2 BM25 over paths | 23 | 0 | 10 | 11 | 11 / 12 / 1 / 7 / 7 |
| B3 current `search` | 23 | 0 | 3 | 3 | 3 / 20 / 1 / 2 / 3 |
| S size control (not a competitor) | 23 | 0 | 8 | 14 | 23 / 0 / 6 / 0 / 4 |

### Per-task table (counts only; short shas; no query text)

Each cell is `f@5/f@10/first`: gold files in the top 5 and in the top 10, then the rank of the first gold file
(`-` when none is returned). `n` is the number of scored gold files; `mention` is the M1 class.

| task | n | mention | R | B1 | B2 | B3 | S |
|---|---|---|---|---|---|---|---|
| d0b3f0058d31 | 2 | none | 1/1/2 | 2/2/1 | 0/1/6 | 0/0/- | 0/0/15 |
| 9583bcf537c3 | 2 | none | 1/1/1 | 2/2/4 | 1/1/4 | 0/0/- | 0/0/13 |
| 4d72a839f106 | 2 | none | 1/1/5 | 0/2/7 | 0/0/- | 0/0/- | 0/0/13 |
| 73de41738f12 | 1 | none | 1/1/4 | 0/0/- | 1/1/5 | 0/0/- | 1/1/3 |
| 7c9a3baec558 | 2 | none | 2/2/1 | 2/2/1 | 1/2/1 | 0/0/- | 0/1/6 |
| c273b0ee1050 | 6 | stem | 4/5/1 | 0/0/- | 2/2/1 | 0/0/- | 1/3/1 |
| d6d5b368402d | 1 | none | 1/1/3 | 0/0/- | 1/1/1 | 0/0/- | 1/1/3 |
| 567ed0970f60 | 2 | none | 2/2/1 | 0/0/- | 2/2/1 | 0/0/- | 0/1/6 |
| 4e5c9d685ee3 | 1 | stem | 1/1/2 | 0/1/10 | 1/1/2 | 1/1/1 | 0/0/18 |
| ad7c38ce4035 | 1 | none | 1/1/1 | 0/0/- | 0/0/- | 0/0/- | 1/1/4 |
| 186f54f43ae0 | 1 | none | 1/1/1 | 1/1/1 | 0/0/- | 1/1/2 | 1/1/4 |
| 43b18c517513 | 1 | none | 1/1/1 | 0/0/- | 0/0/- | 0/0/- | 1/1/3 |
| 3231401b78f1 | 1 | none | 0/1/6 | 1/1/1 | 1/1/1 | 1/1/1 | 0/1/10 |
| a11cf1e25a36 | 2 | none | 1/1/2 | 0/0/- | 0/0/- | 0/0/- | 1/2/5 |
| fd6b4f04a961 | 1 | stem | 1/1/3 | 0/1/6 | 1/1/2 | 0/0/- | 0/1/9 |
| 33b94b7e8aa2 | 1 | none | 1/1/1 | 1/1/1 | 0/0/- | 1/1/1 | 0/0/12 |
| dab4dc1016bc | 1 | none | 1/1/1 | 0/0/- | 1/1/5 | 0/0/- | 0/0/12 |
| 33604963a72c | 1 | stem | 1/1/1 | 1/1/2 | 1/1/1 | 1/1/1 | 0/1/9 |
| 6aa113b13b90 | 1 | stem | 1/1/3 | 0/0/- | 1/1/1 | 0/0/- | 0/1/8 |
| 552e876303b6 | 3 | none | 2/2/3 | 0/0/- | 0/0/- | 0/0/- | 3/3/3 |
| 46b8dd6b27d4 | 1 | none | 0/1/10 | 0/0/- | 0/0/- | 0/0/- | 1/1/4 |
| f3b8b01bf254 | 1 | none | 0/1/9 | 0/0/- | 0/0/- | 0/0/- | 0/0/11 |
| 03819d3c4679 | 5 | none | 2/4/2 | 0/0/- | 0/0/- | 0/0/- | 0/2/6 |
| 19f0a0508748 | 1 | none | 1/1/1 | 0/0/- | 1/1/1 | 0/0/- | 0/1/7 |
| 9074489c121e | 5 | stem | 2/5/1 | 0/0/- | 2/2/1 | 0/0/- | 0/2/8 |
| e1f5d1ae0181 | 1 | none | 0/1/7 | 0/0/- | 0/0/- | 0/0/- | 0/1/6 |
| acd682438392 | 1 | none | 1/1/1 | 0/0/- | 1/1/1 | 0/0/- | 0/0/25 |
| 52aa5c47eff4 | 1 | none | 1/1/2 | 0/0/- | 1/1/1 | 0/0/- | 0/0/25 |
| c90e01d0a70e | 2 | stem | 1/2/2 | 0/0/- | 1/1/1 | 0/0/- | 0/0/12 |
| 20f973d4270b | 3 | stem | 1/1/1 | 2/2/2 | 3/3/1 | 1/1/1 | 0/1/10 |
| 67ae909f9d70 | 2 | stem | 1/1/1 | 1/1/1 | 2/2/1 | 0/0/- | 0/1/8 |
| 4ab6c62bd207 | 1 | stem | 1/1/1 | 1/1/1 | 1/1/1 | 0/0/- | 0/1/8 |
| 2dd9f48c0ca0 | 4 | stem | 1/3/1 | 0/1/6 | 3/3/1 | 0/0/13 | 1/2/3 |
| 8942bccde5a3 | 3 | stem | 2/3/1 | 0/0/- | 2/2/1 | 0/0/- | 0/0/15 |
| 982c3891e29a | 1 | none | 1/1/4 | 0/0/- | 0/0/- | 0/0/- | 0/0/34 |
| 385b524f6514 | 3 | stem | 2/3/1 | 3/3/1 | 3/3/1 | 2/2/1 | 1/1/3 |

## Limits

- 36 tasks. Counts are not rates, and no threshold, significance, comparison or savings claim is made.
- The two splits cover different parts of the history. By author date, the 36 held-out commits run from
  2026-10-25 to 2026-11-14: 33 are dated before 2026-11-13 (the date of the replacement plan) and 3 on 2026-11-13
  or 2026-11-14. The 81 tune commits run from 2026-10-14 to 2026-10-25. The tune and held-out counts above are
  shown side by side without an explanation of any difference between them; none was examined.
- R returns a median of 32 files per task and B1, B2, B3 return 0, 6, 0, so the pair verdicts compare a ranked
  list with much shorter lists. S ranks the whole candidate set (median 101 files).
- S is a control. A high S value does not say that a ranker is poor, and a low one does not say that it is good.
- The strata are small and `named` is empty. M1 is descriptive, not a ceiling or a bound.
- R0 is lexical and follows no call edges; `via` is not part of this result.
- This run is the only held-out measurement. The ranker was not changed before or after it, and it is not rerun.
- The second local repository was not part of this run.
