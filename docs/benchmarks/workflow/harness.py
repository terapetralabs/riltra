# SPDX-License-Identifier: Apache-2.0
"""PR 8 query task set: freeze git-history tasks and measure three frozen baselines.

Commands (stdlib only; run as: python -I harness.py ...):
  freeze --repo DIR --tip FULLSHA --label LABEL --out MANIFEST.jsonl
  run --split tune [--manifest MANIFEST.jsonl] --repo DIR --riltra BIN --work DIR
  run --split heldout --final ...        (PR 8 never passes --final)
Method, rules and limits: README.md in this folder.
"""
import argparse
import hashlib
import io
import json
import math
import os
import posixpath
import re
import shutil
import subprocess
import sys
import tarfile
from collections import Counter

RULES_VERSION = "pr8-rules-1"

if sys.version_info < (3, 12):
    raise SystemExit("harness.py needs Python 3.12 or newer (tarfile extraction filter)")
MAX_CHANGED_FILES = 8     # Rule 1 step 2: more changed files than this is excluded
SELECTIVITY_LIMIT = 50    # B1 and B3: a token matching more files than this is dropped
PAGE_LIMIT = 1000         # riltra MAX_SEARCH_LIMIT (crates/cli/src/index.rs, line 17)
TOKEN_CAP = 20            # E0 keeps the first 20 distinct tokens
SHORT = 12                # short sha length used in tracked rows
BM25_K1 = 1.2
BM25_B = 0.75
TASK_INDEX_NAME = "t"
BASELINES = ("B1", "B2", "B3")
PAIRS = (("B1", "B2"), ("B1", "B3"), ("B2", "B3"))
EXCLUSION_KEYS = ("merges", "over_max_files", "no_supported_file", "no_gold")

# Rule 1 step 3: the extension list copied from classify() in crates/cli/src/syntax.rs
# (lines 77-87; .h is C++ there). cu and cuh are CudaExperimental, so they are not listed.
SUPPORTED_EXTS = "py js mjs cjs ts mts cts tsx rs go java c h hh hpp hxx inl cc cpp cxx".split()
EXT_ALT = "|".join(SUPPORTED_EXTS)
# B1 pathspecs: the supported extensions, case-insensitive as classify() lowercases them.
GREP_PATHSPECS = [":(icase)*." + ext for ext in SUPPORTED_EXTS]

# E0 token extraction. One pass, leftmost match first; at one position the first
# alternative wins, so a span is consumed by one class only:
#   group 1 = (a) text inside backticks (no newline inside the span)
#   group 2 = (b) path-like word ending in a supported extension
#   group 3 = (c)/(d) identifier; group 4 = "()" when the identifier is followed by ()
TOKEN_RE = re.compile(
    r"`([^`\n]+)`"
    r"|(?<![A-Za-z0-9_./\-])([A-Za-z0-9_][A-Za-z0-9_./\-]*\.(?i:" + EXT_ALT + r"))"
    r"(?![A-Za-z0-9_/\-]|\.[A-Za-z0-9_])"
    r"|(?<![A-Za-z0-9_])([A-Za-z_][A-Za-z0-9_]*)(\(\))?"
)


class HarnessError(Exception):
    """A refusal or a failure that stops the run; nothing is reported after it."""


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    try:
        with open(path, "rb") as handle:
            return sha256_hex(handle.read())
    except OSError as error:
        raise HarnessError("cannot read %s (%s)" % (os.path.basename(path), error.strerror))


def set_mismatch(a, b):
    """Number of items in exactly one of the two collections (0 means equal sets)."""
    return len(set(a) ^ set(b))


def is_test_path(path):
    """Python port of is_test_path in crates/cli/src/index.rs (lines 2715-2727)."""
    parts = path.split("/")
    name = parts.pop()
    in_test_dir = any(d in ("test", "tests", "__tests__", "spec") for d in parts)
    test_file = ((name.startswith("test_") and name.endswith(".py"))
                 or name.endswith("_test.go")
                 or name.endswith(".test.ts")
                 or name.endswith(".spec.ts")
                 or name.endswith("Test.java"))
    return in_test_dir or test_file


