# SPDX-License-Identifier: Apache-2.0
"""Controls for harness.py. Runs before any measurement; exits non-zero on the first failure.

Usage: python -I selftest.py [--riltra <path to a riltra binary>]
Without --riltra the B3 control is skipped and the skip reason is printed.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import harness as H  # noqa: E402

PASSED = 0


def check(condition, label):
    global PASSED
    if not condition:
        raise SystemExit("SELFTEST FAILED: " + label)
    PASSED += 1


GIT_OPTS = [
    "-c", "user.name=selftest",
    "-c", "user.email=selftest@example.invalid",
    "-c", "commit.gpgsign=false",
    "-c", "core.autocrlf=false",
]


def git_run(repo, args, ts=None):
    env = dict(os.environ)
    if ts is not None:
        env["GIT_AUTHOR_DATE"] = "%d +0000" % (1600000000 + ts)
        env["GIT_COMMITTER_DATE"] = "%d +0000" % (1600000000 + ts)
    proc = subprocess.run(["git", "-C", repo] + GIT_OPTS + args, capture_output=True, env=env)
    if proc.returncode != 0:
        raise SystemExit("SELFTEST FAILED: fixture git command failed: " + args[0])
    return proc.stdout.decode("utf-8").strip()


def write_files(repo, files):
    for rel, text in files.items():
        full = os.path.join(repo, *rel.split("/"))
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(text)


def commit(repo, message, ts):
    git_run(repo, ["add", "-A"])
    git_run(repo, ["commit", "-q", "-m", message], ts=ts)
    return git_run(repo, ["rev-parse", "HEAD"])


def short(sha):
    return sha[:H.SHORT]


def build_synthetic(root):
    """Synthetic history with a known answer (expected values are written in the tests).

    C1 root, 13 files (step 2)         C6 docs only (step 3)
    C2 M base/core.py (kept)            C7 rename base/util.py -> base/helpers.py (kept)
    C3 M base/util.py on side (kept)    C8 M tests/test_core.py only (step 4)
    C4 merge of side (merge count)      C9 A base/newmod.py only (step 4)
    C5 M nine .py files (step 2)        C10 M base/core.py + tests/test_core.py (kept)
    """
    repo = os.path.join(root, "synthetic")
    os.makedirs(repo)
    git_run(repo, ["init", "-q", "-b", "main"])
    sha = {}
    files = {
        "base/core.py": "def core_parse():\n    return 1\n",
        "base/util.py": "def util():\n    return 1\n",
        "docs/guide.md": "guide\n",
        "tests/test_core.py": "def test_core():\n    pass\n",
    }
    for i in range(1, 10):
        files["many/m%d.py" % i] = "M = %d\n" % i
    write_files(repo, files)
    sha["C1"] = commit(repo, "Initial layout", 1)

    write_files(repo, {"base/core.py": "def core_parse():\n    return 2\n"})
    sha["C2"] = commit(repo, "Adjust `core_parse` helper\n\nSigned-off-by: Selftest <selftest@example.invalid>\n", 2)

    git_run(repo, ["checkout", "-q", "-b", "side", sha["C1"]])
    write_files(repo, {"base/util.py": "def util():\n    return 2\n"})
    sha["C3"] = commit(repo, "Side change to util", 3)

    git_run(repo, ["checkout", "-q", "main"])
    git_run(repo, ["merge", "--no-ff", "-q", "side", "-m", "Merge side"], ts=4)
    sha["C4"] = git_run(repo, ["rev-parse", "HEAD"])

    write_files(repo, {"many/m%d.py" % i: "M = %d\n" % (i + 100) for i in range(1, 10)})
    sha["C5"] = commit(repo, "Nine files", 5)

    write_files(repo, {"docs/guide.md": "guide v2\n"})
    sha["C6"] = commit(repo, "Docs only", 6)

    git_run(repo, ["mv", "base/util.py", "base/helpers.py"])
    sha["C7"] = commit(repo, "Rename util", 7)

    write_files(repo, {"tests/test_core.py": "def test_core():\n    assert True\n"})
    sha["C8"] = commit(repo, "Test only", 8)

    write_files(repo, {"base/newmod.py": "NEW = 1\n"})
    sha["C9"] = commit(repo, "Added only", 9)

    write_files(repo, {
        "base/core.py": "def core_parse():\n    return 3\n",
        "tests/test_core.py": "def test_core():\n    assert False\n",
    })
    sha["C10"] = commit(repo, "Mixed change", 10)
    return repo, sha


def build_widget(root):
    """One file with 1100 matching definitions (sorted first), one gold file sorted after it."""
    repo = os.path.join(root, "widget")
    os.makedirs(repo)
    git_run(repo, ["init", "-q", "-b", "main"])
    big = "".join("def widget_%04d():\n    pass\n\n" % i for i in range(1100))
    write_files(repo, {"a_big.py": big, "b_gold.py": "def widget_gold():\n    pass\n"})
    return repo, commit(repo, "Widget fixture", 1)


def test_set_mismatch():
    check(H.set_mismatch(["a", "b"], ["b", "a"]) == 0, "identical sets compare equal")
    check(H.set_mismatch(["a", "b"], ["a"]) == 1, "one missing item is reported as one")
    check(H.set_mismatch([], ["a"]) != 0, "empty never equals non-empty")
    check(H.set_mismatch([], []) == 0, "two empty sets compare equal")


def test_rule_tables():
    expected = "py js mjs cjs ts mts cts tsx rs go java c h hh hpp hxx inl cc cpp cxx".split()
    check(len(H.SUPPORTED_EXTS) > 0, "extension list is empty")
    check(sorted(H.SUPPORTED_EXTS) == sorted(expected), "extension list differs from classify")
    check(H.is_supported("x/y.H"), "classify lowercases the extension")
    check(not H.is_supported("k.cu") and not H.is_supported("z.md") and not H.is_supported("Makefile"),
          "CUDA, markdown and extensionless paths are not supported")

    positives = [
        "tests/a.py", "src/test/x.rs", "a/__tests__/b.ts", "pkg/spec/c.rb", "test_lib.py",
        "pkg/test_x.py", "pkg/a_test.go", "web/x.test.ts", "web/x.spec.ts", "src/FooTest.java",
    ]
    near_misses = [
        "contest.py", "latest/foo.py", "src/spectrum/a.py", "test.py", "src/Foo_test.java",
        "x.test.js", "web/x.tsx", "pkg/a_spec.rb",
    ]
    check(len(positives) == 10 and len(near_misses) == 8, "is_test_path case counts")
    for path in positives:
        check(H.is_test_path(path), "is_test_path positive: " + path)
    for path in near_misses:
        check(not H.is_test_path(path), "is_test_path near miss: " + path)


def test_path_words_and_bm25():
    check(H.path_words("src/StoreRead.rs") == ["src", "store", "read", "rs"], "path words split on lower-to-upper")
    check(H.path_words("tests/test_core.py") == ["tests", "test", "core", "py"], "path words split on _ and .")

    # Three-document corpus, computed by hand (k1 = 1.2, b = 0.75, query = "query"):
    #   A = "a/query_one.py" -> words a, query, one, py      (length 4)
    #   B = "b/query.py"    -> words b, query, py           (length 3)
    #   C = "c/other.md"    -> words c, other, md           (length 3, no match)
    #   N = 3, avgdl = (4 + 3 + 3) / 3 = 10/3, df(query) = 2
    #   idf = ln(1 + (3 - 2 + 0.5) / (2 + 0.5)) = ln(1.6) = 0.4700036
    #   A: f = 1, dl = 4: denominator 1 + 1.2 * (0.25 + 0.75 * 4 / (10/3)) = 2.38
    #      tf = 1 * 2.2 / 2.38 = 0.9243697; score = 0.4700036 * 0.9243697 = 0.4344571
    #   B: f = 1, dl = 3: denominator 1 + 1.2 * (0.25 + 0.75 * 3 / (10/3)) = 2.11
    #      tf = 1 * 2.2 / 2.11 = 1.0426540; score = 0.4700036 * 1.0426540 = 0.4900512
    #   C scores 0 and is not returned. Ranking: B, then A.
    ranked = H.bm25_rank(["a/query_one.py", "b/query.py", "c/other.md"], "query")
    check([p for p, _ in ranked] == ["b/query.py", "a/query_one.py"], "BM25 ranking on the three-document corpus")
    check(abs(ranked[0][1] - 0.4900512) < 1e-6, "BM25 score of b/query.py")
    check(abs(ranked[1][1] - 0.4344571) < 1e-6, "BM25 score of a/query_one.py")
    check(H.bm25_rank(["a/query_one.py", "b/query.py", "c/other.md"], "query") == ranked,
          "BM25 is repeatable on the same input")


def test_extract_tokens():
    # Five hand-written texts; each lists its planted positives and near misses.
    cases = [
        # (a) backticks, (b) path word, (c) identifier; "Fix", "in", "add" are plain words
        ("Fix `git grep` fallback in store.rs and add test_path_classifier coverage.",
         ["git grep", "store.rs", "test_path_classifier"]),
        # (c) with () and inner capital, (d) a short word followed by (), path word;
        # near miss "Parser" (capital only at start), "a.rs.bak" (not a supported ending)
        ("Call parse_query() then ParseQuery. Plain words: Parser, search, fn(). Also src/main.go and a.rs.bak",
         ["parse_query", "ParseQuery", "fn", "src/main.go"]),
        # near misses only: "go." and "contest"/"spectrum" are not tokens
        ("The contest is latest, a test of spectrum and go.", []),
        # backtick spans are stripped; a one-character backtick span is still class (a)
        ("Use `riltra search --symbol` and `X` then `  spaced  ` please.",
         ["riltra search --symbol", "X", "spaced"]),
        # class (a) near misses: an empty span and an unmatched backtick yield nothing
        ("An unmatched ` tick here.", []),
        ("An empty `` span here.", []),
        # duplicates removed, first 20 kept
        ("alpha_01 alpha_01 " + " ".join("alpha_%02d" % i for i in range(2, 26)),
         ["alpha_%02d" % i for i in range(1, 21)]),
    ]
    for text, expected in cases:
        got = H.extract_tokens(text)
        check(H.set_mismatch(got, expected) == 0 and got == expected,
              "E0 tokens for: " + text[:40] + " -> " + repr(got))


def test_clean_query():
    body = "Subject line\n\nBody text\nSigned-off-by: A <a@x.invalid>\nCo-Authored-By: B <b@x.invalid>\n"
    check(H.clean_query(body) == "Subject line\n\nBody text", "clean_query removes only the two trailer lines")


def test_freeze_and_rules(root):
    repo, sha = build_synthetic(root)
    out1 = os.path.join(root, "m1.jsonl")
    summary = H.freeze(repo, sha["C10"], "synthetic", out1)
    check(summary["kept"] == 4 and summary["tune"] == 2 and summary["heldout"] == 2, "freeze summary counts")

    header, rows, digest = H.load_manifest(out1)
    check(header["excluded"] == {"merges": 1, "over_max_files": 2, "no_supported_file": 1, "no_gold": 2},
          "exclusion counts from construction")
    check(header["non_merge"] == 9 and header["kept"] == 4, "non-merge total and kept count")
    check(header["tune"] == 2 and header["heldout"] == 2, "split sizes: (7*4)//10 = 2 tune")
    check(header["tip"] == sha["C10"] and header["rules_version"] == H.RULES_VERSION, "header tip and rules")

    expected = [
        (short(sha["C2"]), short(sha["C1"]), "tune", ["base/core.py"], [], 0),
        (short(sha["C3"]), short(sha["C1"]), "tune", ["base/util.py"], [], 0),
        (short(sha["C7"]), short(sha["C6"]), "heldout", ["base/util.py"], [], 1),
        (short(sha["C10"]), short(sha["C9"]), "heldout", ["base/core.py"], ["tests/test_core.py"], 0),
    ]
    got = [(r["sha"], r["parent"], r["split"], r["gold"], r["test_gold"], r["gold_added_not_findable"])
           for r in rows]
    check(got == expected, "task rows: shas, gold, test_gold and added-file count from construction")
    for row, want in zip(got, expected):
        check(H.set_mismatch(row[3], want[3]) == 0, "gold set equal for " + row[0])

    # Same tip, same bytes; a commit added after the tip does not move the manifest.
    out2 = os.path.join(root, "m2.jsonl")
    H.freeze(repo, sha["C10"], "synthetic", out2)
    with open(out1, "rb") as a, open(out2, "rb") as b:
        check(a.read() == b.read(), "two freezes of the same tip are byte-identical")
    write_files(repo, {"docs/guide.md": "guide v3\n"})
    commit(repo, "After the tip", 11)
    out3 = os.path.join(root, "m3.jsonl")
    H.freeze(repo, sha["C10"], "synthetic", out3)
    with open(out1, "rb") as a, open(out3, "rb") as b:
        check(a.read() == b.read(), "a commit after the tip does not change the manifest")

    refused = False
    try:
        H.freeze(repo, sha["C10"], "synthetic", out1)
    except H.HarnessError:
        refused = True
    check(refused, "freeze refuses to overwrite an existing manifest")

    tampered = os.path.join(root, "m4.jsonl")
    with open(out1, "rb") as src:
        data = src.read()
    with open(tampered, "wb") as dst:
        dst.write(data.replace(b"base/util.py", b"base/utjl.py", 1))
    with open(os.path.splitext(out1)[0] + ".sha256", "rb") as src, \
            open(os.path.splitext(tampered)[0] + ".sha256", "wb") as dst:
        dst.write(src.read())
    refused = False
    try:
        H.load_manifest(tampered)
    except H.HarnessError:
        refused = True
    check(refused, "run refuses a manifest whose digest does not match")

    stale = os.path.join(root, "m5.jsonl")
    lines = data.decode("utf-8").split("\n")
    lines[0] = lines[0].replace(H.RULES_VERSION, "pr8-rules-0")
    body = "\n".join(lines).encode("utf-8")
    with open(stale, "wb") as dst:
        dst.write(body)
    with open(os.path.splitext(stale)[0] + ".sha256", "w", encoding="utf-8", newline="\n") as dst:
        dst.write("%s  m5.jsonl\n" % H.sha256_hex(body))
    refused = False
    try:
        H.load_manifest(stale)
    except H.HarnessError:
        refused = True
    check(refused, "run refuses a manifest whose rules_version differs from the harness")

    refused = False
    try:
        H.run_split(out1, "heldout", False, repo, os.path.join(root, "no-binary"), os.path.join(root, "w"))
    except H.HarnessError:
        refused = True
    check(refused, "run refuses --split heldout without --final")
    return repo, sha


def test_measurements(root, repo, sha):
    # B1 on the parent tree: token "core_parse" is in base/core.py only; the .md pathspec excludes docs.
    sets = H.b1_collect(repo, sha["C1"], ["core_parse", "guide"])
    check(sets["core_parse"] == {"base/core.py"}, "B1 git grep finds the token in the parent tree")
    check(sets["guide"] == set(), "B1 pathspecs exclude markdown files")

    universe = H.universe_of(repo, sha["C1"])
    check(universe == ["base/core.py", "base/util.py"] + ["many/m%d.py" % i for i in range(1, 10)],
          "universe: supported non-test files of the parent tree")

    row = {"sha": short(sha["C2"]), "parent": short(sha["C1"]), "split": "tune",
           "gold": ["base/core.py"], "test_gold": [], "gold_added_not_findable": 0}
    work = os.path.join(root, "work-bogus")
    os.makedirs(work)
    first = H.measure_task(repo, os.path.join(root, "no-such-riltra.exe"), work, row)
    second = H.measure_task(repo, os.path.join(root, "no-such-riltra.exe"), work, row)
    check(first == second, "two measurements of the same input are identical")
    check(first["B1"]["status"] == "ok" and first["B1"]["found10"] == 1 and first["B1"]["first_gold_rank"] == 1,
          "B1 ranks the gold file first")
    check(first["B2"]["status"] == "ok" and first["B2"]["found10"] == 1 and first["B2"]["first_gold_rank"] == 1,
          "B2 ranks the gold file first")
    check(first["B3"]["status"] == "index-failed" and "returned" not in first["B3"],
          "B3 with a bogus binary is index-failed, not an empty list")
    # M1 on C2: the text has core_parse; the file core.py and the stem core are not counted (word char after).
    check(first["mention"] == "none" and "name_hint" not in first, "M1 class of the C2 task is none; name_hint absent")


def test_s_control(root, repo, sha):
    """S ignores the query: two commits with one parent and one gold file get the same S block."""
    work = os.path.join(root, "work-s")
    os.makedirs(work)
    rows = [{"sha": short(sha[name]), "parent": short(sha["C1"]), "split": "tune",
             "gold": ["base/util.py"], "test_gold": [], "gold_added_not_findable": 0}
            for name in ("C2", "C3")]
    first = H.measure_task(repo, os.path.join(root, "no-such-riltra.exe"), work, rows[0])
    second = H.measure_task(repo, os.path.join(root, "no-such-riltra.exe"), work, rows[1])
    check(first["S"] == second["S"], "S block is the same for two commits with one parent and one gold file")
    check(first["S"]["status"] == "ok" and first["S"]["returned"] == len(H.universe_of(repo, sha["C1"])),
          "S is always ok and returns the whole candidate universe")


def test_run_header(root):
    """The run header carries measure_version beside rules_version; the summary closes the output."""
    dummy = os.path.join(root, "dummy-riltra.bin")
    with open(dummy, "wb") as handle:
        handle.write(b"fixture file; the version call is mocked and the binary is never started")
    manifest = os.path.join(root, "m1.jsonl")
    repo = os.path.join(root, "synthetic")
    with mock.patch.object(H, "riltra_version", return_value="riltra fixture"):
        lines = H.run_split(manifest, "tune", False, repo, dummy, os.path.join(root, "w-header"))
    head = json.loads(lines[0])["run"]
    check(head["measure_version"] == H.MEASURE_VERSION == "pr9b-measure-1", "run header carries MEASURE_VERSION")
    check(head["rules_version"] == "pr8-rules-1" and head["tasks"] == 2, "run header rules_version and task count")
    check(list(json.loads(lines[-1]).keys()) == ["summary"], "the summary is the last output line")


def test_win_loss():
    gold = ["g1", "g2"]
    b1 = ["x1", "x2", "x3", "g1", "x4", "x5", "x6", "x7", "g2", "x8"]   # ranks 4 and 9
    b2 = ["g1"] + ["y%d" % i for i in range(1, 19)] + ["g2"]            # rank 1 and rank 20
    m1 = H.metrics(b1, gold)
    m2 = H.metrics(b2, gold)
    check(m1["found10"] == 2 and m1["first_gold_rank"] == 4, "metrics: B1 two gold in top 10")
    check(m2["found10"] == 1 and m2["first_gold_rank"] == 1, "metrics: B2 one gold in top 10")
    row = {"B1": dict(m1, status="ok"), "B2": dict(m2, status="ok")}
    check(H.compare_pair(row, "B1", "B2") == "B1", "win/loss: higher found@10 wins (B1 on the hand pair)")
    check(H.compare_pair(row, "B2", "B1") == "B1", "win/loss is symmetric in the argument order")
    row["B2"] = {"status": "index-failed", "detail": "x"}
    check(H.compare_pair(row, "B1", "B2") is None, "a non-ok task is not part of a pair")


def test_b3_extract_failure(root):
    """An extraction error is B3 index-failed with detail extract, whatever the machine's path limit is."""
    def boom(repo, rev, dest):
        os.makedirs(dest, exist_ok=True)  # a half-written tree, as a real failure leaves
        raise OSError("simulated extraction failure")

    work = os.path.join(root, "work-extract")
    with mock.patch.object(H, "extract_tree", boom):
        status, detail, sets = H.b3_collect(os.path.join(root, "no-such-riltra.exe"), root, "0" * 40, ["X"], work)
    check(status == "index-failed" and detail == "extract" and sets is None,
          "B3 extraction failure is index-failed with detail extract, not a crash or an empty list")
    check(not os.path.exists(work), "B3 removes its scratch tree even after an extraction failure")


