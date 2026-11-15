# Workflow query task set and baselines (PR 8)

This folder holds a frozen set of concept-search tasks built from git history alone, the
Python 3 standard-library harness that builds and measures it, and the three baselines measured
on its tuning split. Nothing here changes Riltra's Rust code, schema, parser, commands or MCP
tools. PR 9 (the ranker) is the first consumer; it is the only one allowed to use the held-out
split.

Files:

- `harness.py`: `freeze` (builds a task manifest from git history) and `run` (measures B1, B2, B3).
- `selftest.py`: controls. Run before any measurement; it exits non-zero on the first failure.
- `tasks-riltra.jsonl` and `tasks-riltra.sha256`: the frozen Riltra manifest (header line, then one line per task).
- `baseline-2026-11.md`: the tuning-split results for the Riltra repository, with limits.

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

**Held-out is never run in this PR.** `run --split heldout` is refused unless `--final` is passed,
and PR 8 does not pass it. Only the held-out task count is reported. Reason: with only 36
held-out Riltra tasks (the plan estimated about a dozen), per-task results would steer the ranker, and the baselines have no
tunable parameter, so PR 9 loses nothing by running them once with the ranker.

**Expected distribution shift.** The newest tasks are mostly this plan's own PRs, so the held-out
split is not representative of the older history. The tuning split is the older part.

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

**name_hint (per task, descriptive heuristic)**: true when a gold path, or its lowercase file stem,
occurs as a substring of the lowercase query text. It uses git and the manifest only, never Riltra
output. It is **not a ceiling and not a lower or upper bound**, and it errs in both directions: a
gold `storage.rs` with the query `evict_entry` is false, although the symbol name is a direct hint;
the stem `main` matches inside `maintain`, although no file `main` is named. The plan's ceiling ("tasks whose gold file is named nowhere in the text") is not measured; the measurement is not finished. `name_hint` is only a descriptive heuristic; the definition and measurement of how a text mentions its gold files will be handled separately.

**Output change (PR 9)**: `run` output gains the `R` block per task, the `R` summary (with
`abstained`), three pairs (`R-B1`, `R-B2`, `R-B3`), and the `name_hint` true/false counts. PR 8
outputs are therefore not byte-comparable with PR 9 outputs. `rules_version` stays `pr8-rules-1`:
the task rules did not change.

## Metrics and verdicts

Per task and per baseline, counts only: `n` (scored gold files), `found@5`, `found@10` (gold files in
the top k of the ranked list), `first_gold_rank` (null if none returned), `returned` (list length).

Aggregate per baseline: tasks with at least one gold file in the top 10, out of the tasks with status
`ok` for that baseline. Tuning-task total and failure count are reported beside it.

Win/loss, frozen: for a pair of baselines on one task, the higher `found@10` wins, the lower loses,
equal is a tie. Only tasks where both are `ok` count. `found@5` and `first_gold_rank` are reported
beside the verdict and never folded into it. Every number carries its sample size. Samples this
small cannot support any threshold.

## How to rerun

Build the binary (`cargo build --locked`), then, from the repository root, with a scratch folder
outside the repositories:

```
python -I docs/benchmarks/workflow/selftest.py --riltra <riltra-binary>
python -I docs/benchmarks/workflow/harness.py run --split tune --manifest docs/benchmarks/workflow/tasks-riltra.jsonl --repo <repo-root> --riltra <riltra-binary> --work <scratch-dir>
```

`run` writes JSON lines to stdout: a run header (manifest digest, binary digest and `--version`,
split, `rules_version`), one line per task, and a summary with per-baseline counts and pair
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
- No comparison or savings claim is made in this PR.

## Confidentiality

Tracked files name only this repository. A second local repository appears only as the placeholder
line in `baseline-2026-11.md`, until its aggregates are approved. Its manifest and per-task output
stay outside both repositories.