def is_supported(path):
    """Port of classify() in crates/cli/src/syntax.rs: the lowercased extension is in the list."""
    ext = posixpath.splitext(path)[1][1:].lower()
    return ext in SUPPORTED_EXTS


def clean_query(body):
    """Subject and body as written, minus only the Signed-off-by and Co-Authored-By lines."""
    kept = [line for line in body.split("\n")
            if not line.startswith(("Signed-off-by:", "Co-Authored-By:"))]
    return "\n".join(kept).rstrip("\n")


def extract_tokens(text):
    """E0: first 20 distinct tokens in order of first appearance (see TOKEN_RE)."""
    tokens = []
    for match in TOKEN_RE.finditer(text):
        if match.group(1) is not None:
            token = match.group(1).strip()
        elif match.group(2) is not None:
            token = match.group(2)
        else:
            word = match.group(3)
            if (len(word) >= 4 and ("_" in word or any(c.isupper() for c in word[1:]))) \
                    or match.group(4) is not None:
                token = word
            else:
                continue
        if not token or token in tokens:
            continue
        tokens.append(token)
        if len(tokens) == TOKEN_CAP:
            break
    return tokens


def path_words(path):
    """Lowercase words of a path: split on / . _ - and lower-to-upper boundaries (B2)."""
    parts = re.split(r"[/._\-]|(?<=[a-z])(?=[A-Z])", path)
    return [part.lower() for part in parts if part]


def bm25_rank(paths, query_text):
    """Okapi BM25 over path words with the stdlib; returns (path, score) with score > 0.

    idf = ln(1 + (N - df + 0.5) / (df + 0.5)); query terms are the distinct query words;
    ties are broken by path ascending.
    """
    docs = {path: path_words(path) for path in paths}
    n_docs = len(docs)
    if n_docs == 0:
        return []
    avgdl = sum(len(words) for words in docs.values()) / n_docs
    counts = {path: Counter(words) for path, words in docs.items()}
    scores = {}
    for term in sorted(set(re.findall(r"[a-z0-9]+", query_text.lower()))):
        df = sum(1 for path in docs if counts[path][term] > 0)
        if df == 0:
            continue
        idf = math.log(1 + (n_docs - df + 0.5) / (df + 0.5))
        for path in docs:
            freq = counts[path][term]
            if freq == 0:
                continue
            length = len(docs[path])
            denom = freq + BM25_K1 * (1 - BM25_B + BM25_B * length / avgdl)
            scores[path] = scores.get(path, 0.0) + idf * freq * (BM25_K1 + 1) / denom
    return sorted(scores.items(), key=lambda item: (-item[1], item[0]))


def git_bytes(repo, *args):
    proc = subprocess.run(["git", "-C", repo] + list(args), capture_output=True)
    if proc.returncode != 0:
        raise HarnessError("git %s exited %d" % (args[0], proc.returncode))
    return proc.stdout


def git_out(repo, *args):
    return git_bytes(repo, *args).decode("utf-8")


def resolve_commit(repo, rev):
    return git_out(repo, "rev-parse", "--verify", "--quiet", rev + "^{commit}").strip()


def parent_of(repo, sha):
    parents = git_out(repo, "log", "-1", "--format=%P", sha).split()
    if len(parents) > 1:
        raise HarnessError("a merge commit reached the task path")
    return parents[0] if parents else None


def commit_records(repo, tip):
    """Non-merge commits reachable from tip as (committer time, full sha, parent or None)."""
    text = git_out(repo, "log", "--no-merges", "--format=%H%x1f%ct%x1f%P%x1e", tip)
    records = []
    for chunk in text.split("\x1e"):
        chunk = chunk.lstrip("\n")
        if not chunk:
            continue
        sha, committed, parents = chunk.split("\x1f")
        parent_list = parents.split()
        records.append((int(committed), sha, parent_list[0] if parent_list else None))
    return records