def test_b3_control(riltra, root):
    if riltra is None:
        print("B3 control: SKIPPED, no --riltra given (B3 paths not exercised by this run)")
        return
    repo, sha = build_widget(root)
    work = os.path.join(root, "work-widget")
    status, _, sets = H.b3_collect(riltra, repo, sha, ["widget"], work)
    check(status == "ok", "B3 control: index and search succeed on the fixture")
    check("b_gold.py" in sets["widget"] and len(sets["widget"]) == 2,
          "B3 pagination: gold on the second page (after 1000 hits) is found")
    status, _, sets = H.b3_collect(riltra, repo, sha, ["zzqqnomatchzz"], work)
    check(status == "ok" and sets["zzqqnomatchzz"] == set(), "B3 token with no match is ok with no files")


def test_mention():
    """M1 (mention_class): named, stem or none, from hand-built (gold, text, class) cases."""
    check(not hasattr(H, "name_hint"), "name_hint is removed; M1 replaces it")
    cases = [
        (["src/store/lexical_ranker.rs"], "see src/store/lexical_ranker.rs now", "named"),   # path
        (["src/store/lexical_ranker.rs"], "fix lexical_ranker.rs now", "named"),            # name
        (["src/store/lexical_ranker.rs"], "fix lexical_ranker now", "stem"),                # stem
        (["src/store/lexical_ranker.rs"], "nothing relevant here", "none"),
        (["src/store/lexical_ranker.rs"], "xlexical_ranker.rs", "none"),                    # word char before
        (["src/main.rs"], "maintain the cache", "none"),                                    # stoplisted stem
        (["src/cache.rs"], "cached values", "none"),                                        # word char after
        (["src/lexical.rs"], "lexical_ranker", "none"),                                     # underscore is a word char
        (["src/lexical.rs"], "lexical::select", "stem"),                                    # colon is not a word char
        (["src/mod.rs"], "mod", "none"),                                                    # stem under 4 characters
        (["src/index.ts"], "index", "none"),                                                # limit control 1
        (["src/lib.rs"], "lib", "none"),                                                    # limit control 2
        (["src/data.rs"], "use data here", "stem"),                                         # exactly 4 characters
        (["src/Lexical_Ranker.RS"], "Fix LEXICAL_RANKER.RS", "named"),                     # uppercase text and gold
        (["src/lexical_ranker.rs"], "Use LEXICAL_RANKER now", "stem"),
        (["src/store.rs"], "(store)", "stem"),                                              # punctuation boundary
        (["src/store.rs"], "store.", "stem"),
        (["src/store.rs"], "restore", "none"),
        (["src/zz_one.rs", "src/store.rs"], "store", "stem"),                               # over the gold files
        (["src/store.rs", "src/a/zz_name.rs"], "zz_name.rs and store", "named"),           # named beats stem
    ]
    for gold, text, expected in cases:
        check(H.mention_class(gold, text) == expected,
              "M1 class for %r in %r is %s" % (gold, text, expected))
    # Limit controls (named limits of M1, kept as selftest controls): the bare word is not counted.
    check(H.mention_class(["src/index.ts"], "index") == "none", "M1 limit control: index.ts with the word index is none")
    check(H.mention_class(["src/lib.rs"], "lib") == "none", "M1 limit control: lib.rs with the word lib is none")


