# SPDX-License-Identifier: Apache-2.0
# Syntax-fact benchmark: compares `riltra scan` with hand-written expectations.
# usage: python -I syntax_bench.py <abs riltra binary> <abs fixtures/syntax dir> <abs scratch dir> [lang ...]
# Default languages: js py go java (supported today).
# Fixture: <lang>/<name>.<ext> + <name>.expect.json = {"defs":[[name,line]],"imports":[line],"calls":[[text,line]]}
# Lines are 1-based statement/definition start lines. A scanned call matches when its name (as written)
# equals the expected text or ends with it after a `.`, `::` or `->` separator (`os.getcwd` for `getcwd`). Expectations are written from the source, before looking at scan output.
# Exit 0 only when every language has no FN and no FP.
import glob, json, os, shutil, subprocess, sys

B, FIX, WORK = sys.argv[1:4]
langs = sys.argv[4:] or ["js", "py", "go", "java"]
os.environ["RILTRA_HOME"] = os.path.join(WORK, "home")
bad = False
for lang in langs:
    repo = os.path.join(WORK, "s_" + lang)
    shutil.rmtree(repo, ignore_errors=True); os.makedirs(repo)
    exp = {}
    for j in sorted(glob.glob(os.path.join(FIX, lang, "*.expect.json"))):
        stem = os.path.basename(j)[:-len(".expect.json")]
        src = next(p for p in glob.glob(os.path.join(FIX, lang, stem + ".*")) if not p.endswith(".expect.json"))
        shutil.copy(src, repo); exp[os.path.basename(src)] = json.load(open(j, encoding="utf-8"))
    subprocess.run([B, "init", "--repo", repo, "--name", "s_" + lang], check=True, capture_output=True)
    scan = json.loads(subprocess.run([B, "scan", "--repo", "s_" + lang], check=True, capture_output=True, text=True).stdout)
    files = {f["path"]: f for f in scan["files"]}
    tot = {k: [0, 0, 0] for k in ("defs", "imports", "calls")}  # tp, fp, fn
    notes = []
    for fn, e in exp.items():
        f = files.get(fn)
        if not f or f["status"] != "ok":
            notes.append(f"{fn}: status={f['status'] if f else 'missing'} (unsupported)")
            f = {"definitions": [], "imports": [], "calls": []}
        got = {"defs": [(d["name"], d["span"]["start_line"]) for d in f["definitions"]],
               "imports": [i["span"]["start_line"] for i in f["imports"]],
               "calls": [(c["name"], c["span"]["start_line"]) for c in f["calls"]]}
        for k in tot:
            want = [w if k == "imports" else tuple(w) for w in e[k]]
            have = list(got[k])
            for w in list(want):
                if k == "calls": hit = next((h for h in have if h[1] == w[1] and (h[0] == w[0] or any(h[0].endswith(s + w[0]) for s in (".", "::", "->")))), None)
                else: hit = w if w in have else None
                if hit is not None: want.remove(w); have.remove(hit); tot[k][0] += 1
            tot[k][2] += len(want); tot[k][1] += len(have)
            notes += [f"{fn} {k} FN {w}" for w in want] + [f"{fn} {k} FP {h}" for h in have]
    row = " ".join(f"{k}={t[0]}tp/{t[1]}fp/{t[2]}fn" for k, t in tot.items())
    print(f"{lang}: {row}")
    for n in notes: print("   ", n)
    bad |= any(t[1] or t[2] for t in tot.values())
sys.exit(1 if bad else 0)