def changed_files(repo, sha):
    """(status, path) for every file the commit changed, renames split into D and A."""
    text = git_out(repo, "diff-tree", "--no-renames", "-r", "--root", "--no-commit-id",
                   "-z", "--name-status", sha)
    fields = text.split("\0")
    if fields and fields[-1] == "":
        fields.pop()
    if len(fields) % 2:
        raise HarnessError("unexpected diff-tree output")
    return [(fields[i], fields[i + 1]) for i in range(0, len(fields), 2)]


def tree_paths(repo, rev):
    text = git_out(repo, "ls-tree", "-r", "-z", "--full-tree", "--name-only", rev)
    return [path for path in text.split("\0") if path]


def universe_of(repo, rev):
    """Candidate universe: supported-language, non-test files of the tree."""
    return sorted(p for p in tree_paths(repo, rev) if is_supported(p) and not is_test_path(p))


def classify_commit(repo, sha, parent, changed):
    """Rule 2: (gold, test_gold, added-file count) for one commit."""
    parent_paths = set(tree_paths(repo, parent)) if parent else set()
    gold, test_gold, added = [], [], 0
    for status, path in changed:
        if not is_supported(path):
            continue
        if status == "A":
            if not is_test_path(path):
                added += 1
            continue
        if status in ("M", "D") and path in parent_paths:
            (test_gold if is_test_path(path) else gold).append(path)
    return sorted(gold), sorted(test_gold), added


def write_new_file(path, data):
    with open(path, "xb") as handle:
        handle.write(data)


def freeze(repo, tip, label, out):
    """Rule 1 and the split. Writes the manifest and its .sha256; never overwrites."""
    if len(tip) != 40 or any(c not in "0123456789abcdef" for c in tip):
        raise HarnessError("--tip must be a full 40-character lowercase sha")
    sha_path = os.path.splitext(out)[0] + ".sha256"
    if os.path.exists(out) or os.path.exists(sha_path):
        raise HarnessError("manifest already exists; a different tip is a new task set and needs a new file")
    tip_sha = resolve_commit(repo, tip)
    if tip_sha != tip:
        raise HarnessError("--tip did not resolve to itself")
    excluded = dict.fromkeys(EXCLUSION_KEYS, 0)
    excluded["merges"] = len(git_out(repo, "rev-list", "--merges", tip_sha).split())
    records = sorted(commit_records(repo, tip_sha), key=lambda record: (record[0], record[1]))
    kept = []
    for _, sha, parent in records:
        changed = changed_files(repo, sha)
        if len(changed) > MAX_CHANGED_FILES:
            excluded["over_max_files"] += 1
            continue
        if not any(is_supported(path) for _, path in changed):
            excluded["no_supported_file"] += 1
            continue
        gold, test_gold, added = classify_commit(repo, sha, parent, changed)
        if not gold:
            excluded["no_gold"] += 1
            continue
        kept.append((sha, parent, gold, test_gold, added))
    shorts = [sha[:SHORT] for sha, _, _, _, _ in kept]
    if len(set(shorts)) != len(shorts):
        raise HarnessError("short sha collision in the task set")
    tune_n = (7 * len(kept)) // 10
    header = {"label": label, "tip": tip_sha, "rules_version": RULES_VERSION,
              "non_merge": len(records), "excluded": excluded, "kept": len(kept),
              "tune": tune_n, "heldout": len(kept) - tune_n}
    lines = [json.dumps({"header": header}, separators=(",", ":"))]
    for index, (sha, parent, gold, test_gold, added) in enumerate(kept):
        row = {"sha": sha[:SHORT], "parent": parent[:SHORT] if parent else "none",
               "split": "tune" if index < tune_n else "heldout",
               "gold": gold, "test_gold": test_gold, "gold_added_not_findable": added}
        lines.append(json.dumps(row, separators=(",", ":")))
    body = ("\n".join(lines) + "\n").encode("utf-8")
    write_new_file(out, body)
    write_new_file(sha_path, ("%s  %s\n" % (sha256_hex(body), os.path.basename(out))).encode("utf-8"))
    return {"label": label, "tip": tip_sha, "non_merge": len(records), "excluded": excluded,
            "kept": len(kept), "tune": tune_n, "heldout": len(kept) - tune_n}


