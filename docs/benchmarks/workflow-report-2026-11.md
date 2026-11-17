# Workflow verification report, 2026-11

Scope: the four workflow jobs (`impact`, `context`, `changes`, `query`) and the layers under them, checked against oracles that do not depend on Riltra's output. This is a report of what was checked, what was not, and what each check can and cannot support. It makes no comparison claim against another product and no token-savings claim. Every measurement here is report-only: none has a pass threshold, so none can fail.

## Suite and identity

| Field | Value |
|---|---|
| Suite | Task 10 verification report of the GitNexus-replacement plan (plan unit PR 11) |
| Owner approval | the owner's option choices of 2026-11-17 (see "Choices") |
| Repository and commit | this repository at `1120b74` (merge of PR 74); the "before" tree is `cc0c763` |
| Operating systems | CI: Ubuntu, macOS, Windows (run 43845259874 for the test counts; master run 43988507370 for `1120b74`); local measurements: Windows |
| Binaries | after: `riltra 0.2.0-rc.1` built from `1120b74`, sha256 `1baec76595b182fd848ebd9526f3c9321b3dc9ce01be5a58b6b8c3f634905533`; before: built from `cc0c763`, sha256 `44de97eab9110f3cdce7aa45d2546b87bb8b234ffc7d09302731cce4abb5de99`; both debug builds |
| Harness | `docs/benchmarks/workflow/verify.py` and `selftest.py` (Python 3.13, standard library, `python -I`); the Python harness is not run in CI, so its results are local runs |
| Ground-truth origin | hand-written fixtures and manifests; Python `ast`; git's own hunk ranges; the frozen concept-search task set (git history only). Riltra output never built an expectation for a fixture, an oracle or the task set. One check is weaker: the hunk accounting in L3 compares `hunks_outside_definitions` with the hunks that overlap no listed definition, and it uses the definition list that `changes` returned, so it is a consistency check, not an independent oracle of completeness (a definition that was missed can still give a matching result) |

## Summary by layer

| Layer | What | Status |
|---|---|---|
| Controls | set comparison, repeatability | proven locally (selftest, 205 checks) |
| L0 definitions | Python `ast` oracle; hand-written syntax fixtures for 8 languages | proven on the checked sets |
| L1 callers and callees | M6 manifests through synthetic SCIP documents | proven at the synthetic level; real indexers tried for TypeScript and Rust only |
| L2 impact | M6 impact cases; new Go and Java syntax-level fixtures | M6 proven; Go and Java: partial, with named gaps |
| L3 `changes` | 62 tests on three operating systems; replay of 30 commits against git | proven on tests; replay consistent with git's hunk ranges; name comparison inconclusive by construction |
| L4 `query` | frozen task set, one held-out run reused | reused, not rerun |
| `query --expand` (`via`) | implemented and tested | benefit on real repositories not measured |
| GitNexus comparison | none | not done, by decision |

## Controls

- Existing since PR 8 (`selftest.py`): identical sets compare equal; one missing item is reported as one; an empty set never equals a non-empty one; two empty sets are equal.
- Added here: a repeatability control. With one binary and one indexed fixture, `scan` and `impact` each run twice and the outputs are byte-identical. It asserts nothing about content.
- `selftest.py` passes 205 checks with the after binary (192 before the last unit). Without a binary the binary-dependent controls print a skip reason.
- Limit: the harness is not in CI. A run is the operator's run on the day.
- Also observed, not a control: on 2026-11-16 two different builds of the same sources produced identical task and summary lines for the concept-search tune split; only the header's binary hash differed, and the cause of that hash difference was not established.

## L0 definitions

- Python `ast` oracle against `riltra scan`, on every Python file tracked in this repository's working tree at measurement time (27 files, including the edited `selftest.py`; the new `verify.py` was untracked then and is not in the set; 26 scored, 1 deliberately broken fixture skipped for `SyntaxError`): 163 oracle definitions, 163 exact matches, 0 missing, 0 extra, 0 files with a scan status other than ok. The oracle total depends on the harness files in the set: an earlier run, before the last harness unit, gave 158 of 158 with the same result. Decorated definitions are matched on the `def` line or the decorator line and counted apart; none occurred. Scope: this repository's own Python only, small and mostly fixtures and harness code.
- Hand-written syntax fixtures for c, cpp, go, java, js, py, rs, ts, checked by `docs/benchmarks/syntax_bench.py` (existing script, run on the after binary): definitions 82 true positives, imports 15, calls 56, 0 false positives and 0 false negatives in every language. The fixtures are small and were written for this check.
- Older record: the 2026-11-11 language baseline over the M6 sources gives definition recall 35/38 (py, ts), 38/41 (rs), 29/36 (c), 35/38 (cpp). M6 is a relationship corpus, not a syntax ground truth.

