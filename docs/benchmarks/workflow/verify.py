# SPDX-License-Identifier: Apache-2.0
"""Independent checks for the workflow benchmark: a Python ast oracle and a caller-set scorer.

Pure functions are tested by selftest.py without a binary. The CLI subcommands (pyast,
fixtures) run riltra and print counts; they never pass or fail a measurement.

Usage: python -I verify.py pyast --riltra BIN --repo ROOT --work DIR
       python -I verify.py fixtures --riltra BIN --fixtures DIR --work DIR
"""


import argparse
import ast
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys


class VerifyError(Exception):
    """A command could not run, or its output is not the shape the check expects."""


def line_of_byte(data, offset):
    """1-based line of byte `offset`: the count of newlines before it, plus one."""
    return data[:offset].count(b"\n") + 1


def ast_definitions(source):
    """(name, kind, def_line, first_line) for every def and class at any depth, sorted.

    first_line is the smallest of the node's lineno and its decorators' linenos. A SyntaxError
    from ast.parse propagates; the caller decides what to do with it.
    """
    found = []
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            kind = "function"
        elif isinstance(node, ast.ClassDef):
            kind = "class"
        else:
            continue
        first = min([node.lineno] + [dec.lineno for dec in node.decorator_list])
        found.append((node.name, kind, node.lineno, first))
    return sorted(found, key=lambda item: (item[2], item[0]))


def _take(unused, name, line):
    """Remove and return the first unused scanned item with this name and start line, else None."""
    for index, item in enumerate(unused):
        if item[0] == name and item[1] == line:
            return unused.pop(index)
    return None


def score_defs(oracle, scanned):
    """Match Riltra definitions (name, start_line) to ast oracle items, each used at most once.

    An oracle item first looks for a scanned item at its def_line (exact), then at its first_line
    (decorator-line). Oracle items are processed in order. Returns counts plus sorted lists of
    missing (name, def_line) and extra (name, start_line) items.
    """
    unused = list(scanned)
    exact = 0
    decorator_line = 0
    missing = []
    for name, _kind, def_line, first_line in oracle:
        if _take(unused, name, def_line) is not None:
            exact += 1
        elif _take(unused, name, first_line) is not None:
            decorator_line += 1
        else:
            missing.append((name, def_line))
    return {"oracle": len(oracle), "exact": exact, "decorator_line": decorator_line,
            "missing": sorted(missing), "extra": sorted(unused)}


def site_records(doc, repo_root, command):
    """One record per distinct caller site: path, line, and basis, candidate_scope and candidates
    when the document carries them. command is "impact" (distance-1 paths) or "callers" (edges).
    The line is computed from the caller's byte_start in the file under repo_root; the first
    record for a site wins. Records are sorted by (path, line).
    """
    if command == "impact":
        pairs = [(entry, entry["edges"][0]) for entry in doc["paths"] if entry["distance"] == 1]
    elif command == "callers":
        pairs = [(None, edge) for edge in doc["edges"]]
    else:
        raise VerifyError("site_records: unknown command " + command)
    records = {}
    for entry, edge in pairs:
        caller = edge.get("caller") if isinstance(edge, dict) else None
        if not isinstance(caller, dict):
            raise VerifyError("a %s edge has no caller endpoint" % command)
        with open(os.path.join(repo_root, caller["path"]), "rb") as handle:
            data = handle.read()
        line = line_of_byte(data, caller["byte_start"])
        key = (caller["path"], line)
        if key in records:
            continue
        record = {"path": caller["path"], "line": line}
        for field in ("basis", "candidate_scope", "candidates"):
            for source in (edge, entry):
                if source is not None and field in source:
                    record[field] = source[field]
                    break
        records[key] = record
    return [records[key] for key in sorted(records)]


def caller_sites(doc, repo_root):
    """(path, line) of each distance-1 caller in an impact document, sorted and de-duplicated."""
    return [(record["path"], record["line"]) for record in site_records(doc, repo_root, "impact")]


def score_callers(expected, decoys, actual):
    """Compare returned (path, line) caller sites with the truth; decoys are known non-callers."""
    truth = set(expected)
    returned = set(actual)
    decoy_set = set(decoys)
    return {
        "truth": len(truth),
        "returned": len(returned),
        "found": sorted(truth & returned),
        "missed": sorted(truth - returned),
        "extra_decoy": sorted((returned - truth) & decoy_set),
        "extra_other": sorted(returned - truth - decoy_set),
    }


def run_riltra(binary, args, env, label):
    """stdout bytes of one riltra call; a call that cannot start or exits non-zero is a VerifyError."""
    try:
        proc = subprocess.run([binary] + args, capture_output=True, env=env)
    except OSError as error:
        raise VerifyError("%s: cannot start riltra (%s)" % (label, error.strerror))
    if proc.returncode != 0:
        raise VerifyError("%s: riltra exited %d" % (label, proc.returncode))
    return proc.stdout


def parse_json(data, label):
    try:
        return json.loads(data.decode("utf-8"))
    except ValueError:
        raise VerifyError("%s: output is not JSON" % label)