def load_manifest(path):
    """Verify the digest and rules_version, then return (header, rows, digest)."""
    with open(path, "rb") as handle:
        data = handle.read()
    digest = sha256_hex(data)
    sha_path = os.path.splitext(path)[0] + ".sha256"
    with open(sha_path, "r", encoding="utf-8") as handle:
        recorded = handle.read().split()
    if not recorded or recorded[0] != digest:
        raise HarnessError("manifest digest does not match its .sha256 file")
    lines = data.decode("utf-8").split("\n")
    if lines[-1] != "":
        raise HarnessError("manifest must end with a newline")
    lines = lines[:-1]
    header = json.loads(lines[0])["header"]
    if header.get("rules_version") != RULES_VERSION:
        raise HarnessError("manifest rules_version %r differs from the harness %r"
                           % (header.get("rules_version"), RULES_VERSION))
    rows = [json.loads(line) for line in lines[1:]]
    return header, rows, digest


def git_grep_files(repo, parent, token):
    proc = subprocess.run(
        ["git", "-C", repo, "grep", "-z", "-l", "-w", "-F", "-e", token, parent, "--"] + GREP_PATHSPECS,
        capture_output=True)
    if proc.returncode == 1:
        return set()
    if proc.returncode != 0:
        raise HarnessError("git grep exited %d" % proc.returncode)
    prefix = parent + ":"
    files = set()
    for entry in proc.stdout.decode("utf-8").split("\0"):
        if not entry:
            continue
        if not entry.startswith(prefix):
            raise HarnessError("git grep output without the tree prefix")
        files.add(entry[len(prefix):])
    return files


def b1_collect(repo, parent, tokens):
    """B1 raw matches per token: files of the parent tree (supported pathspecs, tests included)."""
    return {token: git_grep_files(repo, parent, token) for token in tokens}


def score_sets(sets, universe):
    """Selectivity rule, then file score = number of distinct tokens matched; path ascending on ties."""
    allowed = set(universe)
    score = Counter()
    dropped = 0
    for files in sets.values():
        if len(files) > SELECTIVITY_LIMIT:
            dropped += 1
            continue
        for path in files:
            if path in allowed:
                score[path] += 1
    return sorted(score, key=lambda path: (-score[path], path)), dropped


def metrics(ranked, gold):
    position = {}
    for index, path in enumerate(ranked, start=1):
        position[path] = index
    ranks = sorted(position[g] for g in gold if g in position)
    return {"found5": sum(1 for g in gold if position.get(g, 10 ** 9) <= 5),
            "found10": sum(1 for g in gold if position.get(g, 10 ** 9) <= 10),
            "first_gold_rank": ranks[0] if ranks else None,
            "returned": len(ranked)}


def compare_pair(row, a, b):
    """Win/loss on found@10 over tasks where both baselines are ok: a, b, 'tie', or None."""
    if row[a]["status"] != "ok" or row[b]["status"] != "ok":
        return None
    fa, fb = row[a]["found10"], row[b]["found10"]
    if fa > fb:
        return a
    if fb > fa:
        return b
    return "tie"


def extract_tree(repo, rev, dest):
    data = git_bytes(repo, "archive", rev)
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        archive.extractall(dest, filter="data")


def run_riltra(riltra, args, env):
    proc = subprocess.run([riltra] + args, capture_output=True, env=env)
    return proc.returncode, proc.stdout


