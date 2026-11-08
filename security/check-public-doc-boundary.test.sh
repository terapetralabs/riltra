#!/usr/bin/env bash
set -euo pipefail
repo=''
trap '[[ -z "$repo" ]] || rm -rf "$repo"' EXIT

checker="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-public-doc-boundary.sh"
[[ -f "$checker" ]] || { echo "missing boundary checker: $checker" >&2; exit 1; }
repo_root="$(cd "$(dirname "$checker")/.." && pwd)"
licensing="$repo_root/docs/licensing.md"
grep -Fq '| clangd (LLVM) | 23.1.4 | Apache-2.0 WITH LLVM-exception |' "$licensing" || {
    echo 'public licensing inventory lost the clangd pilot attribution' >&2
    exit 1
}
if grep -Fq 'docs/pilots/clangd/method/' "$licensing"; then
    echo 'public licensing inventory still points into the local-only clangd package' >&2
    exit 1
fi

forbidden_paths=(
    ROADMAP.md
    docs/hybrid-architecture-roadmap.md
    docs/documentation-changelog.md
    docs/audits/2026-10-19-windows-wal-concurrency.md
    security/history-rewrite-plan.md
    OPEN_SOURCE_CODE_INTELLIGENCE_KNOWLEDGE_BLUEPRINT.md
    docs/pilots/demo/OPEN_SOURCE_CODE_INTELLIGENCE_KNOWLEDGE_BLUEPRINT.md
    Docs/Pilots/Demo/OPEN_SOURCE_CODE_INTELLIGENCE_KNOWLEDGE_BLUEPRINT.md
    docs/codex/2026-11-05-codex-readiness-plan.md
    docs/m9-cp2-evidence.md
    docs/riltra-wiy-followup-2026-10-29.md
    docs/pilots/demo/raw/output.json
    docs/pilots/demo/raw-private/output.json
    docs/pilots/demo/bench/results.json
    docs/pilots/m10/task2-claude-code/harness/session.sh
    docs/pilots/m10/task2-claude-code/runs/session.json
    docs/pilots/m10/task3-codex/PRECHECK.md
    docs/pilots/m10/task3-codex/pilot-20261026T162908300Z/final-review.json
    docs/pilots/m10/task4-opencode/protocol.json
    docs/pilots/m10/task4-opencode/grading/audit/A4-R-filesystem.json
    docs/pilots/m10/task4-opencode/freeze/SHA256SUMS.txt
    docs/pilots/cp7a/goldens-raw/status.json
    docs/pilots/cp7a/corpus/corpus.json
    Docs/Pilots/CP7A/Corpus/corpus.json
    docs/pilots/m10/task3-codex/pilot-20261026T162908300Z/state-history/study.json
    docs/pilots/multilanguage-correctness-2026-11-03/laporan.md
    docs/pilots/wiy-correctness-2026-10-31/ORACLE.md
    docs/pilots/wiy-correctness-2026-10-31/results/e2e-impact.stdout.json
    docs/pilots/agent-kebenaran-2026-11-02/hasil-pilot.md
    docs/pilots/clangd/method/probe.ps1
    DOCS/PILOTS/CLANGD/METHOD/PROBE.PS1
    docs/pilots/cp2/method/grade.cjs
    docs/pilots/cp2-reeval/method/opencode-session-d5.ps1
    docs/pilots/cp3/method/e8-anchor.cjs
    docs/pilots/cp7a/.gitattributes
    docs/pilots/e7-slice2-evidence/probe/src/main.rs
    docs/pilots/m9-gknw/method/g-knw.sh
    docs/pilots/m9-grf-transparency/method/cmp-grading.cjs
    docs/pilots/m9-gsyn-fix/method/import-before-after.cjs
    docs/pilots/m9-rc/comparison.md
    docs/pilots/m9-rc-ee7482d/evidence/summary.txt
    docs/pilots/m9-task6/method/query-check.cjs
    docs/pilots/riltra-e0s-semantic-reliability-2026-10-29/results/summary.json
    docs/pilots/wiy-correctness-2026-10-31/trait-fixture/src/lib.rs
    docs/pilots/wiy-caller-precision-2026-10-31/REPORT.md
    docs/pilots/wiy-caller-precision-2026-10-31/results/a.json
    docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/results/import.json
    docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture/callers.json
    docs/pilots/m6-windows-evidence.json
    docs/pilots/m10/task3-codex-sanitized-20261026T162908300Z.zip
    docs/pilots/m10/task4-opencode-sanitized-20261027.zip
)