def test_summary_arithmetic():
    check(H.median_lower([1, 3, 5]) == 3 and H.median_lower([5]) == 5, "median of odd counts")
    check(H.median_lower([1, 2, 3, 4]) == 2 and H.median_lower([1, 6]) == 1, "lower median of even counts")
    check(H.median_lower([]) is None, "median of no values is None")
    check(H.first_rank_summary([1, 3, 4, None], 4) == {"found": 3, "none": 1, "median": 3, "rank1": 1, "le3": 2},
          "first_gold_rank block: rank1 and le3 boundaries")
    check(H.first_rank_summary([None], 1) == {"found": 0, "none": 1, "median": None, "rank1": 0, "le3": 0},
          "first_gold_rank block with no rank is none and median None")

    gold = ["g"]

    def fake_row(mention, ranked, failed=()):
        row = {"mention": mention}
        for name in H.BASELINES:
            if name in failed:
                row[name] = {"status": "index-failed", "detail": "fixture"}
            else:
                row[name] = dict(H.metrics(ranked, gold), status="ok")
        return row

    rows = [
        fake_row("named", ["g"]),                                              # rank 1, returned 1
        fake_row("stem", ["x1", "x2", "x3", "x4", "x5", "g"], failed=("B2",)),  # rank 6, returned 6
        fake_row("none", []),                                                  # no gold, returned 0
        fake_row("none", ["x%d" % i for i in range(12)]),                      # no gold, returned 12
        fake_row("stem", ["x1", "x2", "x3", "x4", "x5", "g"]),                 # rank 6, returned 6
    ]
    s = H.summarize(rows)
    check(s["tasks"] == 5, "summary task count")
    check(s["mention"] == {"named": 1, "stem": 2, "none": 2}, "mention class counts")
    check(s["B1"]["hits_at_5"] == 1 and s["B1"]["hits_at_10"] == 3, "B1 hits at 5 and 10 from fixture ranks")
    check(s["B1"]["first_gold_rank"] == {"found": 3, "none": 2, "median": 6, "rank1": 1, "le3": 1},
          "B1 first_gold_rank summary from fixture ranks")
    check(s["B1"]["returned_median"] == 6, "B1 returned median over five ok tasks")
    check(s["B2"]["ok"] == 4 and s["B2"]["failed"] == 1, "B2 ok and failed counts with one failed task")
    check(s["B2"]["hits_at_5"] == 1 and s["B2"]["hits_at_10"] == 2, "B2 hits over ok tasks only")
    check(s["B2"]["first_gold_rank"] == {"found": 2, "none": 2, "median": 1, "rank1": 1, "le3": 1},
          "B2 first_gold_rank summary over ok tasks")
    check(s["strata"]["stem"]["tasks"] == 2 and s["strata"]["stem"]["B2"]["ok"] == 1
          and s["strata"]["stem"]["B2"]["failed"] == 1, "stratum counts carry the failed task")
    check(s["strata"]["none"]["B1"]["first_gold_rank"] == {"found": 0, "none": 2, "median": None, "rank1": 0, "le3": 0},
          "stratum none has no first gold rank")
    check(sum(s["strata"][cls]["tasks"] for cls in H.MENTION_CLASSES) == s["tasks"],
          "strata task counts sum to the task count")
    for name in H.BASELINES:
        check(s[name]["hits_at_5"] <= s[name]["hits_at_10"], "hits_at_5 <= hits_at_10 for " + name)


