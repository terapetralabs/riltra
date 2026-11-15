# SPDX-License-Identifier: Apache-2.0
"""Controls for harness.py. Runs before any measurement; exits non-zero on the first failure.

Usage: python -I selftest.py [--riltra <path to a riltra binary>]
Without --riltra the B3 control is skipped and the skip reason is printed.
"""
import argparse
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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--riltra", default=None)
    args = parser.parse_args()
    test_set_mismatch()
    test_rule_tables()
    test_path_words_and_bm25()
    test_extract_tokens()
    test_clean_query()
    test_win_loss()
    if shutil.which("git") is None:
        print("GIT-BASED CHECKS: SKIPPED, git is not on PATH (freeze, measurements, B3 not exercised)")
        print("SELFTEST PASSED: %d checks" % PASSED)
        return
    with tempfile.TemporaryDirectory() as root:
        repo, sha = test_freeze_and_rules(root)
        test_measurements(root, repo, sha)
        test_b3_extract_failure(root)
        test_b3_control(os.path.abspath(args.riltra) if args.riltra else None, root)
    print("SELFTEST PASSED: %d checks" % PASSED)


if __name__ == "__main__":
    main()