def parse_page(stdout):
    """(total, [paths]) from one search page, or None when the JSON is not the expected shape."""
    try:
        page = json.loads(stdout.decode("utf-8"))
    except ValueError:
        return None
    if not isinstance(page, dict):
        return None
    total = page.get("total")
    results = page.get("results")
    if not isinstance(total, int) or isinstance(total, bool) or not isinstance(results, list):
        return None
    paths = []
    for hit in results:
        if not isinstance(hit, dict) or not isinstance(hit.get("path"), str):
            return None
        paths.append(hit["path"])
    return total, paths


def b3_index_and_search(riltra, repo, parent, tokens, task_dir):
    src = os.path.join(task_dir, "src")
    home = os.path.join(task_dir, "home")
    os.makedirs(src)
    os.makedirs(home)
    try:
        extract_tree(repo, parent, src)
    except (OSError, tarfile.TarError):
        return "index-failed", "extract", None
    env = dict(os.environ, RILTRA_HOME=home)
    try:
        for args in (["init", "--repo", src, "--name", TASK_INDEX_NAME],
                     ["index", "--repo", TASK_INDEX_NAME]):
            code, _ = run_riltra(riltra, args, env)
            if code != 0:
                return "index-failed", "riltra %s exited %d" % (args[0], code), None
    except OSError as error:
        return "index-failed", "cannot start riltra (%s)" % error.strerror, None
    sets = {}
    for token in tokens:
        files = set()
        offset = 0
        while True:
            args = ["search", "--repo", TASK_INDEX_NAME, "--symbol", token,
                    "--limit", str(PAGE_LIMIT), "--offset", str(offset)]
            try:
                code, stdout = run_riltra(riltra, args, env)
            except OSError as error:
                return "search-error", "cannot start riltra for search (%s)" % error.strerror, None
            if code != 0:
                return "search-error", "search exited %d" % code, None
            page = parse_page(stdout)
            if page is None:
                return "search-error", "unparsable search JSON", None
            total, paths = page
            if not paths:
                if offset >= total:
                    break
                return "search-error", "empty page before the reported total", None
            files.update(paths)
            offset += len(paths)
            if offset >= total:
                break
        sets[token] = files
    return "ok", "", sets


def b3_collect(riltra, repo, parent, tokens, task_dir):
    """B3 raw matches per token from a fresh index of the parent tree.

    Returns (status, detail, sets); sets is None unless status is ok. task_dir must not exist;
    it is removed afterwards.
    """
    os.makedirs(task_dir)
    try:
        return b3_index_and_search(riltra, repo, parent, tokens, task_dir)
    finally:
        remove_tree(task_dir)


def remove_tree(path):
    """Remove a scratch tree; on Windows the \\\\?\\ prefix lets it remove paths over MAX_PATH."""
    target = path
    if os.name == "nt":
        target = "\\\\?\\" + os.path.abspath(path)
    shutil.rmtree(target)


def measure_task(repo, riltra, work, row):
    """One task row (from the manifest) measured by B1, B2 and B3; counts only, no query text out."""
    sha = resolve_commit(repo, row["sha"])
    if not sha.startswith(row["sha"]):
        raise HarnessError("manifest sha does not resolve to its own commit")
    parent = parent_of(repo, sha)
    if parent is None or not parent.startswith(row["parent"]):
        raise HarnessError("manifest parent is not the commit's first parent")
    query = clean_query(git_out(repo, "log", "-1", "--format=%B", sha))
    tokens = extract_tokens(query)
    universe = universe_of(repo, parent)
    gold = row["gold"]
    if not set(gold) <= set(universe):
        raise HarnessError("a gold file is outside the candidate universe; the manifest does not match the repository")
    result = {"sha": row["sha"], "parent": row["parent"], "split": row["split"],
              "n": len(gold), "e0_tokens": len(tokens)}
    ranked, dropped = score_sets(b1_collect(repo, parent, tokens), universe)
    result["B1"] = dict(status="ok", dropped=dropped, **metrics(ranked, gold))
    ranked = [path for path, _ in bm25_rank(universe, query)]
    result["B2"] = dict(status="ok", **metrics(ranked, gold))
    status, detail, sets = b3_collect(riltra, repo, parent, tokens, os.path.join(work, row["sha"]))
    if status == "ok":
        ranked, dropped = score_sets(sets, universe)
        result["B3"] = dict(status="ok", dropped=dropped, **metrics(ranked, gold))
    else:
        result["B3"] = {"status": status, "detail": detail, "token_count": len(tokens)}
    return result