def test_size_order(root):
    """Control S order on a hand-built tree: size descending, a tie by path ascending, markdown out."""
    repo = os.path.join(root, "sizes")
    os.makedirs(repo)
    git_run(repo, ["init", "-q", "-b", "main"])
    write_files(repo, {"big/a.py": "x" * 100, "small/b.py": "x" * 10,
                       "tie/y.py": "x" * 50, "tie/x.py": "x" * 50, "docs/z.md": "x" * 500})
    rev = commit(repo, "Sizes", 1)
    universe = H.universe_of(repo, rev)
    check(universe == ["big/a.py", "small/b.py", "tie/x.py", "tie/y.py"], "size fixture: universe excludes markdown")
    check(H.size_ranking(repo, rev, universe) == ["big/a.py", "tie/x.py", "tie/y.py", "small/b.py"],
          "S order: size descending, ties by path ascending")


def test_r_win_loss():
    gold = ["g1", "g2"]
    b1 = ["x1", "x2", "x3", "g1", "x4", "x5", "x6", "x7", "g2", "x8"]   # found@10 = 2
    r_low = ["g1"] + ["y%d" % i for i in range(1, 10)] + ["g2"]         # found@10 = 1 (g2 at rank 11)
    r_high = ["g1", "g2"] + ["y%d" % i for i in range(1, 9)]             # found@10 = 2
    m1 = H.metrics(b1, gold)
    row = {"B1": dict(m1, status="ok"), "R": dict(H.metrics(r_low, gold), status="ok")}
    check(H.compare_pair(row, "R", "B1") == "B1", "R-B1 pair: the baseline with higher found@10 wins")
    row["R"] = dict(H.metrics(r_high, gold), status="ok")
    check(H.compare_pair(row, "R", "B1") == "tie", "R-B1 pair: equal found@10 is a tie")
    row["R"] = {"status": "search-error", "detail": "x"}
    check(H.compare_pair(row, "R", "B1") is None, "R-B1 pair: a failed R task is not part of the pair")
    rows = [dict(row, B2={"status": "index-failed", "detail": "x"}, B3={"status": "index-failed", "detail": "x"},
                 S=dict(H.metrics(["s1"], gold), status="ok"), mention="none")]
    rows[0]["R"] = dict(H.metrics(r_high, gold), status="ok", abstained=False)
    summary = H.summarize(rows)
    check("R-B1" in summary["pairs"] and "R-B2" in summary["pairs"] and "R-B3" in summary["pairs"],
          "summary carries the three R pairs")
    check(summary["R"]["ok"] == 1 and summary["R"]["failed"] == 0, "summary counts R ok and failed")