## L1 callers and callees

Two levels of evidence, kept apart.

- Synthetic-SCIP level. The M6 test builds a SCIP document from each manifest, imports it, and compares the CALLS count of `callees` and `callers` with `callees_calls` and `inbound_calls`, the REFERENCES count with `inbound_references`, and the `impact` verdict with the manifest, for 79 bundles (c, cpp, py, rs, ts) and 3 impact bundles. These run in `tests/edges.rs` (175 tests, green on three operating systems). They show that Riltra handles well-formed SCIP data correctly. They do not show that a real indexer produces such data. Counts only: the manifests carry no caller names.
- Real-indexer level. A SCIP import from a real indexer was verified for TypeScript (`scip-typescript`) and Rust (`rust-analyzer`), one fixture each. Python, JavaScript, C and C++: import contract only. Go and Java: no SCIP or language-server evidence of any kind.
- Decoy precision: the Task 1 memo of 2026-11-13 recorded that name-candidate `callers` recovered 35 of 35 manifest inbound calls and returned 31 extra edges in 31 decoy cases (35/66). It was measured on `cc0c763` and was not recomputed on `1120b74`.

Test counts of the Rust suite per target and operating system (run 43845259874, all jobs success, 0 failed, 0 ignored):

| target | Ubuntu | macOS | Windows |
|---|---|---|---|
| unit | 612 | 612 | 616 |
| changes | 62 | 62 | 62 |
| cli | 28 | 28 | 27 |
| context_facts | 20 | 20 | 20 |
| edges | 175 | 175 | 175 |
| expand | 16 | 16 | 16 |
| index | 45 | 45 | 45 |
| knowledge | 60 | 60 | 62 |
| mcp | 91 | 91 | 91 |
| semantic | 24 | 24 | 24 |
| syntax | 15 | 15 | 15 |
| workflow | 64 | 64 | 64 |
| sum | 1,212 | 1,212 | 1,217 |

The semantic-knowledge feature jobs run further tests that are not counted here.

## L2 impact

- M6 impact cases: three bundles and the candidate tests, green in CI. After the impact change (PR 2), `syntax_impact` paths changed in 60 golden bundles, no verdict changed, and proven paths were unchanged.
- Go and Java: no relationship corpus exists. Five small cases per language were written from source (direct, recursion, cycle, same-name decoy, cross-file) before any run; the expectation digests are listed in the evidence section. Each case asks which functions call the anchor (direct callers; `impact --depth 1` and `callers` compared with the same truth). One run:

| case | Go: callers found / truth | Go: extra | Java: found / truth | Java: extra |
|---|---|---|---|---|
| direct | 1/1 | 0 | 1/1 | 0 |
| recursion | 2/2 | 0 | 2/2 | 0 |
| cycle | 1/1 | 0 | 1/1 | 0 |
| same_name | 1/1 | 1 (the declared decoy) | 0/1 | 0 |
| cross_file | 0/1 | 0 | 0/1 | 0 |

`impact` and `callers` agree on every case. Every returned path has `basis: candidate-name`; `candidate_scope` is `same-file` in every returned site; the verdict is `UNKNOWN` in every case.
- Reading: within one file the name-matched candidates recover the callers, and a same-name decoy appears as an extra, labelled candidate. Across files nothing is returned for Go or Java, which agrees with the code: file linkage for candidates exists only for Python imports and aliases, Rust crate sources and C/C++ includers. A cross-file caller in Go or Java is therefore a miss, not a "none found".
- Observed and unexplained: in the Java `same_name` fixture, the calls `a.run()` and `b.run()` are extracted by `scan` as `a.run` and `b.run`, yet no candidate edge is returned, while the Go equivalent returns both. The cause was not investigated here and no fixture or expectation was changed.
- Not done: synthetic-SCIP `proven` fixtures for Go and Java (not approved), so nothing here says anything about proven edges in those languages.

## L3 `changes`

- Tests: 62 in `tests/changes.rs` plus MCP tests, green on three operating systems. They cover scopes, rename, binary, CRLF, syntax error, option-like `--base`, a configured textconv driver, and Python, Rust, TypeScript, JavaScript, C, C++, Go and Java.
- Findings recorded at PR 4 and PR 5, as they stand now (read from code and test names; the CI run is the execution evidence):

| Finding | Now |
|---|---|
| Freshness check could run a repository filter driver or hook | closed: `index_freshness_runs_no_repository_hook_and_reports_a_real_status` and its MCP twin |
| A path deleted from the index and recreated listed twice | closed: `a_path_deleted_from_the_index_and_recreated_is_one_entry_or_none` |
| `hunks_outside_definitions` has no per-file cap | still open (the file list is capped by `--limit`) |
| Non-UTF-8 path names | implemented and unit-tested on git's raw output; not exercised through a real file system |
| git 2.31 floor | implemented; only the version parser is unit-tested |
| Symlink and junction branches | blob mode 120000 tested; directory-link tests exist per operating system |
| MCP skip branch, touch-rule start boundary, `content_diff` reading with `std::fs::read`, sidecar files created by read tools | not re-checked here |