new_repo() {
    repo=$(mktemp -d)
    git -C "$repo" init -q
    git -C "$repo" config user.name BoundaryTest
    git -C "$repo" config user.email boundary-test@example.invalid
    git -C "$repo" config core.autocrlf false
    printf 'public\n' > "$repo/README.md"
    git -C "$repo" add README.md
    git -C "$repo" commit -qm baseline
    base=$(git -C "$repo" rev-parse HEAD)
}

run_checker() {
    (cd "$repo" && bash "$checker" "$1" "$2") >/dev/null 2>&1
}

protected_path_commit() {
    local path=$1 base=$2 blob subtree readme root_tree commit basename index
    local -a segments
    IFS='/' read -r -a segments <<< "$path"
    blob=$(printf 'private\n' | git -C "$repo" hash-object -w --stdin)
    basename=${path##*/}
    subtree=$(printf '100644 blob %s\t%s\0' "$blob" "$basename" | git -C "$repo" mktree -z)
    for ((index=${#segments[@]}-2; index>=1; index-=1)); do
        subtree=$(printf '040000 tree %s\t%s\0' "$subtree" "${segments[index]}" | git -C "$repo" mktree -z)
    done
    readme=$(git -C "$repo" rev-parse "$base:README.md")
    root_tree=$(
        {
            printf '040000 tree %s\tdocs\0' "$subtree"
            printf '100644 blob %s\tREADME.md\0' "$readme"
        } | git -C "$repo" mktree -z
    )
    commit=$(printf 'add protected path\n' | git -C "$repo" commit-tree "$root_tree" -p "$base")
    printf '%s\n' "$commit"
}

for path in "${forbidden_paths[@]}"; do
    new_repo
    mkdir -p "$repo/$(dirname "$path")"
    printf 'private\n' > "$repo/$path"
    git -C "$repo" add -- "$path"
    git -C "$repo" commit -qm "add local-only path"
    head=$(git -C "$repo" rev-parse HEAD)
    if run_checker "$base" "$head"; then
        echo "boundary checker allowed $path" >&2
        exit 1
    fi
    rm -rf "$repo"
done

new_repo
mkdir -p "$repo/docs"
printf 'private\n' > "$repo/docs/m9-cp2-evidence.md"
git -C "$repo" add docs/m9-cp2-evidence.md
git -C "$repo" commit -qm 'baseline includes a protected tracked path'
base=$(git -C "$repo" rev-parse HEAD)
if run_checker "$base" "$base"; then
    echo 'boundary checker allowed a protected file already present at the tip' >&2
    exit 1
fi
rm -rf "$repo"

new_repo
mkdir -p "$repo/docs"
printf 'local\n' > "$repo/ROADMAP.md"
git -C "$repo" add ROADMAP.md
git -C "$repo" commit -qm 'baseline includes a formerly tracked local-only file'
base=$(git -C "$repo" rev-parse HEAD)
printf 'changed local\n' > "$repo/ROADMAP.md"
git -C "$repo" add ROADMAP.md
git -C "$repo" commit -qm 'modify local-only file'
head=$(git -C "$repo" rev-parse HEAD)
if run_checker "$base" "$head"; then
    echo 'boundary checker allowed a modification to a local-only path' >&2
    exit 1
fi
rm -rf "$repo"

new_repo
mkdir -p "$repo/docs"
printf 'private\n' > "$repo/docs/documentation-changelog.md"
git -C "$repo" add docs/documentation-changelog.md
git -C "$repo" commit -qm 'add then remove local-only file'
git -C "$repo" rm -q -- docs/documentation-changelog.md
git -C "$repo" commit -qm 'remove local-only file before the proposed head'
head=$(git -C "$repo" rev-parse HEAD)
if run_checker "$base" "$head"; then
    echo 'boundary checker missed a local-only path added then removed in the range' >&2
    exit 1
fi
rm -rf "$repo"

new_repo
for path in $'docs/pilots/cp7a/corpus/line\nbreak.json' $'docs/pilots/cp7a/corpus/tab\tname.json' $'docs/pilots/cp7a/corpus/unicode-\u03bb.json'; do
    private_commit=$(protected_path_commit "$path" "$base")
    if run_checker "$base" "$private_commit"; then
        echo 'boundary checker allowed a protected Unicode/tab/newline path in the tip or change range' >&2
        exit 1
    fi

    if run_checker "$private_commit" "$private_commit"; then
        echo 'boundary checker missed a protected Unicode/tab/newline path already present at the tip' >&2
        exit 1
    fi

    base_tree=$(git -C "$repo" rev-parse "$base^{tree}")
    clean_commit=$(printf 'remove protected path\n' | git -C "$repo" commit-tree "$base_tree" -p "$private_commit")
    if run_checker "$base" "$clean_commit"; then
        echo 'boundary checker missed an added then deleted Unicode/tab/newline path' >&2
        exit 1
    fi
    rm -rf "$repo"
    new_repo
done
rm -rf "$repo"

new_repo
printf 'local\n' > "$repo/ROADMAP.md"
git -C "$repo" add ROADMAP.md
git -C "$repo" commit -qm 'baseline protected roadmap before rename'
base=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" mv -- ROADMAP.md public-notes.md
git -C "$repo" commit -qm 'rename protected roadmap to public path'
head=$(git -C "$repo" rev-parse HEAD)
if run_checker "$base" "$head"; then
    echo 'boundary checker allowed a rename from a local-only path' >&2
    exit 1
fi
rm -rf "$repo"

new_repo
mkdir -p "$repo/docs/pilots/wiy-caller-precision-2026-10-31/source/src"
mkdir -p "$repo/docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture/src"
printf 'source fixture\n' > "$repo/docs/pilots/wiy-caller-precision-2026-10-31/source/src/lib.rs"
printf 'edition = "2021"\n' > "$repo/docs/pilots/wiy-caller-precision-2026-10-31/source/Cargo.toml"
printf 'pub fn closure_caller() {}\n' > "$repo/docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture/src/lib.rs"
printf 'edition = "2021"\n' > "$repo/docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture/Cargo.toml"
git -C "$repo" add docs/pilots/wiy-caller-precision-2026-10-31/source docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture
git -C "$repo" commit -qm 'keep source fixtures used by product tests'
fixture_head=$(git -C "$repo" rev-parse HEAD)
run_checker "$base" "$fixture_head" || { echo 'boundary checker rejected a source fixture required by product tests' >&2; exit 1; }

rm -rf "$repo"
new_repo
mkdir -p "$repo/docs/pilots/fixture/source"
printf 'fixture\n' > "$repo/docs/pilots/fixture/source/lib.rs"
printf 'fixture notes\n' > "$repo/docs/pilots/fixture/README.md"
git -C "$repo" add docs/pilots/fixture
git -C "$repo" commit -qm 'keep public pilot fixture'
fixture_head=$(git -C "$repo" rev-parse HEAD)
run_checker "$base" "$fixture_head" || { echo 'boundary checker rejected a public pilot fixture and its README' >&2; exit 1; }

git -C "$repo" rm -q -- docs/pilots/fixture/source/lib.rs
git -C "$repo" commit -qm 'remove public fixture'
removed=$(git -C "$repo" rev-parse HEAD)
run_checker "$fixture_head" "$removed" || { echo 'boundary checker rejected a deletion-only change' >&2; exit 1; }
rm -rf "$repo"

echo 'public-doc boundary guard tests passed'