def test_r_bogus_binary(root, repo, sha):
    row = {"sha": short(sha["C2"]), "parent": short(sha["C1"]), "split": "tune",
           "gold": ["base/core.py"], "test_gold": [], "gold_added_not_findable": 0}
    work = os.path.join(root, "work-r-bogus")
    os.makedirs(work)
    result = H.measure_task(repo, os.path.join(root, "no-such-riltra.exe"), work, row)
    check(result["R"]["status"] == "index-failed" and "returned" not in result["R"],
          "R with a bogus binary is index-failed, never an empty list")


def test_r_doc_comment(riltra, root):
    if riltra is None:
        print("R doc-comment control: SKIPPED, no --riltra given (R paths not exercised by this run)")
        return
    repo = os.path.join(root, "docrepo")
    os.makedirs(repo)
    git_run(repo, ["init", "-q", "-b", "main"])
    write_files(repo, {"src/store.py": 'def handler():\n    """Frobnicate the gizmo."""\n    return 1\n',
                       "misc/other.py": "def other():\n    return 2\n"})
    parent = commit(repo, "Base", 1)
    write_files(repo, {"src/store.py": 'def handler():\n    """Frobnicate the gizmo."""\n    return 3\n'})
    sha = commit(repo, "Frobnicate the gizmo", 2)
    row = {"sha": short(sha), "parent": short(parent), "split": "tune",
           "gold": ["src/store.py"], "test_gold": [], "gold_added_not_findable": 0}
    work = os.path.join(root, "work-r-doc")
    os.makedirs(work)
    result = H.measure_task(repo, riltra, work, row)
    check(result["R"]["status"] == "ok" and result["R"]["found10"] == 1 and result["R"]["first_gold_rank"] == 1,
          "R finds a gold file reachable only through a doc comment at rank 1")


def load_verify():
    """verify.py beside this file; a missing module is a failed check, never a skip."""
    import verify  # noqa: E402 (resolved through sys.path HERE)
    return verify


def test_line_of_byte():
    V = load_verify()
    data = b"ab\ncd\nef"
    check(V.line_of_byte(data, 0) == 1, "line_of_byte: offset 0 is line 1")
    check(V.line_of_byte(data, 3) == 2, "line_of_byte: offset just after the first newline is line 2")
    check(V.line_of_byte(data, 2) == 1, "line_of_byte: a newline byte belongs to the line it ends")
    check(V.line_of_byte(data, 6) == 3, "line_of_byte: offset after the second newline is line 3")


AST_SOURCE = "\n".join([
    "import functools", "", "",
    "def top():", "    pass", "", "",
    "@functools.cache", "def decorated():", "    pass", "", "",
    "class Outer:", "    def method(self):", "        pass", "",
    "    class Inner:", "        async def coro(self):", "            pass",
]) + "\n"


def test_ast_definitions():
    V = load_verify()
    expected = [("top", "function", 4, 4), ("decorated", "function", 9, 8), ("Outer", "class", 13, 13),
                ("method", "function", 14, 14), ("Inner", "class", 17, 17), ("coro", "function", 18, 18)]
    got = V.ast_definitions(AST_SOURCE)
    check(got == expected, "ast_definitions on the hand-written source: %r" % (got,))
    raised = False
    try:
        V.ast_definitions("def broken(:\n    pass\n")
    except SyntaxError:
        raised = True
    check(raised, "ast_definitions lets a SyntaxError propagate")


def test_score_defs():
    V = load_verify()
    one = [("a", "function", 1, 1), ("b", "function", 3, 3)]
    got = V.score_defs(one, [("a", 1), ("b", 3)])
    check(got == {"oracle": 2, "exact": 2, "decorator_line": 0, "missing": [], "extra": []},
          "score_defs: all exact: %r" % (got,))
    dec = [("decorated", "function", 9, 8)]
    got = V.score_defs(dec, [("decorated", 8)])
    check(got == {"oracle": 1, "exact": 0, "decorator_line": 1, "missing": [], "extra": []},
          "score_defs: one decorator-line match: %r" % (got,))
    got = V.score_defs(one, [("a", 1)])
    check(got == {"oracle": 2, "exact": 1, "decorator_line": 0, "missing": [("b", 3)], "extra": []},
          "score_defs: one missing: %r" % (got,))
    got = V.score_defs(one, [("a", 1), ("b", 3), ("zz", 7)])
    check(got == {"oracle": 2, "exact": 2, "decorator_line": 0, "missing": [], "extra": [("zz", 7)]},
          "score_defs: one extra: %r" % (got,))
    dup = [("f", "function", 1, 1), ("f", "function", 5, 5)]
    got = V.score_defs(dup, [("f", 1)])
    check(got == {"oracle": 2, "exact": 1, "decorator_line": 0, "missing": [("f", 5)], "extra": []},
          "score_defs: same name at two lines matches only the line it names: %r" % (got,))
    twice = [("f", "function", 1, 1), ("f", "function", 1, 1)]
    got = V.score_defs(twice, [("f", 1)])
    check(got == {"oracle": 2, "exact": 1, "decorator_line": 0, "missing": [("f", 1)], "extra": []},
          "score_defs: one scanned item is never used for two oracle items: %r" % (got,))