def summarize(results):
    summary = {"tasks": len(results)}
    for name in BASELINES:
        ok = [r for r in results if r[name]["status"] == "ok"]
        summary[name] = {"ok": len(ok), "failed": len(results) - len(ok),
                         "hits_at_10": sum(1 for r in ok if r[name]["found10"] >= 1)}
    pairs = {}
    for a, b in PAIRS:
        verdicts = [compare_pair(r, a, b) for r in results]
        pairs["%s-%s" % (a, b)] = {
            "both_ok": sum(1 for v in verdicts if v is not None),
            "%s_wins" % a: sum(1 for v in verdicts if v == a),
            "%s_wins" % b: sum(1 for v in verdicts if v == b),
            "ties": sum(1 for v in verdicts if v == "tie")}
    summary["pairs"] = pairs
    return summary


def riltra_version(riltra):
    proc = subprocess.run([riltra, "--version"], capture_output=True)
    if proc.returncode != 0:
        raise HarnessError("riltra --version exited %d" % proc.returncode)
    return proc.stdout.decode("utf-8").strip()


def run_split(manifest, split, final, repo, riltra, work):
    """Measure one split; returns the output lines (a run header, one row per task, a summary)."""
    if split not in ("tune", "heldout"):
        raise HarnessError("--split must be tune or heldout")
    if split == "heldout" and not final:
        raise HarnessError("--split heldout needs --final; PR 8 never runs the held-out split")
    header, rows, digest = load_manifest(manifest)
    selected = [row for row in rows if row["split"] == split]
    riltra_digest = sha256_file(riltra)
    version = riltra_version(riltra)
    os.makedirs(work, exist_ok=True)
    run_header = {"label": header["label"], "split": split, "rules_version": RULES_VERSION,
                  "manifest_sha256": digest, "riltra_sha256": riltra_digest,
                  "riltra_version": version, "tasks": len(selected)}
    lines = [json.dumps({"run": run_header}, separators=(",", ":"))]
    results = []
    for row in selected:
        result = measure_task(repo, riltra, work, row)
        results.append(result)
        lines.append(json.dumps(result, separators=(",", ":")))
    lines.append(json.dumps({"summary": summarize(results)}, separators=(",", ":")))
    return lines


def main(argv=None):
    parser = argparse.ArgumentParser(prog="harness.py")
    sub = parser.add_subparsers(dest="command", required=True)
    fz = sub.add_parser("freeze")
    fz.add_argument("--repo", required=True)
    fz.add_argument("--tip", required=True)
    fz.add_argument("--label", required=True)
    fz.add_argument("--out", required=True)
    rn = sub.add_parser("run")
    rn.add_argument("--split", required=True, choices=["tune", "heldout"])
    rn.add_argument("--final", action="store_true")
    rn.add_argument("--manifest", required=True)
    rn.add_argument("--repo", required=True)
    rn.add_argument("--riltra", required=True)
    rn.add_argument("--work", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "freeze":
            lines = [json.dumps(freeze(args.repo, args.tip, args.label, args.out), separators=(",", ":"))]
        else:
            lines = run_split(args.manifest, args.split, args.final, args.repo, os.path.abspath(args.riltra) if args.riltra else None, args.work)
    except HarnessError as error:
        sys.stderr.write("REFUSED: %s\n" % error)
        return 3
    sys.stdout.buffer.write(("\n".join(lines) + "\n").encode("utf-8"))
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