- Replay of 30 commits (report-only). Selection rule, fixed in advance: non-merge commits of `1120b74` that have a parent and changed a file of a supported language (197 qualify), sorted by full sha ascending, the first 30. For each commit `changes --scope compare --base <parent>` was run on a clean checkout of that commit, with no index, and compared with git's hunk-header function context, by name.
  - 588 files in all (as `changes` lists them); 1,959 hunks in git's diff over all files of the 30 commits, of which 615 are in files of a supported language (the hunks the name comparison uses); no `changes` failure, no truncated document.
  - Name comparison classes (a hunk-context name matched as a whole word against the definitions `changes` lists): 15 agree, 72 differ, 8 have git context but no Riltra definition, 1 has neither, 14 are whole added files, 478 are files in a language Riltra does not parse (status `unsupported`).
  - This comparison is inconclusive by construction: git's context is the nearest preceding header line of each hunk, while `changes` lists every definition a hunk touches, so a hunk spanning several functions or adding new ones differs without either side being wrong. Counts of "differ" are not an error rate.
  - Independent check with git's own hunk line ranges, on the 96 files with status modified and symbols established: every listed definition (627 of 627) overlaps a hunk range reported by `git diff -U0`; the `hunks` count equals git's count in 96 of 96 files; the number of hunks that overlap no listed definition equals `hunks_outside_definitions` in 96 of 96 files. This checks consistency of ranges and counts, not names, and it cannot show a definition that was touched and not listed unless it leaves a hunk uncounted as outside.
  - Not adjudicated by hand.

## L4 `query`

Four separate statements.

1. Implemented and merged: the lexical ranker R0 (PR 70) and the opt-in edge expansion `--expand` with `via` (PR 74; 16 integration tests and 34 unit tests, green on three operating systems).
2. Evaluated once on held-out data and not rerun: the held-out split of the frozen task set (36 tasks) was run once on 2026-11-16 on `77efc9e` (binary sha256 `7973f262...f898`, output sha256 `e2aea7d2...8b7a`, manifest digest `84598bd7...e88c`). Counts of tasks with at least one gold file in the top 5 and the top 10: R 32 and 36; B1 `git grep` 11 and 15; B2 BM25 over paths 23 and 24; B3 current `search` 7 and 7; S size control (not a competitor) 11 and 24. The tune split (81 tasks): R 73 and 78. These are counts on a small sample, the two splits cover different periods, R returns a median of 32 files against 0, 6 and 0 for B1 to B3, and the single-run authorization is an operator statement. The full table, per-task rows and limits are in `ranker-2026-11-heldout.md`; nothing was recomputed or rerun for this report.
3. Not measured: the benefit of `via` on real repositories. The benchmark repositories carry no SCIP edges, so `via` returns nothing there; the owner accepted this as not measured when the feature was specified.
4. Not measured: the plan's original ceiling (tasks whose gold file is named nowhere in the text). The mention class M1 is a descriptive class, not that ceiling.

## Before and after for `impact` and `callers`

The only contemporaneous "before" is the Task 1 measurement of 2026-11-13 on `cc0c763`. It was repeated now with the `cc0c763` binary on a clean checkout (a retrospective reproduction) and with the after binary on a clean checkout of `1120b74`; commands: `callers` and `impact`, each with `--limit 1000`, no other flags, at the identifier column and at the keyword column of each definition; raw outputs kept locally with digests. The two trees differ, so a change in a count can come from the code under test or from the repository changing; no difference is attributed to one cause.

| anchor | callers edges, before to after | impact paths, before to after | impact verdict |
|---|---|---|---|
| `impact_query` | 1 to 1 | 0 to 1 | UNKNOWN to UNKNOWN |
| `edge_query` | 4 to 4 | 0 to 4 | UNKNOWN to UNKNOWN |
| `trace_query` | 1 to 1 | 0 to 1 | UNKNOWN to UNKNOWN |
| `validated_trace_anchor` | 3 to 3 | 0 to 3 | UNKNOWN to UNKNOWN |
| `called_name` | 0 to 2 | 0 to 2 | UNKNOWN to UNKNOWN |
| `open_live` | 91 to 107 | 0 to 107 | UNKNOWN to UNKNOWN |
| `parse_source` | 42 to 47 | 0 to 47 | UNKNOWN to UNKNOWN |
| `push_def` | 27 to 27 | 0 to 27 | UNKNOWN to UNKNOWN |