def test_score_callers():
    V = load_verify()
    truth = [("a.py", 3), ("b.py", 5)]
    decoys = [("c.py", 9)]
    got = V.score_callers(truth, decoys, [("a.py", 3), ("b.py", 5)])
    check(got == {"truth": 2, "returned": 2, "found": [("a.py", 3), ("b.py", 5)], "missed": [],
                  "extra_decoy": [], "extra_other": []}, "score_callers: found all: %r" % (got,))
    got = V.score_callers(truth, decoys, [("a.py", 3)])
    check(got["missed"] == [("b.py", 5)] and got["found"] == [("a.py", 3)] and got["returned"] == 1,
          "score_callers: one missed: %r" % (got,))
    got = V.score_callers(truth, decoys, [("a.py", 3), ("b.py", 5), ("c.py", 9)])
    check(got["extra_decoy"] == [("c.py", 9)] and got["extra_other"] == [],
          "score_callers: one decoy extra: %r" % (got,))
    got = V.score_callers(truth, decoys, [("a.py", 3), ("b.py", 5), ("d.py", 2)])
    check(got["extra_other"] == [("d.py", 2)] and got["extra_decoy"] == [],
          "score_callers: one other extra: %r" % (got,))
    got = V.score_callers(truth, decoys, [])
    check(got == {"truth": 2, "returned": 0, "found": [], "missed": [("a.py", 3), ("b.py", 5)],
                  "extra_decoy": [], "extra_other": []}, "score_callers: empty actual: %r" % (got,))
    got = V.score_callers([], [], [("x.py", 1)])
    check(got == {"truth": 0, "returned": 1, "found": [], "missed": [], "extra_decoy": [],
                  "extra_other": [("x.py", 1)]}, "score_callers: empty expected, one actual: %r" % (got,))


def test_caller_sites():
    V = load_verify()
    with tempfile.TemporaryDirectory() as root:
        write_files(root, {"a.py": "x\ny\nzzz\n", "c.py": "p\n"})   # bytes: a.py line 3 starts at byte 4
        doc = {"paths": [
            {"distance": 1, "edges": [{"caller": {"path": "a.py", "byte_start": 4}}]},
            {"distance": 2, "edges": [{"caller": {"path": "b.py", "byte_start": 0}}]},   # ignored; b.py does not exist
            {"distance": 1, "edges": [{"caller": {"path": "a.py", "byte_start": 4}}]},   # duplicate
            {"distance": 1, "edges": [{"caller": {"path": "a.py", "byte_start": 2}}]},
            {"distance": 1, "edges": [{"caller": {"path": "c.py", "byte_start": 2}}]},
        ]}
        got = V.caller_sites(doc, root)
    check(got == [("a.py", 2), ("a.py", 3), ("c.py", 2)],
          "caller_sites: distance-1 callers, de-duplicated, sorted, distance 2 ignored: %r" % (got,))


def test_same_binary_two_runs_identical(riltra, root):
    """C1 repeatability control: the same binary gives byte-identical output on repeated runs."""
    if riltra is None:
        print("C1 same-binary control: SKIPPED, no --riltra given (repeatability not exercised by this run)")
        return
    repo = os.path.join(root, "c1-repo")
    os.makedirs(repo)
    fixture = os.path.join(HERE, "fixtures", "go", "direct")
    for name in os.listdir(fixture):
        if os.path.isfile(os.path.join(fixture, name)):
            shutil.copyfile(os.path.join(fixture, name), os.path.join(repo, name))
    home = os.path.join(root, "c1-home")
    os.makedirs(home)
    env = dict(os.environ, RILTRA_HOME=home)
    code, _ = H.run_riltra(riltra, ["init", "--repo", repo, "--name", "t"], env)
    check(code == 0, "C1: init exits 0")
    code, _ = H.run_riltra(riltra, ["index", "--repo", "t"], env)
    check(code == 0, "C1: index exits 0")
    commands = [
        ["scan", "--repo", "t"],
        ["impact", "--repo", "t", "--path", "direct.go", "--line", "3", "--col", "6", "--depth", "1"],
    ]
    for args in commands:
        first_code, first_out = H.run_riltra(riltra, args, env)
        second_code, second_out = H.run_riltra(riltra, args, env)
        check(first_code == 0 and second_code == 0, "C1: %s exits 0 both times" % args[0])
        check(first_out == second_out, "C1: two runs of %s are byte-identical" % args[0])


def test_site_records():
    V = load_verify()
    with tempfile.TemporaryDirectory() as root:
        write_files(root, {"a.py": "x\ny\nzzz\n"})   # a.py line 3 starts at byte 4
        impact_doc = {"paths": [
            {"distance": 1, "basis": "candidate-name",
             "edges": [{"caller": {"path": "a.py", "byte_start": 4}, "candidate_scope": "same-file", "candidates": 2}]},
            {"distance": 1, "basis": "other",   # same site again: the first record wins
             "edges": [{"caller": {"path": "a.py", "byte_start": 4}, "candidates": 1}]},
            {"distance": 2, "basis": "ignored",
             "edges": [{"caller": {"path": "a.py", "byte_start": 2}}]},
        ]}
        got = V.site_records(impact_doc, root, "impact")
        check(got == [{"path": "a.py", "line": 3, "basis": "candidate-name",
                       "candidate_scope": "same-file", "candidates": 2}],
              "site_records impact: first record per site, distance 1 only: %r" % (got,))
        callers_doc = {"edges": [{"caller": {"path": "a.py", "byte_start": 2},
                                  "candidate_scope": "same-file", "candidates": 2}]}
        got = V.site_records(callers_doc, root, "callers")
        check(got == [{"path": "a.py", "line": 2, "candidate_scope": "same-file", "candidates": 2}],
              "site_records callers: fields come from the edge: %r" % (got,))


