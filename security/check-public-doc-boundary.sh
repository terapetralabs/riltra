#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <base-commit> <head-commit>" >&2
    exit 2
fi

base=$(git rev-parse --verify "$1^{commit}")
head=$(git rev-parse --verify "$2^{commit}")
comparison_base=$(git merge-base "$base" "$head" || true)
if [[ -z "$comparison_base" ]]; then
    echo "public-doc boundary: base and head have no common ancestor" >&2
    exit 2
fi

is_local_only() {
    local LC_ALL=C
    local path=${1,,} segment rest local_doc
    local -a local_docs=(
        ROADMAP.md
        docs/hybrid-architecture-roadmap.md
        docs/documentation-changelog.md
        docs/audits/2026-10-19-windows-wal-concurrency.md
        open_source_code_intelligence_knowledge_blueprint.md
        security/history-rewrite-plan.md
        docs/codex/2026-11-05-codex-readiness-design.md
        docs/codex/2026-11-05-codex-readiness-plan.md
        docs/m0-verification.md
        docs/m1-verification.md
        docs/m2-validation.md
        docs/m3-validation.md
        docs/m4-validation.md
        docs/m5-validation.md
        docs/m6-task4-validation.md
        docs/m6-task5-validation.md
        docs/m6-task6-validation.md
        docs/m6-task7-correction-validation.md
        docs/m6-task7-validation.md
        docs/m6-validation.md
        docs/m7-validation.md
        docs/m8-design-gate.md
        docs/m8-final-acceptance-review.md
        docs/m8-opencode-first-amendment.md
        docs/m8-owner-acceptance.md
        docs/m9-clangd-mcp-implementation.md
        docs/m9-clangd-mcp-integration-design.md
        docs/m9-clangd-pilot.md
        docs/m9-consolidation.md
        docs/m9-cp1-pilot-spec.md
        docs/m9-cp2-evidence.md
        docs/m9-cp2-harness-correction.md
        docs/m9-cp2-reevaluation.md
        docs/m9-cp3-evidence.md
        docs/m9-cp4-evidence.md
        docs/m9-cpp-semantic-index-decision-2026-10-25.md
        docs/m9-cpp-semantic-index-spike-2026-10-25.md
        docs/m9-design-gate.md
        docs/m9-e7-implementation.md
        docs/m9-e7-reference-resolution-design.md
        docs/m9-e7-slice1-implementation.md
        docs/m9-e7-slice2-evidence-and-decision.md
        docs/m9-e7-slice2-implementation-spec.md
        docs/m9-gknw-amendment.md
        docs/m9-grader-correction-amendment-2026-10-25.md
        docs/m9-grf-transparency-amendment.md
        docs/m9-gsem-amendment.md
        docs/m9-gsyn-import-span-amendment.md
        docs/m9-inl-amendment.md
        docs/m9-lsp-hardening.md
        docs/m9-preprelease-acceptance-amendment.md
        docs/m9-release-candidate-gate-amendment-2026-10-26.md
        docs/m9-task5-capability-gap-evaluation.md
        docs/m9-task6-final-verification.md
        docs/m10-consolidation.md
        docs/m10-design-gate.md
        docs/m10-phase-b-amendment-2026-11-04.md
        docs/m10-task5-decision.md
        docs/riltra-correctness-followup-2026-11-02.md
        docs/riltra-e0s-semantic-reliability-2026-10-29.md
        docs/riltra-impact-evidence-design-2026-10-31.md
        docs/riltra-multilanguage-baseline-2026-11-03.md
        docs/riltra-multilanguage-evaluation-plan-2026-11-03.md
        docs/riltra-rust-alias-correctness-2026-11-03.md
        docs/riltra-rust-semantic-feasibility-2026-10-31.md
        docs/riltra-what-impact-why-implementation.md
        docs/riltra-wiy-correctness-evaluation-2026-10-31.md
        docs/riltra-wiy-correctness-plan-2026-10-31.md
        docs/riltra-wiy-followup-2026-10-29.md
        docs/riltra-wiy-full-project-recheck-2026-10-31.md
        docs/riltra-wiy-research-2026-10-31.md
        docs/pilots/m6-measure.ps1
        docs/pilots/m6-profile.cjs
        docs/pilots/m6-real-providers.cjs
        docs/pilots/m6-windows-evidence.json
        docs/pilots/wiy-caller-precision-2026-10-31/REPORT.md
        docs/pilots/wiy-caller-precision-2026-10-31/results/a.json
        docs/pilots/wiy-caller-precision-2026-10-31/results/b.json
        docs/pilots/wiy-caller-precision-2026-10-31/results/import.json
        docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/MANIFEST.sha256
        docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture/callers.json
        docs/pilots/m10/task3-codex-sanitized-20261026t162908300z.zip
        docs/pilots/m10/task4-opencode-sanitized-20261027.zip
        docs/pilots/cp2-harness-fix/README.md
    )

    for local_doc in "${local_docs[@]}"; do
        [[ "$path" == "${local_doc,,}" ]] && return 0
    done

    case "$path" in */open_source_code_intelligence_knowledge_blueprint.md) return 0 ;; esac
    case "$path" in docs/superpowers/*) return 0 ;; esac
    case "$path" in docs/pilots/*)
        case "$path" in
            docs/pilots/agent-kebenaran-2026-11-02/*|docs/pilots/clangd/*|docs/pilots/cp2/*|docs/pilots/cp2-reeval/*|docs/pilots/cp3/*|docs/pilots/cp7a/*|docs/pilots/e7-slice2-evidence/*)
                return 0 ;;
            docs/pilots/m9-gknw/*|docs/pilots/m9-grf-transparency/*|docs/pilots/m9-gsyn-fix/*|docs/pilots/m9-rc/*|docs/pilots/m9-rc-ee7482d/*|docs/pilots/m9-task6/*|docs/pilots/riltra-e0s-semantic-reliability-2026-10-29/*|docs/pilots/wiy-correctness-2026-10-31/*)
                return 0 ;;
            docs/pilots/m10/task2-claude-code/*|docs/pilots/m10/task3-codex/*|docs/pilots/m10/task4-opencode/*)
                return 0 ;;
            docs/pilots/m10/task3-codex-sanitized-20261026t162908300z.zip|docs/pilots/m10/task4-opencode-sanitized-20261027.zip|docs/pilots/multilanguage-correctness-2026-11-03/*)
                return 0 ;;
            docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/manifest.sha256|docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/results/*|docs/pilots/wiy-caller-precision-2026-10-31/report.md|docs/pilots/wiy-caller-precision-2026-10-31/results/*)
                return 0 ;;
        esac
        rest=${path#docs/pilots/}
        while [[ "$rest" == */* ]]; do
            segment=${rest%%/*}
            case "$segment" in raw|raw-*|bench) return 0 ;; esac
            if [[ "$path" == docs/pilots/m10/* ]]; then
                case "$segment" in runs|freeze) return 0 ;; esac
            fi
            rest=${rest#*/}
        done
        ;;
    esac
    return 1
}