- The identifier and keyword columns gave the same counts for every anchor. After the change every `impact` path carries `basis: candidate-name`; none is proven, and no verdict changed. The before-side values for these eight anchors equal the values recorded in the 2026-11-13 memo.
- `new`: the name has four definitions at `cc0c763` and six at `1120b74`. The 2026-11-13 memo recorded 19 caller edges for `new` without saying which definition it used, and that value was not reproduced (636 edges at `cc0c763`); the historical pair for `new` cannot be established, and these rows are retrospective only. Every definition named `new` returns the same count within one tree (636 before, 788 after), because name-matched callers of a common name are merged across its definitions.
- The Task 1 Step 3 questionnaire (12 workflow questions with expected answers) was not found in the plan notes, the benchmark folders, the dogfood reports or the project memory, so that historical measurement is recorded as not found. No retrospective baseline for it was created. `context` existed at `cc0c763`; by-name lookup, `changes`, `query`, `agent-block` and `--expand` did not.

## Usage logs

Descriptive counts over the local dogfood usage logs were computed for the Riltra repository (window 2026-11-11 to before 2026-11-17 UTC, main-agent rows and subagent rows kept apart, a fixed classification of Grep patterns). They stay local by the project's confidentiality rule and are not reproduced in this tracked report. What can be said without them: every logged session used the dogfood build from 2026-11-11 (commit `2e72f16`), which predates by-name lookup, the new impact behaviour, `changes`, `context` sections, `query`, `agent-block` and `--expand`. No logged call can therefore be a call to those tools, there is no "after" data, and none can appear until that build is replaced. Earlier usage figures quoted while the plan was written cannot be reproduced from the current logs and are not used.

## What was not measured

Processes and communities, rename planning, route maps, taint, import cycles; the benefit of `via` on real repositories; the plan's concept-search ceiling; proven edges for Go and Java; the 12-question workflow questionnaire; any comparison with another product; any token or time saving; any second-repository result; GitNexus (not run, by decision).

## Deviations and limits

- The Python harness is not run in CI; one unit (`definition_found`) was written before its failing check was shown, and the CLI glue is covered only by one run each.
- The `callers`/`impact` comparison for Go and Java uses small hand-written files; it measures candidate recall and decoys on those files and says nothing about real code.
- The anchor output pairs the `new` definitions of the two trees by ordinal (`new-1` and so on); these are not identity pairs (the definition list differs between the trees) and the report does not use them.
- Known latent limits of the harness code, not triggered in these runs: the replay selection reads paths from `git log` without `-z`, so a supported path with unusual characters would be dropped; a malformed `changes` or `impact` document raises an exception (exit 1) instead of the documented exit 3; the definition regex for anchors accepts spaces but not tabs before `fn`; the `pyast` mismatch lists would carry identifier names.
- Debug builds were used for the local measurements. Timings were not measured.
- Two builds of the same sources gave different binary hashes on 2026-11-16 (cause not established); the numbers did not differ.

## Choices recorded

Approved by the owner on 2026-11-17: the 30-commit replay; the nine-anchor re-measurement; the Python `ast` oracle; syntax-level Go and Java fixtures; counting the old usage logs as "before"; Riltra repository only; dated notes in the benchmark README and ranker record where they said Task 8 was incomplete; `via` benefit left unmeasured. Not approved and therefore not done: a retrospective baseline for the 12 questions, synthetic-SCIP Go and Java `proven` fixtures, second-repository aggregates, the GitNexus step, any change to the dogfood build.

## Evidence pointers

- Go and Java expectation files (frozen before the run), sha256: go/cross_file `10a93734...`, go/cycle `ed9f80d1...`, go/direct `bef886b4...`, go/recursion `148da301...`, go/same_name `1ca8ab06...`, java/cross_file `3e838d94...`, java/cycle `0c2c3da9...`, java/direct `50c34077...`, java/recursion `fdcd292c...`, java/same_name `129a9d39...` (full digests: `fixtures/` listing at the commit of this report).
- Replay: 30 short shas in the header of the replay output (first `00029b3a18b6`, last `2dd9f48c0ca0`); the 30 raw `changes` documents and the 262 anchor output files are kept locally, each set with its own SHA256SUMS file (written once; the replay list was written after the review, with all 60 files verifying).
- CI: run 43845259874 (branch head `98d56ea`) and run 43988507370 (master `1120b74`), all jobs success.
- Held-out record: `ranker-2026-11-heldout.md` (merged), unchanged.
- Verdict within the tested scope: the checks above support correctness of definitions and relationship counts on the sets named, the consistency of `changes` with git's hunk ranges on the 96 files checked, and the reuse of the held-out result as recorded. They do not support any claim about real-indexer quality outside TypeScript and Rust, cross-file candidates in Go and Java, or the benefit of `via`.
