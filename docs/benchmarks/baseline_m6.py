# SPDX-License-Identifier: Apache-2.0
# usage: python -I baseline_m6.py <riltra-binary> <tests/fixtures/m6> <scratch-dir>   (absolute paths; prints JSON)
import json, os, shutil, subprocess, sys, glob
B = sys.argv[1]            # riltra binary
FIX = sys.argv[2]          # tests/fixtures/m6
WORK = sys.argv[3]         # scratch dir
os.environ["RILTRA_HOME"] = os.path.join(WORK, "home")
res = {}
for lang in ["py", "ts", "rs", "c", "cpp"]:
    repo = os.path.join(WORK, "r_" + lang)
    shutil.rmtree(repo, ignore_errors=True); os.makedirs(repo)
    mans = {}
    for j in sorted(glob.glob(os.path.join(FIX, lang, "*.json"))):
        m = json.load(open(j, encoding="utf-8"))
        if not os.path.exists(os.path.join(FIX, lang, m["file"])): continue
        mans[m["file"]] = m
        shutil.copy(os.path.join(FIX, lang, m["file"]), repo)
    subprocess.run([B, "init", "--repo", repo, "--name", "b_" + lang], check=True, capture_output=True)
    out = subprocess.run([B, "scan", "--repo", "b_" + lang], check=True, capture_output=True, text=True).stdout
    scan = json.loads(out); files = {f["path"]: f for f in scan["files"]}
    r = dict(files=len(mans), unsupported=0, parse_err_files=0, def_exp=0, def_tp=0, def_scan=0, def_fn=[], def_extra=[],
             call_exp=0, call_tp=0, call_fn=[], call_scan=0, ref_cases_excluded=0)
    for fn, m in mans.items():
        f = files.get(fn)
        if not f or f["status"] != "ok":
            r["unsupported"] += 1; continue
        if f["errors"]: r["parse_err_files"] += 1
        names = {s["id"]: s["name"] for s in m["symbols"]}
        defs, calls = f["definitions"], f["calls"]
        r["def_scan"] += len(defs); r["call_scan"] += len(calls)
        matched = set()
        for o in m["occurrences"]:
            L, nm = o["line"] + 1, o["text"]
            if o["definition"]:
                r["def_exp"] += 1
                hit = next((i for i, d in enumerate(defs) if d["name"] == nm and d["span"]["start_line"] <= L <= d["span"]["end_line"] and i not in matched), None)
                if hit is None: r["def_fn"].append(f"{fn}:{L}:{nm}")
                else: r["def_tp"] += 1; matched.add(hit)
        for i, d in enumerate(defs):
            if i not in matched: r["def_extra"].append(f"{fn}:{d['span']['start_line']}:{d['name']}")
        # call recall only on cases with zero non-call references; every non-def occurrence is a call
        if m.get("inbound_references") == 0:
            for o in m["occurrences"]:
                if o["definition"]: continue
                r["call_exp"] += 1
                L, C = o["line"] + 1, o["col"] + 1
                ok = any(o["text"] in c["name"] and c["span"]["start_line"] <= L <= c["span"]["end_line"] for c in calls)
                if ok: r["call_tp"] += 1
                else: r["call_fn"].append(f"{fn}:{L}:{o['text']}")
        else: r["ref_cases_excluded"] += 1
    res[lang] = r
json.dump(res, sys.stdout, indent=1)