def test_unit_b_pure():
    """Unit B pure functions: hunk contexts, file classes, commit selection, summaries, anchors, stderr."""
    V = load_verify()
    diff = "\n".join([
        "diff --git a/src/a.py b/src/a.py",
        "index 1111111..2222222 100644",
        "--- a/src/a.py",
        "+++ b/src/a.py",
        "@@ -3 +3 @@ def foo(self):",
        "-    x = 1",
        "+    x = 2",
        "@@ -10,2 +10,3 @@",
        "--- old comment",
        "-    y",
        "+    z",
        "+    w",
        "+    v",
        "diff --git a/src/new.py b/src/new.py",
        "new file mode 100644",
        "--- /dev/null",
        "+++ b/src/new.py",
        "@@ -0,0 +1,2 @@",
        "+A = 1",
        "+B = 2",
        "diff --git a/src/gone.py b/src/gone.py",
        "deleted file mode 100644",
        "--- a/src/gone.py",
        "+++ /dev/null",
        "@@ -1,2 +0,0 @@ class Gone:",
        "-C = 1",
        "-D = 2",
        "",
    ])
    got = V.parse_hunk_contexts(diff)
    check(got == {"src/a.py": ["def foo(self):", ""], "src/new.py": [""], "src/gone.py": ["class Gone:"]},
          "parse_hunk_contexts: modified, added and deleted files, hunk bodies skipped by count: %r" % (got,))

    classify_cases = [
        (("binary", ["foo"], ["def foo():"]), {"class": "status:binary", "unmatched_contexts": 0, "unmatched_names": 0}),
        (("parse-error", [], []), {"class": "status:parse-error", "unmatched_contexts": 0, "unmatched_names": 0}),
        (("unsupported", [], []), {"class": "status:unsupported", "unmatched_contexts": 0, "unmatched_names": 0}),
        (("added", ["foo"], [""]), {"class": "whole-file:added", "unmatched_contexts": 0, "unmatched_names": 1}),
        (("deleted", ["foo"], ["def foo():"]), {"class": "whole-file:deleted", "unmatched_contexts": 0, "unmatched_names": 0}),
        (("modified", ["foo"], ["def foo(self):"]), {"class": "agree", "unmatched_contexts": 0, "unmatched_names": 0}),
        (("modified", ["foo"], ["", "def bar():"]), {"class": "disagree", "unmatched_contexts": 1, "unmatched_names": 1}),
        (("modified", ["foo"], ["def foobar():"]), {"class": "disagree", "unmatched_contexts": 1, "unmatched_names": 1}),
        (("modified", ["foo"], ["", ""]), {"class": "no-git-context", "unmatched_contexts": 0, "unmatched_names": 1}),
        (("renamed", [], ["def bar():"]), {"class": "no-riltra-definitions", "unmatched_contexts": 1, "unmatched_names": 0}),
        (("modified", [], ["", ""]), {"class": "both-empty", "unmatched_contexts": 0, "unmatched_names": 0}),
    ]
    for args, expected in classify_cases:
        got = V.classify_file(*args)
        check(got == expected, "classify_file %r -> %r" % (args, got))

    records = [
        ("bbbb", True, ["x/a.py"]),
        ("aaaa", False, ["x/a.py"]),
        ("cccc", True, ["docs/x.md"]),
        ("dddd", True, ["x/B.PY"]),
        ("eeee", True, ["x/c.md", "y/d.rs"]),
    ]
    supported = lambda path: os.path.splitext(path)[1][1:].lower() in ("py", "rs")
    check(V.select_commits(records, supported) == ["bbbb", "dddd", "eeee"],
          "select_commits: root and non-supported commits dropped, sorted by sha")
    check(V.select_commits(records, supported, limit=2) == ["bbbb", "dddd"],
          "select_commits: first N after sorting")

    check(V.summarize_callers({"edges": [{"kind": "CALLS", "candidates": 2}, {"kind": "POTENTIAL_CALL"},
                                         {"kind": "CALLS"}]})
          == {"edges": 3, "by_kind": {"CALLS": 2, "POTENTIAL_CALL": 1}, "candidates_field": True},
          "summarize_callers: kinds counted, candidates field present")
    check(V.summarize_callers({"edges": [{"kind": "CALLS"}]})
          == {"edges": 1, "by_kind": {"CALLS": 1}, "candidates_field": False},
          "summarize_callers: no candidates field")
    check(V.summarize_callers({"edges": [{"candidates": 1}]})
          == {"edges": 1, "by_kind": {"none": 1}, "candidates_field": True},
          "summarize_callers: a missing kind is counted as none")
    check(V.summarize_callers({"edges": []}) == {"edges": 0, "by_kind": {}, "candidates_field": False},
          "summarize_callers: no edges")

    check(V.summarize_impact({"paths": [{"basis": "candidate-name"}, {"basis": "candidate-name"}, {}],
                              "verdict": "UNKNOWN", "completeness": "partial"})
          == {"paths": 3, "verdict": "UNKNOWN", "completeness": "partial",
              "by_basis": {"candidate-name": 2, "none": 1}},
          "summarize_impact: basis counted, missing basis is none")
    check(V.summarize_impact({"paths": []})
          == {"paths": 0, "verdict": None, "completeness": None, "by_basis": {}},
          "summarize_impact: missing verdict and completeness are None, not guessed")

    a_rs = "\n".join([
        "fn foo() {}",
        "    pub(crate) fn foo_bar() {}",
        "pub fn foo(x: u8) {}",
        "// fn foo mention",
        "    const fn foo() {}",
    ])
    got = V.anchor_matches("foo", {"b.rs": "fn foo() {}\n", "a.rs": a_rs})
    check(got == [("a.rs", 1, 4, 1), ("a.rs", 3, 8, 5), ("a.rs", 5, 14, 11), ("b.rs", 1, 4, 1)],
          "anchor_matches: identifier and keyword columns, near misses ignored: %r" % (got,))

    check(V.stderr_class("") == "none", "stderr_class: empty text is none")
    check(V.stderr_class("   \n") == "none", "stderr_class: blank text is none")
    check(V.stderr_class("error: no index found\n") == "error: no index found", "stderr_class: first words of the first line")
    check(V.stderr_class("\n\nunknown flag --depth\nmore") == "unknown flag --depth", "stderr_class: first non-empty line")
    check(V.stderr_class("C" + ":/x/y.db") == "unnamed", "stderr_class: a path-only line is unnamed")
    check(V.stderr_class("cannot open C:" + chr(92) + "x" + chr(92) + "y.db now") == "cannot open now",
          "stderr_class: path tokens with a separator are dropped")

    search_doc = {"results": [{"path": "a.rs", "span": {"start_line": 3}}, {"path": "b.rs"}]}
    check(V.definition_found(search_doc, "a.rs", 3) is True, "definition_found: path and start line both match")
    check(V.definition_found(search_doc, "a.rs", 4) is False, "definition_found: a different start line does not match")
    check(V.definition_found(search_doc, "b.rs", 1) is False, "definition_found: a result without a span does not match")
    check(V.definition_found({"results": []}, "a.rs", 3) is False, "definition_found: no results")
    raised = False
    try:
        V.definition_found({"total": 0}, "a.rs", 3)
    except V.VerifyError:
        raised = True
    check(raised, "definition_found: a document without a results list is a VerifyError")


