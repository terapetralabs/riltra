# Workflow query task set and baselines (PR 8; measurement rules as of PR 9b)

This folder holds a frozen set of concept-search tasks built from git history alone, the
Python 3 standard-library harness that builds and measures it, and the frozen baselines measured
on its tuning split: B1, B2, B3 (PR 8), R (the lexical ranker, PR 9) and S (a text-free size
control, PR 9b). Nothing here changes Riltra's Rust code, schema, parser, commands or MCP
tools. PR 9 (the ranker) was the first consumer. The held-out split was used by one final run on
2026-11-16 (see Split and `ranker-2026-11-heldout.md`). PR 9b closed the lexical evaluation's
measurement; edge expansion (`via`) is a separate later PR, so Task 8 is not complete.

Files:

- `harness.py`: `freeze` (builds a task manifest from git history) and `run` (measures B1, B2, B3, R, S).
- `selftest.py`: controls. Run before any measurement; it exits non-zero on the first failure.
- `tasks-riltra.jsonl` and `tasks-riltra.sha256`: the frozen Riltra manifest (header line, then one line per task).
- `baseline-2026-11.md`: the PR 8 tuning-split results for the Riltra repository, with limits.
- `ranker-2026-11.md`: the R0 ranker rules and the tuning-split results with the PR 9b measurement
  (mention classes, strata, control S, limits).

## Task definition

A task is one commit. The query is the commit message (subject and body as written, with only the
`Signed-off-by:` and `Co-Authored-By:` lines removed). The gold list is the set of files the commit
changed that a ranker could find. Nothing Riltra prints was used to build a task or a gold list.

Query text is never written to a tracked file. Tracked rows carry a 12-character short sha, the
gold paths (public repository paths), and counts. `run` re-reads each query from git by its short
sha and refuses if the sha no longer resolves.

## Rule 1: which commits become tasks

Commits come from `git log --no-merges <tip>`, sorted by (committer timestamp, full sha). The
steps apply in this order; each excluded commit is counted at the first step that removes it.

1. Merge commits (`git rev-list --merges <tip>`): counted, never tasks.
2. More than 8 changed files, all languages, from `git diff-tree --no-renames -r --root` (root commits included).
3. No changed file in a supported language. The extension list is copied from `classify()` in
   `crates/cli/src/syntax.rs` (lines 77-87): `py js mjs cjs ts mts cts tsx rs go java c h hh hpp hxx inl cc cpp cxx`.
   `.h` is C++. `cu` and `cuh` are not supported. Extensions are compared lowercased, as `classify()` does.
   `selftest.py` fails if the list is empty or differs from this list.
4. No gold file left after Rule 2.

## Rule 2: gold

Gold is every file the commit changed with status `M` or `D` (renames are split into `D` of the old
path and `A` of the new path, so only the old path can be gold) that exists in the parent tree, is
in a supported language, and is not a test path.

- Test path: a Python port of `is_test_path` in `crates/cli/src/index.rs` (lines 2715-2727). The
  selftest checks its 10 positive and 8 near-miss paths from `test_path_classifier`.
- `test_gold`: changed files with status `M` or `D` that exist in the parent, are supported, and are
  test paths. They are listed per task and are not scored in this PR.
- `gold_added_not_findable`: the number of added (`A`) supported non-test files of the commit. They
  cannot be found in the parent, so they are counted and never treated as misses.

## Split

After Rules 1 and 2 with `N` kept tasks, `tune` is the `(7*N)//10` oldest tasks and the rest are
`heldout`. `freeze --tip <full sha>` walks history from that commit only, so later commits cannot
move the boundary. A manifest is written once; a different tip needs a new file name. The manifest
header records the label, tip, `rules_version`, the non-merge count, the exclusion counts, and the
split sizes.

**Held-out was run once.** `run --split heldout` is refused unless `--final` is passed. PR 8, PR 9 and
PR 9b did not pass it. After PR 9b was merged with green CI, the owner gave an explicit instruction, and the split
(36 Riltra tasks) was run once on 2026-11-16 on the frozen commit `77efc9e`, with ranker R0 unchanged. The result is in
`ranker-2026-11-heldout.md`. The run is not repeated for this manifest: a second run would no longer be a held-out
measurement, and this README does not authorize one. A new task set needs a new manifest. The baselines have no
tunable parameter, so running them once together with the ranker loses nothing.