failed=0
path_file=$(mktemp)
trap 'rm -f "$path_file"' EXIT

if ! git ls-tree -rz --name-only "$head" > "$path_file"; then
    echo "public-doc boundary: could not read tip tree" >&2
    exit 2
fi
while IFS= read -r -d '' path; do
    if is_local_only "$path"; then
        printf 'public-doc boundary: protected path remains in tip tree %q\n' "$path" >&2
        failed=1
    fi
done < "$path_file"

if ! commits=$(git rev-list --reverse "$comparison_base..$head"); then
    echo "public-doc boundary: could not enumerate commits" >&2
    exit 2
fi
for commit in $commits; do
    parent=$(git rev-parse --verify "$commit^1" 2>/dev/null || true)
    if [[ -n "$parent" ]]; then
        if ! git diff-tree -z --no-commit-id --name-status -r -M "$parent" "$commit" > "$path_file"; then
            printf 'public-doc boundary: could not inspect commit %s\n' "$commit" >&2
            failed=1
            continue
        fi
    else
        if ! git diff-tree -z --root --no-commit-id --name-status -r -M "$commit" > "$path_file"; then
            printf 'public-doc boundary: could not inspect root commit %s\n' "$commit" >&2
            failed=1
            continue
        fi
    fi

    while IFS= read -r -d '' status; do
        if ! IFS= read -r -d '' first; then
            printf 'public-doc boundary: malformed diff path data in commit %s\n' "$commit" >&2
            failed=1
            break
        fi
        [[ "$status" == D ]] && continue

        if [[ "$status" == R* || "$status" == C* ]]; then
            if ! IFS= read -r -d '' second; then
                printf 'public-doc boundary: malformed rename/copy data in commit %s\n' "$commit" >&2
                failed=1
                break
            fi
            changed_paths=("$first" "$second")
        else
            changed_paths=("$first")
        fi

        for path in "${changed_paths[@]}"; do
            if is_local_only "$path"; then
                printf 'public-doc boundary: %s changes local-only path %q\n' "$commit" "$path" >&2
                failed=1
            fi
        done
    done < "$path_file"
done

if [[ "$failed" -ne 0 ]]; then
    echo "Keep internal plans, raw runs, and detailed evaluation records out of the public tree. Add only a reviewed public summary." >&2
    exit 1
fi