def emit(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")


def new_work(work):
    if os.path.exists(work):
        raise VerifyError("work directory already exists; choose a new one")
    os.makedirs(work)


def git_py_files(root):
    proc = subprocess.run(["git", "-C", root, "ls-files", "*.py"], capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("git ls-files exited %d" % proc.returncode)
    return sorted(proc.stdout.decode("utf-8").splitlines())


def cmd_pyast(binary, root, work):
    files = git_py_files(root)
    new_work(work)
    repo = os.path.join(work, "repo")
    home = os.path.join(work, "home")
    os.makedirs(repo)
    os.makedirs(home)
    env = dict(os.environ, RILTRA_HOME=home)
    oracles = {}
    skipped = []
    for rel in files:
        with open(os.path.join(root, rel), "rb") as handle:
            data = handle.read()
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            raise VerifyError("not UTF-8, cannot parse: " + rel)
        try:
            oracle = ast_definitions(text)
        except SyntaxError:
            skipped.append(rel)
            continue
        target = os.path.join(repo, *rel.split("/"))
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as handle:
            handle.write(data)
        oracles[rel] = oracle
    run_riltra(binary, ["init", "--repo", repo, "--name", "t"], env, "pyast init")
    scan = parse_json(run_riltra(binary, ["scan", "--repo", "t"], env, "pyast scan"), "pyast scan")
    totals = {"oracle": 0, "exact": 0, "decorator_line": 0, "missing": 0, "extra": 0}
    reported = set()
    not_ok = []
    scored = 0
    for entry in scan["files"]:
        path = entry["path"]
        reported.add(path)
        if entry["status"] != "ok":
            not_ok.append({"path": path, "status": entry["status"]})
            continue
        if path not in oracles:
            raise VerifyError("scan reported a file that was not copied: " + path)
        scanned = [(d["name"], d["span"]["start_line"]) for d in entry["definitions"]]
        score = score_defs(oracles[path], scanned)
        emit({"path": path, "oracle": score["oracle"], "exact": score["exact"],
              "decorator_line": score["decorator_line"], "missing": score["missing"], "extra": score["extra"]})
        scored += 1
        totals["oracle"] += score["oracle"]
        totals["exact"] += score["exact"]
        totals["decorator_line"] += score["decorator_line"]
        totals["missing"] += len(score["missing"])
        totals["extra"] += len(score["extra"])
    not_reported = sorted(set(oracles) - reported)
    if not_reported:
        raise VerifyError("scan did not report %d copied file(s), first: %s" % (len(not_reported), not_reported[0]))
    emit({"summary": dict(totals, files_listed=len(files), files_scored=scored,
                          files_skipped_syntax_error=len(skipped), skipped_syntax_error_paths=skipped,
                          scan_not_ok=not_ok)})


def run_fixture_case(binary, lang, case, case_dir, work):
    label = lang + "/" + case
    with open(os.path.join(case_dir, "expect.json"), "rb") as handle:
        expect = parse_json(handle.read(), label + " expect.json")
    base = os.path.join(work, lang, case)
    repo = os.path.join(base, "repo")
    home = os.path.join(base, "home")
    os.makedirs(repo)
    os.makedirs(home)
    for name in sorted(os.listdir(case_dir)):
        source = os.path.join(case_dir, name)
        if name != "expect.json" and os.path.isfile(source):
            shutil.copyfile(source, os.path.join(repo, name))
    env = dict(os.environ, RILTRA_HOME=home)
    run_riltra(binary, ["init", "--repo", repo, "--name", "t"], env, label + " init")
    run_riltra(binary, ["index", "--repo", "t"], env, label + " index")
    anchor = expect["anchor"]
    truth = [(item["path"], item["line"]) for item in expect["callers"]]
    decoys = [(item["path"], item["line"]) for item in expect["not_callers"]]
    anchor_args = ["--path", anchor["path"], "--line", str(anchor["line"]), "--col", str(anchor["col"])]
    for command in ("impact", "callers"):
        if command == "impact":
            args = ["impact", "--repo", "t"] + anchor_args + ["--depth", "1"]
        else:
            args = ["callers", "--repo", "t"] + anchor_args
        doc = parse_json(run_riltra(binary, args, env, label + " " + command), label + " " + command)
        records = site_records(doc, repo, command)
        actual = [(record["path"], record["line"]) for record in records]
        line = {"lang": lang, "case": case, "command": command, "score": score_callers(truth, decoys, actual)}
        for key in ("verdict", "unresolved"):
            if key in doc:
                line[key] = doc[key]
        line["sites"] = records
        emit(line)


def cmd_fixtures(binary, fixtures, work):
    new_work(work)
    with open(binary, "rb") as handle:
        digest = hashlib.sha256(handle.read()).hexdigest()
    version = run_riltra(binary, ["--version"], dict(os.environ), "--version").decode("utf-8").strip()
    emit({"header": {"riltra_sha256": digest, "riltra_version": version}})
    for lang in sorted(os.listdir(fixtures)):
        lang_dir = os.path.join(fixtures, lang)
        if not os.path.isdir(lang_dir):
            continue
        for case in sorted(os.listdir(lang_dir)):
            case_dir = os.path.join(lang_dir, case)
            if os.path.isdir(case_dir) and os.path.isfile(os.path.join(case_dir, "expect.json")):
                run_fixture_case(binary, lang, case, case_dir, work)


HUNK_RE = re.compile("^@@ -([0-9]+)(?:,([0-9]+))? [+]([0-9]+)(?:,([0-9]+))? @@(.*)$")
NON_FILE_STATUSES = ("binary", "unsupported", "parse-error")
WHOLE_FILE_STATUSES = ("added", "deleted")
ANCHOR_NAMES = ("impact_query", "edge_query", "trace_query", "validated_trace_anchor", "called_name",
                "open_live", "parse_source", "push_def", "new")
REPLAY_TIP = "1120b74"
REPLAY_RULE = ("commits with a parent that changed a file whose lowercase extension is in "
               "harness.SUPPORTED_EXTS (git log --no-merges --no-renames), sorted by full sha, first 30")
IDENTITY_RULE = ("per line of crates/cli/src/*.rs: optional leading spaces, optional pub or pub(...), "
                 "optional const, then fn, one or more spaces, NAME as a whole word; new also excludes scip_proto.rs")


def _diff_path(text, side):
    """Path from a ---/+++ header value; side is "a" or "b". Returns None for /dev/null."""
    if text == "/dev/null":
        return None
    if text.endswith("\t"):
        text = text[:-1]
    if not text.startswith(side + "/"):
        raise VerifyError("unexpected diff header path")
    return text[2:]


def parse_hunk_contexts(diff_text):
    """Map each file's path to its hunk contexts in order (text after the second @@, stripped; "" if none).

    The path is the +++ path, or the --- path for a deletion (+++ /dev/null). A hunk body is skipped by the
    line counts in its header, so a removed line such as "-- note" is never read as a file header.
    """
    lines = diff_text.split("\n")
    contexts = {}
    path = None
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        if line.startswith("--- ") and index < len(lines) and lines[index].startswith("+++ "):
            old_value = line[4:]
            new_value = lines[index][4:]
            index += 1
            if new_value == "/dev/null":
                path = _diff_path(old_value, "a")
            else:
                path = _diff_path(new_value, "b")
            contexts.setdefault(path, [])
        elif line.startswith("@@"):
            match = HUNK_RE.match(line)
            if match is None or path is None:
                raise VerifyError("unparsable hunk header, or a hunk before any file header")
            old_count = int(match.group(2)) if match.group(2) is not None else 1
            new_count = int(match.group(4)) if match.group(4) is not None else 1
            contexts[path].append(match.group(5).strip())
            index += old_count + new_count
    return contexts


def _side_range(start_text, count_text):
    """(start, start + count - 1) for one side of a hunk, or None when its count is zero; a missing count is 1."""
    start = int(start_text)
    count = 1 if count_text is None else int(count_text)
    return (start, start + count - 1) if count > 0 else None


def parse_hunk_ranges(diff_text):
    """Map each file's path to its hunks as line ranges: [{"old": (start, end) or None, "new": ...}, ...].

    The path is the +++ path, or the --- path for a deletion. A hunk body is skipped by the line counts in its
    header, as in parse_hunk_contexts, so a removed line such as "-- note" is never read as a file header.
    """
    lines = diff_text.split("\n")
    ranges = {}
    path = None
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        if line.startswith("--- ") and index < len(lines) and lines[index].startswith("+++ "):
            old_value = line[4:]
            new_value = lines[index][4:]
            index += 1
            if new_value == "/dev/null":
                path = _diff_path(old_value, "a")
            else:
                path = _diff_path(new_value, "b")
            ranges.setdefault(path, [])
        elif line.startswith("@@"):
            match = HUNK_RE.match(line)
            if match is None or path is None:
                raise VerifyError("unparsable hunk header, or a hunk before any file header")
            old_count = 1 if match.group(2) is None else int(match.group(2))
            new_count = 1 if match.group(4) is None else int(match.group(4))
            ranges[path].append({"old": _side_range(match.group(1), match.group(2)),
                                 "new": _side_range(match.group(3), match.group(4))})
            index += old_count + new_count
    return ranges


def _overlaps(span, line_range):
    """True when a definition span {start_line, end_line} and a hunk range (start, end) share a line."""
    if span is None or line_range is None:
        return False
    return span["start_line"] <= line_range[1] and line_range[0] <= span["end_line"]


def check_file_ranges(entry, hunks):
    """Check one changed file's definitions and hunk count against git's line ranges, report-only.

    entry is one element of a changes document's files list; hunks are the parse_hunk_ranges entries for its
    path. An added or modified definition intersects when its new span overlaps a hunk's new range; a removed
    or modified one when its old span overlaps a hunk's old range (a modified one checks both). A hunk is
    outside when no listed definition overlaps either of its ranges on the matching side.
    """
    definitions = entry["definitions"]
    intersecting = 0
    for definition in definitions:
        change = definition["change"]
        hit = False
        if change in ("added", "modified"):
            hit = hit or any(_overlaps(definition["new_span"], h["new"]) for h in hunks)
        if change in ("removed", "modified"):
            hit = hit or any(_overlaps(definition["old_span"], h["old"]) for h in hunks)
        if hit:
            intersecting += 1
    outside = 0
    for h in hunks:
        touches = any(_overlaps(d["new_span"], h["new"]) or _overlaps(d["old_span"], h["old"])
                      for d in definitions)
        if not touches:
            outside += 1
    return {"definitions": len(definitions), "definitions_intersecting": intersecting,
            "hunks_equal": len(hunks) == entry["hunks"],
            "outside_equal": outside == len(entry["hunks_outside_definitions"])}


def _names_in(name, text):
    """True when name occurs in text as a whole word (no identifier character on either side)."""
    pattern = "(?<![A-Za-z0-9_])" + re.escape(name) + "(?![A-Za-z0-9_])"
    return re.search(pattern, text) is not None


def classify_file(status, definitions, contexts):
    """Compare git's hunk contexts with riltra's definition names for one changed file.

    An empty-string context means "no git context": only non-empty contexts count as matched or unmatched.
    The counts are over non-empty contexts and over all names. Classes: status:<status> for binary,
    unsupported and parse-error; whole-file:<status> for added and deleted; otherwise agree, no-git-context,
    no-riltra-definitions, both-empty or disagree, as the brief's rules state.
    """
    named = [text for text in contexts if text != ""]
    unmatched_contexts = sum(1 for text in named if not any(_names_in(n, text) for n in definitions))
    unmatched_names = sum(1 for n in definitions if not any(_names_in(n, text) for text in named))
    if status in NON_FILE_STATUSES:
        cls = "status:" + status
    elif status in WHOLE_FILE_STATUSES:
        cls = "whole-file:" + status
    elif named and definitions and unmatched_contexts == 0 and unmatched_names == 0:
        cls = "agree"
    elif definitions and not named:
        cls = "no-git-context"
    elif named and not definitions:
        cls = "no-riltra-definitions"
    elif not named and not definitions:
        cls = "both-empty"
    else:
        cls = "disagree"
    return {"class": cls, "unmatched_contexts": unmatched_contexts, "unmatched_names": unmatched_names}


def select_commits(records, is_supported, limit=None):
    """Sorted full shas of commits that have a parent and changed a supported file; first `limit` if given.

    records are (sha, has_parent, changed_paths) tuples.
    """
    kept = sorted(sha for sha, has_parent, paths in records if has_parent and any(is_supported(p) for p in paths))
    return kept if limit is None else kept[:limit]


def summarize_callers(doc):
    """Edge count, counts by kind, and whether any edge carries a candidates field. A missing kind counts as none."""
    edges = doc.get("edges")
    if not isinstance(edges, list):
        raise VerifyError("callers document has no edges list")
    by_kind = {}
    for edge in edges:
        kind = edge.get("kind")
        if kind is None:
            kind = "none"
        by_kind[kind] = by_kind.get(kind, 0) + 1
    return {"edges": len(edges), "by_kind": dict(sorted(by_kind.items())),
            "candidates_field": any("candidates" in edge for edge in edges)}


def summarize_impact(doc):
    """Path count, verdict and completeness as the document states them (None when absent), counts by basis.

    A missing basis counts as none; nothing else is inferred.
    """
    paths = doc.get("paths")
    if not isinstance(paths, list):
        raise VerifyError("impact document has no paths list")
    by_basis = {}
    for entry in paths:
        basis = entry.get("basis")
        if basis is None:
            basis = "none"
        by_basis[basis] = by_basis.get(basis, 0) + 1
    return {"paths": len(paths), "verdict": doc.get("verdict"), "completeness": doc.get("completeness"),
            "by_basis": dict(sorted(by_basis.items()))}


def anchor_matches(name, sources):
    """Sorted (path, line, identifier_col, keyword_col) for every line that declares fn NAME.

    sources maps a repository-relative path to its text. Columns and lines are 1-based. The line must start,
    after spaces, with an optional pub or pub(...), an optional const, then fn; NAME must be a whole word.
    """
    pattern = re.compile("(?P<pre>[ ]*(?:pub(?:[(][a-z]+[)])?[ ]+)?(?:const[ ]+)?)fn(?P<gap>[ ]+)"
                         + re.escape(name) + "(?![A-Za-z0-9_])")
    found = []
    for path in sorted(sources):
        for number, text in enumerate(sources[path].split("\n"), 1):
            match = pattern.match(text)
            if match is not None:
                pre = len(match.group("pre"))
                gap = len(match.group("gap"))
                found.append((path, number, pre + gap + 3, pre + 1))
    return found


def stderr_class(text):
    """The first four words of the first non-empty stderr line, dropping tokens that look like paths."""
    for line in text.splitlines():
        if line.strip():
            words = [w for w in line.split() if not any(ch in w for ch in "/" + chr(92) + ".")][:4]
            return " ".join(words) if words else "unnamed"
    return "none"


def definition_found(search_doc, path, line):
    """True when a search result has this path and this start line."""
    results = search_doc.get("results")
    if not isinstance(results, list):
        raise VerifyError("search document has no results list")
    return any(item.get("path") == path and (item.get("span") or {}).get("start_line") == line
               for item in results)


def write_once(path, data):
    """Write new bytes to a path that does not exist yet; never overwrite a raw output."""
    if os.path.exists(path):
        raise VerifyError("refusing to overwrite " + os.path.basename(path))
    with open(path, "wb") as handle:
        handle.write(data)


def run_capture(binary, args, env, label):
    """(exit code, stdout bytes, stderr bytes) of one riltra call; a call that cannot start is a VerifyError."""
    try:
        proc = subprocess.run([binary] + args, capture_output=True, env=env)
    except OSError as error:
        raise VerifyError("%s: cannot start riltra (%s)" % (label, error.strerror))
    return proc.returncode, proc.stdout, proc.stderr


def run_required(binary, args, env, label):
    """stdout bytes of a setup call (init, index); a non-zero exit stops the run."""
    code, body, _err = run_capture(binary, args, env, label)
    if code != 0:
        raise VerifyError("%s: riltra exited %d" % (label, code))
    return body


def binary_info(binary):
    with open(binary, "rb") as handle:
        digest = hashlib.sha256(handle.read()).hexdigest()
    code, body, _err = run_capture(binary, ["--version"], dict(os.environ), "--version")
    if code != 0:
        raise VerifyError("--version exited %d" % code)
    return {"sha256": digest, "version": body.decode("utf-8").strip()}


def tree_head(tree):
    proc = subprocess.run(["git", "-C", tree, "rev-parse", "HEAD"], capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("git rev-parse exited %d" % proc.returncode)
    return proc.stdout.decode("utf-8").strip()


def harness_is_supported():
    """harness.is_supported, imported from the folder beside this file (python -I does not add it)."""
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
    import harness
    return harness.is_supported


def list_commit_records(tree, tip):
    """(records, parents): records are (sha, has_parent, changed paths) for non-merge commits reachable from tip."""
    proc = subprocess.run(["git", "-C", tree, "log", "--no-merges", "--no-renames", "--format=@@%H %P",
                           "--name-only", tip], capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("git log exited %d" % proc.returncode)
    records = []
    parents = {}
    for line in proc.stdout.decode("utf-8", "replace").splitlines():
        if line.startswith("@@"):
            fields = line[2:].split()
            records.append([fields[0], len(fields) > 1, []])
            if len(fields) > 1:
                parents[fields[0]] = fields[1]
        elif line.strip():
            records[-1][2].append(line)
    return records, parents


def checkout_detached(rp, sha):
    proc = subprocess.run(["git", "-C", rp, "checkout", "-q", "--detach", sha], capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("git checkout exited %d" % proc.returncode)


def git_diff_text(rp, parent, sha, paths):
    """Zero-context diff of the supported paths between parent and sha; "" when there are none."""
    if not paths:
        return ""
    args = ["git", "-C", rp, "-c", "core.quotepath=off", "diff", "-U0", "--no-ext-diff", "--no-textconv",
            "--no-color", "--no-renames", parent, sha, "--"] + paths
    proc = subprocess.run(args, capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("git diff exited %d" % proc.returncode)
    return proc.stdout.decode("utf-8", "replace")


def cmd_replay(binary, tree, work, out):
    is_supported = harness_is_supported()
    info = binary_info(binary)
    records, parents = list_commit_records(tree, REPLAY_TIP)
    paths_of = {sha: paths for sha, _has_parent, paths in records}
    kept_all = select_commits(records, is_supported)
    kept = kept_all[:30]
    rp = os.path.join(work, "rp")
    home = os.path.join(work, "rp-home")
    replay_dir = os.path.join(out, "replay")
    if not os.path.isdir(rp):
        raise VerifyError("replay worktree is missing; create it with git worktree add --detach first")
    if os.path.exists(home) or os.path.exists(replay_dir):
        raise VerifyError("replay home or output already exists; choose a new work or output directory")
    os.makedirs(home)
    os.makedirs(replay_dir)
    env = dict(os.environ, RILTRA_HOME=home)
    run_required(binary, ["init", "--repo", rp, "--name", "r"], env, "replay init")
    emit({"header": {"binary_sha256": info["sha256"], "binary_version": info["version"],
                     "tree_commit": tree_head(tree), "selection": REPLAY_RULE,
                     "kept": len(kept_all), "first_30": [sha[:12] for sha in kept]}})
    totals = {}
    files_total = 0
    hunks_total = 0
    truncated = 0
    failed = []
    for sha in kept:
        short = sha[:12]
        parent = parents[sha]
        checkout_detached(rp, sha)
        changed = sorted(p for p in paths_of[sha] if is_supported(p))
        contexts = parse_hunk_contexts(git_diff_text(rp, parent, sha, changed))
        hunks = sum(len(values) for values in contexts.values())
        hunks_total += hunks
        code, body, err = run_capture(binary, ["changes", "--repo", "r", "--scope", "compare", "--base", parent,
                                               "--limit", "1000"], env, "changes " + short)
        write_once(os.path.join(replay_dir, short + "-changes.json"), body)
        write_once(os.path.join(replay_dir, short + "-changes.stderr.txt"), err)
        row = {"commit": short, "hunks": hunks}
        doc = None
        if code == 0:
            try:
                doc = json.loads(body.decode("utf-8"))
            except ValueError:
                doc = None
        if doc is None:
            failed.append(short)
            row.update({"files": 0, "classes": {}, "changes_exit": code,
                        "stderr_class": stderr_class(err.decode("utf-8", "replace")) if code != 0 else "not-json"})
        else:
            files = doc.get("files")
            if not isinstance(files, list):
                raise VerifyError("changes document has no files list")
            classes = {}
            for entry in files:
                # Every definition is used. The brief's filter (change is not null) is a no-op: the changes
                # document always carries change as a string ("modified", "added" or "removed").
                names = [item["name"] for item in entry["definitions"]]
                result = classify_file(entry["status"], names, contexts.get(entry["path"], []))
                classes[result["class"]] = classes.get(result["class"], 0) + 1
                totals[result["class"]] = totals.get(result["class"], 0) + 1
            files_total += len(files)
            if doc.get("truncated") is True:
                truncated += 1
            row.update({"files": len(files), "classes": dict(sorted(classes.items())), "changes_exit": 0,
                        "stderr_class": None})
        emit(row)
    emit({"summary": {"commits_run": len(kept), "kept": len(kept_all), "classes": dict(sorted(totals.items())),
                      "files": files_total, "hunks": hunks_total, "truncated_documents": truncated,
                      "changes_failed": failed}})


def read_sources(tree):
    """Text of every crates/cli/src/*.rs file in a tree, keyed by repository-relative path."""
    src_dir = os.path.join(tree, "crates", "cli", "src")
    sources = {}
    for name in sorted(os.listdir(src_dir)):
        if name.endswith(".rs"):
            with open(os.path.join(src_dir, name), "rb") as handle:
                sources["crates/cli/src/" + name] = handle.read().decode("utf-8")
    return sources


def resolve_anchors(sources):
    """Anchor entries for one tree: ok with path, line and columns; anchor not found; or anchor ambiguous."""
    entries = []
    for name in ANCHOR_NAMES:
        if name == "new":
            pool = {path: text for path, text in sources.items() if not path.endswith("scip_proto.rs")}
            matches = anchor_matches(name, pool)
            if not matches:
                entries.append({"anchor": name, "name": name, "status": "anchor not found", "matches": []})
            for number, (path, line, ident, kw) in enumerate(matches, 1):
                entries.append({"anchor": "new-%d" % number, "name": name, "status": "ok", "path": path,
                                "line": line, "ident_col": ident, "kw_col": kw,
                                "label": "retrospective only"})
            continue
        matches = anchor_matches(name, sources)
        if len(matches) == 1:
            path, line, ident, kw = matches[0]
            entries.append({"anchor": name, "name": name, "status": "ok", "path": path, "line": line,
                            "ident_col": ident, "kw_col": kw})
        elif not matches:
            entries.append({"anchor": name, "name": name, "status": "anchor not found", "matches": []})
        else:
            entries.append({"anchor": name, "name": name, "status": "anchor ambiguous",
                            "matches": [[path, line] for path, line, _i, _k in matches]})
    return entries


def search_verdict(binary, env, entry, out, label):
    stem = "%s-%s-search" % (label, entry["anchor"])
    code, body, err = run_capture(binary, ["search", "--repo", "r", "--symbol", entry["name"], "--limit", "50"],
                                  env, label + " search " + entry["anchor"])
    write_once(os.path.join(out, stem + ".json"), body)
    write_once(os.path.join(out, stem + ".stderr.txt"), err)
    if code != 0:
        return {"found": False, "problem": "exit %d" % code}
    try:
        doc = json.loads(body.decode("utf-8"))
    except ValueError:
        return {"found": False, "problem": "non-json"}
    return {"found": definition_found(doc, entry["path"], entry["line"]), "problem": None,
            "total": doc.get("total")}


def command_row(label, entry, column, command, code, body, err):
    row = {"tree": label, "anchor": entry["anchor"], "column": column, "command": command, "exit": code,
           "stderr_class": stderr_class(err.decode("utf-8", "replace")), "problem": None, "summary": None}
    if code != 0:
        row["problem"] = "exit %d" % code
    else:
        try:
            doc = json.loads(body.decode("utf-8"))
        except ValueError:
            row["problem"] = "non-json"
        else:
            row["summary"] = summarize_callers(doc) if command == "callers" else summarize_impact(doc)
    return row


def measure_tree(label, binary, tree, home, out, entries):
    """Rows for one tree: index status, search verdicts, and callers/impact summaries per anchor and column."""
    env = dict(os.environ, RILTRA_HOME=home)
    os.makedirs(home)
    run_required(binary, ["init", "--repo", tree, "--name", "r"], env, label + " init")
    run_required(binary, ["index", "--repo", "r"], env, label + " index")
    code, body, err = run_capture(binary, ["index", "status", "--repo", "r"], env, label + " index status")
    if code != 0:
        raise VerifyError("%s index status exited %d" % (label, code))
    write_once(os.path.join(out, label + "-index-status.json"), body)
    status_doc = parse_json(body, label + " index status")
    snapshot = status_doc.get("snapshot") or {}
    rows = [{"tree": label, "index_freshness": status_doc.get("freshness"),
             "snapshot_head": snapshot.get("head"), "tree_head": tree_head(tree)}]
    for entry in entries:
        if entry["status"] != "ok":
            continue
        rows.append({"tree": label, "anchor": entry["anchor"],
                     "search": search_verdict(binary, env, entry, out, label)})
        for column, key in (("identifier", "ident_col"), ("keyword", "kw_col")):
            for command in ("callers", "impact"):
                args = [command, "--repo", "r", "--path", entry["path"], "--line", str(entry["line"]),
                        "--col", str(entry[key]), "--limit", "1000"]
                code, body, err = run_capture(binary, args, env, label + " " + entry["anchor"])
                stem = "%s-%s-%s-%s" % (label, entry["anchor"], column, command)
                write_once(os.path.join(out, stem + ".json"), body)
                write_once(os.path.join(out, stem + ".stderr.txt"), err)
                rows.append(command_row(label, entry, column, command, code, body, err))
    return rows


def anchor_sort_key(anchor):
    base, _sep, number = anchor.partition("-")
    return (ANCHOR_NAMES.index(base), int(number) if number else 0)


def pair_lines(before_rows, after_rows):
    """Pairs of (anchor, column, command) with the before and after summary, or the problem, or no row."""
    def index(rows):
        return {(row["anchor"], row["column"], row["command"]): row for row in rows if "command" in row}

    def view(row):
        if row is None:
            return {"problem": "no row"}
        return row["summary"] if row["summary"] is not None else {"problem": row["problem"]}

    before = index(before_rows)
    after = index(after_rows)
    keys = sorted(set(before) | set(after),
                  key=lambda k: (anchor_sort_key(k[0]), k[1], k[2]))
    return [{"anchor": a, "column": c, "command": m, "before": view(before.get((a, c, m))),
             "after": view(after.get((a, c, m)))} for a, c, m in keys]


def write_sums(out):
    lines = []
    for name in sorted(os.listdir(out)):
        path = os.path.join(out, name)
        if os.path.isfile(path) and name != "SHA256SUMS":
            with open(path, "rb") as handle:
                lines.append("%s  %s\n" % (hashlib.sha256(handle.read()).hexdigest(), name))
    write_once(os.path.join(out, "SHA256SUMS"), "".join(lines).encode("utf-8"))


def cmd_anchors(before_bin, after_bin, before_tree, after_tree, work, out):
    if os.path.exists(os.path.join(out, "SHA256SUMS")):
        raise VerifyError("SHA256SUMS already exists; choose a new output directory")
    before_home = os.path.join(work, "home-before")
    after_home = os.path.join(work, "home-after")
    if os.path.exists(before_home) or os.path.exists(after_home):
        raise VerifyError("home-before or home-after already exists; choose a new work directory")
    os.makedirs(out, exist_ok=True)
    before_info = binary_info(before_bin)
    after_info = binary_info(after_bin)
    before_entries = resolve_anchors(read_sources(before_tree))
    after_entries = resolve_anchors(read_sources(after_tree))
    emit({"header": {
        "binary_before": before_info, "binary_after": after_info,
        "tree_before": tree_head(before_tree), "tree_after": tree_head(after_tree),
        "anchors": list(ANCHOR_NAMES), "identity": IDENTITY_RULE,
        "verification": "search --repo r --symbol NAME --limit 50; the definition path and start line must appear",
        "commands": "callers and impact with --repo r --path P --line L --col C --limit 1000, no other flags",
        "columns": ["identifier", "keyword"],
        "statement": ("The two trees differ, so a change in a count can come from the code under test or from "
                      "the repository changing, and no difference is attributed to one cause.")}})
    for label, entries in (("before", before_entries), ("after", after_entries)):
        for entry in entries:
            emit({"tree": label, "identity": entry})
    before_rows = measure_tree("before", before_bin, before_tree, before_home, out, before_entries)
    after_rows = measure_tree("after", after_bin, after_tree, after_home, out, after_entries)
    for row in before_rows + after_rows:
        emit(row)
    for pair in pair_lines(before_rows, after_rows):
        emit({"pair": pair})
    problems = ["%s %s %s %s: %s" % (row["tree"], row["anchor"], row["column"], row["command"], row["problem"])
                for row in before_rows + after_rows if row.get("problem")]
    unfound = ["%s %s" % (row["tree"], row["anchor"]) for row in before_rows + after_rows
               if "search" in row and not row["search"]["found"]]
    emit({"summary": {"problem_rows": problems, "search_not_found": unfound}})
    write_sums(out)


def full_sha(tree, short):
    proc = subprocess.run(["git", "-C", tree, "rev-parse", short], capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("replay-check: git rev-parse failed for commit " + short)
    return proc.stdout.decode("utf-8").strip()


def commit_diff_text(tree, full):
    """Zero-context diff of every file between a commit and its first parent, as git prints it."""
    args = ["git", "-C", tree, "-c", "core.quotepath=off", "diff", "-U0", "--no-ext-diff", "--no-textconv",
            "--no-color", "--no-renames", full + "^", full]
    proc = subprocess.run(args, capture_output=True)
    if proc.returncode != 0:
        raise VerifyError("replay-check: git diff failed for commit " + full[:12])
    return proc.stdout.decode("utf-8", "replace")


def cmd_replay_check(tree, replay_dir):
    """Compare each saved changes document with git's hunk line ranges for its commit. Report-only."""
    shorts = sorted(name[:-len("-changes.json")] for name in os.listdir(replay_dir)
                    if name.endswith("-changes.json"))
    emit({"header": {"tree_commit": tree_head(tree), "commits": len(shorts)}})
    totals = {"files": 0, "definitions": 0, "definitions_intersecting": 0,
              "files_hunks_equal": 0, "files_outside_equal": 0}
    inconsistent = []
    for short in shorts:
        full = full_sha(tree, short)
        ranges = parse_hunk_ranges(commit_diff_text(tree, full))
        with open(os.path.join(replay_dir, short + "-changes.json"), "rb") as handle:
            doc = parse_json(handle.read(), "replay-check " + short)
        files = doc.get("files") if isinstance(doc, dict) else None
        if not isinstance(files, list):
            raise VerifyError("replay-check %s: changes document has no files list" % short)
        for index, entry in enumerate(files):
            if entry["status"] != "modified" or entry["symbols"] != "established":
                continue
            result = check_file_ranges(entry, ranges.get(entry["path"], []))
            totals["files"] += 1
            totals["definitions"] += result["definitions"]
            totals["definitions_intersecting"] += result["definitions_intersecting"]
            totals["files_hunks_equal"] += 1 if result["hunks_equal"] else 0
            totals["files_outside_equal"] += 1 if result["outside_equal"] else 0
            if not (result["hunks_equal"] and result["outside_equal"]):
                inconsistent.append("%s:%d" % (short, index))
    emit({"summary": dict(totals, inconsistent=inconsistent)})


def main(argv=None):
    parser = argparse.ArgumentParser(description="Independent oracle and caller-set checks for the workflow benchmark.")
    sub = parser.add_subparsers(dest="command", required=True)
    py = sub.add_parser("pyast")
    py.add_argument("--riltra", required=True)
    py.add_argument("--repo", required=True)
    py.add_argument("--work", required=True)
    fx = sub.add_parser("fixtures")
    fx.add_argument("--riltra", required=True)
    fx.add_argument("--fixtures", required=True)
    fx.add_argument("--work", required=True)
    rp = sub.add_parser("replay")
    rp.add_argument("--binary", required=True)
    rp.add_argument("--tree", required=True)
    rp.add_argument("--work", required=True)
    rp.add_argument("--out", required=True)
    an = sub.add_parser("anchors")
    an.add_argument("--before-binary", required=True)
    an.add_argument("--after-binary", required=True)
    an.add_argument("--before-tree", required=True)
    an.add_argument("--after-tree", required=True)
    an.add_argument("--work", required=True)
    an.add_argument("--out", required=True)
    rc = sub.add_parser("replay-check")
    rc.add_argument("--tree", required=True)
    rc.add_argument("--replay-dir", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "replay":
            cmd_replay(os.path.abspath(args.binary), os.path.abspath(args.tree),
                       os.path.abspath(args.work), os.path.abspath(args.out))
        elif args.command == "anchors":
            cmd_anchors(os.path.abspath(args.before_binary), os.path.abspath(args.after_binary),
                        os.path.abspath(args.before_tree), os.path.abspath(args.after_tree),
                        os.path.abspath(args.work), os.path.abspath(args.out))
        elif args.command == "replay-check":
            cmd_replay_check(os.path.abspath(args.tree), os.path.abspath(args.replay_dir))
        elif args.command == "pyast":
            cmd_pyast(os.path.abspath(args.riltra), os.path.abspath(args.repo), os.path.abspath(args.work))
        else:
            cmd_fixtures(os.path.abspath(args.riltra), os.path.abspath(args.fixtures), os.path.abspath(args.work))
    except VerifyError as error:
        sys.stderr.write("VERIFY FAILED: %s\n" % error)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