**Distribution shift.** The split is by time, so the two splits cover different parts of the history. By
author date, the 36 held-out commits run from 2026-10-25 to 2026-11-14 (33 dated before 2026-11-13, the date of
the replacement plan, and 3 on 2026-11-13 or 2026-11-14), and the 81 tuning commits run from 2026-10-14 to
2026-10-25. The held-out split is not representative of the older history.

## Baselines

All baselines rank files (each file once, at its best rank), restricted to the candidate universe:
supported-language, non-test files of the parent tree. Ties are broken by path ascending. No
parameter is tuned.

- **E0 tokens** (frozen extractor, `extract_tokens`): one left-to-right pass over the query, first
  20 distinct tokens. (a) text inside backticks (stripped, no newline inside the span);
  (b) path-like words ending in a supported extension (for example `src/main.go`);
  (c) identifiers of at least 4 characters with an underscore or an inner capital letter;
  (d) words followed by `()`. A position is consumed by the first class that matches it.
- **B1 `git grep`**: for each token, `git grep -z -l -w -F -e <token> <parent> -- <supported pathspecs>`
  (case-insensitive extension pathspecs; no checkout). Score = number of distinct tokens matched.
- **B2 BM25 over paths**: one document per universe file, its words from the path (split on `/`,
  `.`, `_`, `-`, and lower-to-upper boundaries). Query = lowercase alphanumeric words. Okapi BM25
  with `k1 = 1.2`, `b = 0.75`, `idf = ln(1 + (N - df + 0.5) / (df + 0.5))`. Files with score 0 are not returned.
- **B3 current `search`**: a fresh `init` and `index` of the extracted parent tree in a scratch
  `RILTRA_HOME`, then `riltra search --repo t --symbol <token> --limit 1000 --offset <n>` for each
  token, advancing the offset until it reaches the reported `total`. Files are collected from the
  pages. Same scoring as B1.
  - Status per task: `ok`, `index-failed` (init or index exit non-zero, or the binary cannot start,
    or the parent tree cannot be extracted; the extraction case has detail `extract`), or
    `search-error` (any search exit non-zero, unparsable JSON, or a page that is empty before the
    reported total). A non-`ok` task is excluded from that baseline's counts and wins and losses,
    and is never recorded as returned zero files. Its row carries the token count. One failing task
    never aborts the run.
  - Scratch root: `run --work` must be a short path at a drive root, outside both repositories
    (for example a folder named `w8` at a drive root). Unless long paths are enabled, Windows limits paths to 260 characters, and a deep tree under a long
    scratch path can fail to extract. The harness removes each task's scratch tree, using the `\\?\`
    prefix on Windows so removal works past that limit. Delete the scratch root after the run.

Selectivity (B1 and B3): a token matching more than 50 files is dropped and counted. The count is
taken on the tool's raw result (supported-language files, tests included), before the universe filter.

- **R lexical concept search (PR 9)**: one `riltra query --repo t --text <query text> --limit 1000`
  call on B3's scratch index, in the same per-task loop (no second index build). Symbols collapse
  to files at their best rank, restricted to the candidate universe. There is one call and no
  paging, so rows past the first 1000 symbols are unreachable and are not measured. The ranker is
  the frozen rule set R0 in `ranker-2026-11.md`. It is lexical only: it follows no call edges.
  - Status per task: `ok`; `index-failed` when B3's index was not built (the same detail as B3);
    `search-error` (non-zero exit, unparsable JSON, or a `reason` other than `no-match` or
    `no-extractable-term`). A failure is never recorded as an empty list.
  - `abstained` = status `ok` with `reason` `no-extractable-term`. Abstentions are counted apart
    from failures. In the pair verdicts an abstention is an `ok` task with found@10 = 0.

**S, text-free size control (PR 9b).** The files of the parent tree that are in the candidate
universe, ranked by size in bytes, largest first, ties by path ascending (`git ls-tree -r -l` on the
parent). S ignores the query and is always `ok`. It is a **control, not a competitor**. It exists
because a repository whose changed files are mostly its largest ones saturates `found@10` for a ranker
that returns big files, so a high `found@10` alone says little. S returns the whole universe, so its
`returned` equals the universe size.

**Mention class M1 (per task, PR 9b; replaces `name_hint`).** Computed from git and the manifest only,
never from Riltra output, over the manifest's `gold` (not `test_gold`), on the clean query text
lowercased. A word character is `[a-z0-9_]`; an occurrence counts only when the characters directly
before and after it are not word characters (or the text edge). Per gold file, the levels in order:

- `path`: the full repository path (lowercased) occurs.
- `name`: the basename with extension occurs.
- `stem`: the basename without extension occurs. The stem must be at least 4 characters and not one of
  `main`, `index`, `init`, `test`, `tests`, `spec`, `specs`, `utils`, `types`, `common`.

Per task: `named` when any gold file is at `path` or `name`; otherwise `stem` when any gold file is at
`stem`; otherwise `none`. `none` means exactly "no mention that M1 accepts". It does not mean "no gold
file is named in the text": M1 ignores short and generic stems on purpose. It is the closest M1 gets to
the plan's ceiling stratum, not that ceiling, and it is a descriptive class, not a bound on what any
ranker can find. Its limits, in both directions:

- (a) a stem that is an ordinary word, such as `store`, counts as `stem` even when the text means
  something else;
- (b) a file whose symbol is named in the text but whose file name is not counts as `none` (the
  `evict_entry` case: a gold `storage.rs` is not named);
- (c) gold `src/index.ts` with the text `index`, and gold `src/lib.rs` with the text `lib`, are `none`:
  the stem is generic or under 4 characters, although the bare file name is given. These two cases are
  kept as named limits, and selftest asserts the class `none` for both;
- (d) M1 is literal: a non-ASCII letter is not a word character, so `éstore` counts as a mention of
  `store`.

The plan's ceiling ("tasks whose gold file is named nowhere in the text") is not measured by M1.

**Output change (PR 9b)**: `run` output gains the `S` block per task and in the summary, the pair
`R-S`, the per-task `mention` class (replacing `name_hint`), the summary `mention` counts, the
`strata` block, and the `hits_at_5`, `first_gold_rank` and `returned_median` fields per baseline. The
run header gains `measure_version` (`pr9b-measure-1`) beside `rules_version`. PR 8 and PR 9 outputs are
therefore not byte-comparable with PR 9b outputs; the values of B1 to B3 and of R are unchanged (checked
against the PR 9 tuning table, see `ranker-2026-11.md`). `rules_version` stays `pr8-rules-1`: the task
rules did not change.

## Metrics and verdicts

Per task and per baseline, counts only: `n` (scored gold files), `found@5`, `found@10` (gold files in
the top k of the ranked list), `first_gold_rank` (null if none returned), `returned` (list length).

Aggregate per baseline, over its `ok` tasks: `hits_at_5` and `hits_at_10` (tasks with at least one gold
file in the top 5 or the top 10); `first_gold_rank` as `{found, none, median, rank1, le3}` (`found`
counts tasks with a rank, `none` the rest, `median` the lower median of those ranks, `rank1` and `le3`
count first ranks of 1 and of at most 3); and `returned_median` (the same lower median over `returned`).
The lower median is element `(n-1)//2` of the ascending sort, and `null` when there is none. The
tuning-task total and failure count are reported beside them.

Win/loss, frozen: for a pair of baselines on one task, the higher `found@10` wins, the lower loses,
equal is a tie. Only tasks where both are `ok` count. The pair `R-S` follows the same rule; no new
verdict rule is added. `found@5` and `first_gold_rank` are reported beside the verdict and never folded
into it. Every number carries its sample size. Samples this small cannot support any threshold.

Strata: the same hit and first-rank block is repeated per baseline for each M1 class (`named`, `stem`,
`none`), with the task count of each class and, per baseline, the `ok` and failed counts, so a failed
task keeps its place in the denominator. Hits are counted over `ok` tasks only. Counts only; the strata
are small and nothing is inferred from them.

## How to rerun

Build the binary (`cargo build --locked`), then, from the repository root, with a scratch folder
outside the repositories:

```
python -I docs/benchmarks/workflow/selftest.py --riltra <riltra-binary>
python -I docs/benchmarks/workflow/harness.py run --split tune --manifest docs/benchmarks/workflow/tasks-riltra.jsonl --repo <repo-root> --riltra <riltra-binary> --work <scratch-dir>
```

`run` writes JSON lines to stdout: a run header (manifest digest, binary digest and `--version`,
split, `rules_version`, `measure_version`), one line per task, and a summary with per-baseline counts and pair
win/loss/tie counts. Two runs are comparable only when their headers match. Output contains no
timings, so two runs of the same input are byte-identical.