def test_unit_c_pure():
    """Unit C pure functions: hunk line ranges from git's headers, and the range consistency check."""
    V = load_verify()
    diff = "\n".join([
        "diff --git a/src/a.py b/src/a.py",
        "--- a/src/a.py",
        "+++ b/src/a.py",
        "@@ -3,2 +3,4 @@ def f():",
        "-x",
        "-y",
        "+a",
        "+b",
        "+c",
        "+d",
        "@@ -5,0 +6,3 @@",
        "+p",
        "+q",
        "+r",
        "@@ -10,2 +9,0 @@",
        "--- old comment",
        "-q",
        "@@ -1 +1 @@",
        "-old",
        "+new",
        "diff --git a/src/new.py b/src/new.py",
        "--- /dev/null",
        "+++ b/src/new.py",
        "@@ -0,0 +1,5 @@",
        "+A = 1",
        "+B = 2",
        "+C = 3",
        "+D = 4",
        "+E = 5",
        "diff --git a/src/gone.py b/src/gone.py",
        "--- a/src/gone.py",
        "+++ /dev/null",
        "@@ -1,5 +0,0 @@ class Gone:",
        "-C = 1",
        "-D = 2",
        "-E = 3",
        "-F = 4",
        "-G = 5",
        "",
    ])
    got = V.parse_hunk_ranges(diff)
    want = {
        "src/a.py": [{"old": (3, 4), "new": (3, 6)}, {"old": None, "new": (6, 8)},
                     {"old": (10, 11), "new": None}, {"old": (1, 1), "new": (1, 1)}],
        "src/new.py": [{"old": None, "new": (1, 5)}],
        "src/gone.py": [{"old": (1, 5), "new": None}],
    }
    check(got == want, "parse_hunk_ranges: every hunk shape, added and deleted files: %r" % (got,))

    def span(start, end):
        return {"start_line": start, "end_line": end}

    def make_entry(definitions, hunks, outside):
        return {"status": "modified", "symbols": "established", "hunks": hunks,
                "hunks_outside_definitions": outside, "definitions": definitions}

    cases = [
        ("all consistent",
         make_entry([{"change": "modified", "old_span": span(3, 5), "new_span": span(3, 7)}], 1, []),
         [{"old": (4, 4), "new": (4, 4)}],
         {"definitions": 1, "definitions_intersecting": 1, "hunks_equal": True, "outside_equal": True}),
        ("listed definition intersects nothing",
         make_entry([{"change": "added", "old_span": None, "new_span": span(20, 25)}], 1, [{"hunk": 1}]),
         [{"old": None, "new": (4, 4)}],
         {"definitions": 1, "definitions_intersecting": 0, "hunks_equal": True, "outside_equal": True}),
        ("hunk count mismatch",
         make_entry([{"change": "modified", "old_span": span(3, 5), "new_span": span(3, 7)}], 2, []),
         [{"old": (4, 4), "new": (4, 4)}],
         {"definitions": 1, "definitions_intersecting": 1, "hunks_equal": False, "outside_equal": True}),
        ("outside count mismatch",
         make_entry([{"change": "added", "old_span": None, "new_span": span(20, 25)}], 1, []),
         [{"old": None, "new": (4, 4)}],
         {"definitions": 1, "definitions_intersecting": 0, "hunks_equal": True, "outside_equal": False}),
        ("removed definition intersects on the old side",
         make_entry([{"change": "removed", "old_span": span(10, 12), "new_span": None}], 1, []),
         [{"old": (11, 11), "new": None}],
         {"definitions": 1, "definitions_intersecting": 1, "hunks_equal": True, "outside_equal": True}),
        ("removed definition ignores the new side",
         make_entry([{"change": "removed", "old_span": span(10, 12), "new_span": span(11, 11)}], 1, []),
         [{"old": None, "new": (11, 11)}],
         {"definitions": 1, "definitions_intersecting": 0, "hunks_equal": True, "outside_equal": True}),
        ("modified definition intersects on the old side only",
         make_entry([{"change": "modified", "old_span": span(10, 12), "new_span": span(30, 35)}], 1, []),
         [{"old": (11, 11), "new": (4, 4)}],
         {"definitions": 1, "definitions_intersecting": 1, "hunks_equal": True, "outside_equal": True}),
    ]
    for name, doc_entry, hunks, want_check in cases:
        got_check = V.check_file_ranges(doc_entry, hunks)
        check(got_check == want_check, "check_file_ranges, %s: %r" % (name, got_check))


def test_unit_c_replay(root):
    """replay-check on a three-commit repository: C2 agrees with its changes document, C3 disagrees on hunk count."""
    import contextlib
    import io
    V = load_verify()
    repo = os.path.join(root, "replay-repo")
    os.makedirs(repo)
    git_run(repo, ["init", "-q", "-b", "main"])
    write_files(repo, {"src/a.py": "one\ntwo\nthree\nfour\nfive\n"})
    commit(repo, "Base", 1)
    write_files(repo, {"src/a.py": "one\nTWO\nthree\nfour\nfive\n"})
    c2 = commit(repo, "Change line two", 2)
    write_files(repo, {"src/a.py": "one\nTWO\nthree\nFOUR\nfive\n"})
    c3 = commit(repo, "Change line four", 3)

    definition = [{"change": "modified", "old_span": {"start_line": 1, "end_line": 5},
                   "new_span": {"start_line": 1, "end_line": 5}}]
    checked = {"path": "src/a.py", "status": "modified", "symbols": "established", "hunks": 1,
               "hunks_outside_definitions": [], "definitions": definition}
    unchecked = {"path": "src/b.py", "status": "added", "symbols": "established", "hunks": 1,
                 "hunks_outside_definitions": [], "definitions": []}
    replay = os.path.join(root, "replay-dir")
    os.makedirs(replay)
    write_files(replay, {
        c2[:12] + "-changes.json": json.dumps({"files": [checked, unchecked]}),
        c2[:12] + "-changes.stderr.txt": "",
        c3[:12] + "-changes.json": json.dumps({"files": [dict(checked, hunks=2)]}),
    })
    out = io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
        code = V.main(["replay-check", "--tree", repo, "--replay-dir", replay])
    check(code == 0, "replay-check exits 0 when every commit has a readable changes document")
    lines = [json.loads(line) for line in out.getvalue().splitlines()]
    check(len(lines) == 2 and lines[0] == {"header": {"tree_commit": c3, "commits": 2}},
          "replay-check header: tree commit and commit count: %r" % (lines[:1],))
    want = {"files": 2, "definitions": 2, "definitions_intersecting": 2, "files_hunks_equal": 1,
            "files_outside_equal": 2, "inconsistent": [c3[:12] + ":0"]}
    check(lines[1] == {"summary": want}, "replay-check summary: %r" % (lines[1],))

    bad_sha = os.path.join(root, "replay-bad-sha")
    os.makedirs(bad_sha)
    write_files(bad_sha, {"000000000000-changes.json": json.dumps({"files": []})})
    err = io.StringIO()
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
        code = V.main(["replay-check", "--tree", repo, "--replay-dir", bad_sha])
    check(code == 3 and "000000000000" in err.getvalue(),
          "replay-check exits 3 and names the commit when git cannot resolve it")

    bad_json = os.path.join(root, "replay-bad-json")
    os.makedirs(bad_json)
    write_files(bad_json, {c2[:12] + "-changes.json": "not json"})
    err = io.StringIO()
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
        code = V.main(["replay-check", "--tree", repo, "--replay-dir", bad_json])
    check(code == 3 and c2[:12] in err.getvalue(),
          "replay-check exits 3 and names the commit when its changes document is not JSON")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--riltra", default=None)
    args = parser.parse_args()
    riltra_path = os.path.abspath(args.riltra) if args.riltra else None
    test_set_mismatch()
    test_rule_tables()
    test_path_words_and_bm25()
    test_extract_tokens()
    test_clean_query()
    test_win_loss()
    test_mention()
    test_summary_arithmetic()
    test_r_win_loss()
    test_line_of_byte()
    test_ast_definitions()
    test_score_defs()
    test_score_callers()
    test_caller_sites()
    test_site_records()
    test_unit_b_pure()
    test_unit_c_pure()
    if shutil.which("git") is None:
        print("GIT-BASED CHECKS: SKIPPED, git is not on PATH (freeze, measurements, S, B3 and R not exercised)")
        print("SELFTEST PASSED: %d checks" % PASSED)
        return
    with tempfile.TemporaryDirectory() as root:
        repo, sha = test_freeze_and_rules(root)
        test_measurements(root, repo, sha)
        test_s_control(root, repo, sha)
        test_r_bogus_binary(root, repo, sha)
        test_b3_extract_failure(root)
        test_b3_control(riltra_path, root)
        test_r_doc_comment(riltra_path, root)
        test_same_binary_two_runs_identical(riltra_path, os.path.join(root, "c1"))
        test_size_order(root)
        test_run_header(root)
        test_unit_c_replay(root)
    print("SELFTEST PASSED: %d checks" % PASSED)


if __name__ == "__main__":
    main()