`run` refuses (exit 3, with a reason) when the manifest digest does not match its `.sha256` file,
when the manifest `rules_version` differs from the harness, when a short sha does not resolve, or
when `--split heldout` is given without `--final`.

To freeze a new tip: `python -I docs/benchmarks/workflow/harness.py freeze --repo <repo-root> --tip <full sha> --label <label> --out <new manifest path>`.

## Rulings made in this PR

These are choices the brief left open. Each one is part of the frozen rules; changing one means
bumping `RULES_VERSION` and regenerating the manifest.

1. E0 order: one pass, left to right, first appearance; at one position the first class wins.
   Class (d) has no length rule (the brief states the 4-character rule for class (c) only).
2. E0 backtick spans cannot cross a newline.
3. BM25 query terms are the distinct query words; idf uses the `+1` inside the logarithm.
4. Universe: all blob entries of the parent tree that pass the supported and non-test rules
   (symlinks and submodule entries are not filtered by type).
5. `test_gold` lists changed (`M` or `D`) test files of a supported language that exist in the
   parent; added and unsupported test files are not listed. `gold_added_not_findable` counts added
   supported non-test files.
6. Commit messages are read from git by short sha at run time; the trailing newline is removed.
7. Parent trees are extracted with `git archive` and Python `tarfile` (filter `data`, Python 3.12
   or newer), not a shell pipe to `tar`.
8. The B3 index runs on an extracted tree with no `.git` folder. The B3 control in `selftest.py`
   confirms that `index` works there, so no `git init` is run.
9. B3 paging advances the offset by the number of hits the page returned. An empty page before
   the reported total is a `search-error`.
10. Short shas are 12 characters.
11. The second repository's tip is recorded in the local output only.
12. A parent tree that cannot be extracted (for example a path over the Windows 260-character limit when long paths are not enabled) is
    B3 `index-failed` with detail `extract`, counted with the other B3 failures. It is not an
    empty result and does not stop the run. Found by the first tune attempt; the selftest covers it.
13. (PR 9b) M1 compares lowercased gold paths and file names with the lowercased query text, so the
    case of a path in the manifest does not matter.
14. (PR 9b) S sorts the universe by the sizes `git ls-tree -l` prints for the parent. A candidate with
    no numeric blob size stops the run (`HarnessError`); no size is guessed.
15. (PR 9b) Medians are lower medians (element `(n-1)//2`). `returned_median` is taken over `ok` tasks,
    abstentions included (an abstention has `returned` 0).
16. (PR 9b) The mention classes partition the tasks, so the stratum task counts sum to the task count.
17. (PR 9b) `MEASURE_VERSION` is `pr9b-measure-1`. Any change to M1, to the S order or to the summary
    fields needs a new `MEASURE_VERSION`. `RULES_VERSION` is unchanged, because the task rules are.

## Limits

- The Riltra manifest is tied to current shas. A history rewrite (planned, owner-gated) invalidates
  it; regenerate it with the same rules and the same `RULES_VERSION` before reusing any result.
- Small samples: 36 held-out tasks and a tuning split of 81 tasks for Riltra. Counts
  are not rates and no threshold is claimed.
- The held-out split is not representative of the older history (see Split).
- B1 matches whole words (`git grep -w`); B3 is the `search` command's FTS prefix plus substring
  match on symbol names. The same token can therefore match more files in B3 than in B1, and the
  50-file selectivity cut can drop it in one baseline and keep it in the other. B3's hits come
  FTS first, then substring-only, not in path order (paging is still deterministic).
- Python 3.12 or newer is required (`harness.py` exits otherwise). `--out` and `--work` are not
  checked against the repository roots; keep them outside both repositories.
- B2 sees only path words, B1 only exact tokens, and B3 only symbol names. None of them reads
  file content beyond the tokens' presence.
- The selftest covers the rules with a synthetic repository of known answer; it does not
  establish that the real history was read correctly beyond the counts it prints.
- S saturates `found@10` where a repository's changed files are mostly its largest ones. S is a
  control, not a competitor, and a high S value does not say that a ranker is poor.
- M1 classes are descriptive and small in number per class (see Strata); they make no inference.
- No comparison or savings claim is made in this PR or in PR 9b.

## Confidentiality

Tracked files name only this repository. A second local repository appears only as the placeholder
line in `baseline-2026-11.md`, until its aggregates are approved. Its manifest and per-task output
stay outside both repositories.
