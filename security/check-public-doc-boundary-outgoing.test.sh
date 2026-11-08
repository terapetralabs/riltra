#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
checker="$repo_root/security/check-public-doc-boundary-outgoing.sh"
guard="$repo_root/security/check-public-doc-boundary.sh"
scan="$repo_root/security/scan.sh"
pre_push="$repo_root/.githooks/pre-push"
real_git="$(command -v git)"
canonical_url='https://github.com/terapetralabs/riltra.git'
renamed_url='https://test-user:test-secret@github.com/RyanAraaful/riltra.git'
repo=''
trap '[[ -z "$repo" ]] || rm -rf "$repo"' EXIT

fail() {
    echo "pre-push boundary test: $*" >&2
    exit 1
}

new_repo() {
    [[ -z "$repo" ]] || rm -rf "$repo"
    repo="$(mktemp -d)"
    mkdir -p "$repo/security" "$repo/.githooks" "$repo/bin" "$repo/hooktmp"
    cp "$guard" "$repo/security/check-public-doc-boundary.sh"
    chmod -x "$repo/security/check-public-doc-boundary.sh"
    cp "$scan" "$repo/security/scan.sh"
    cp "$pre_push" "$repo/.githooks/pre-push"
    cp "$repo_root/.gitleaks.toml" "$repo/.gitleaks.toml"
    cp "$checker" "$repo/security/check-public-doc-boundary-outgoing.sh"
    chmod -x "$repo/security/check-public-doc-boundary-outgoing.sh"
    cat > "$repo/bin/gitleaks" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${GITLEAKS_LOG:?}"
STUB
    cat > "$repo/bin/git" <<'GITWRAP'
#!/usr/bin/env bash
if [[ "${1:-}" == ls-remote ]]; then
    if [[ "${2:-}" == https://github.com/terapetralabs/riltra.git || \
          "${2:-}" == https://test-user:test-secret@github.com/RyanAraaful/riltra.git ]]; then
        if [[ -n "${TEST_MASTER_SHA:-}" ]]; then
            printf '%s\trefs/heads/master\n' "$TEST_MASTER_SHA"
            exit 0
        fi
    fi
    exit 1
fi
exec "$REAL_GIT" "$@"
GITWRAP
    chmod +x "$repo/bin/git"
    chmod +x "$repo/bin/gitleaks"
    export REAL_GIT="$real_git"
    export TEST_MASTER_SHA=''
    export PATH="$repo/bin:$PATH"
    git -C "$repo" init -q -b master
    git -C "$repo" config user.name BoundaryTest
    git -C "$repo" config user.email boundary-test@example.invalid
    git -C "$repo" config core.autocrlf false
    printf 'public baseline\n' > "$repo/README.md"
    git -C "$repo" add README.md
    git -C "$repo" commit -qm baseline
    base="$(git -C "$repo" rev-parse HEAD)"
    git -C "$repo" update-ref refs/remotes/origin/master "$base"
    TEST_MASTER_SHA="$base"
    export TEST_MASTER_SHA
    zeros='0000000000000000000000000000000000000000'
    export GITLEAKS_BIN=gitleaks
    export GITLEAKS_LOG="$repo/gitleaks.log"
}

write_commit() {
    local path=$1 text=$2 message=$3
    mkdir -p "$repo/$(dirname "$path")"
    printf '%s\n' "$text" > "$repo/$path"
    git -C "$repo" add -- "$path"
    git -C "$repo" commit -qm "$message"
    git -C "$repo" rev-parse HEAD
}

run_checker() {
    local input=$1
    shift
    (cd "$repo" && bash "$checker" "$@") <<< "$input"
}

expect_checker_failure() {
    local label=$1 input=$2 output="$repo/output.txt"
    shift 2
    if run_checker "$input" "$@" >"$output" 2>&1; then
        fail "$label was accepted"
    fi
}

[[ -f "$checker" ]] || fail "missing outgoing boundary checker"

new_repo
git -C "$repo" checkout -qb legacy "$base"
legacy_private="$(write_commit ROADMAP.md 'previously published fixture' legacy-protected-commit)"
git -C "$repo" update-ref refs/remotes/origin/legacy "$legacy_private"
git -C "$repo" rm -q -- ROADMAP.md
git -C "$repo" commit -qm 'remove local-only file from current tip'
legacy_clean="$(git -C "$repo" rev-parse HEAD)"
run_checker "refs/heads/legacy $legacy_clean refs/heads/legacy $legacy_private" >/dev/null || \
    fail 'existing remote SHA was not used as the update base for a deletion-only cleanup'

git -C "$repo" checkout -qb feature "$base"
safe_head="$(write_commit docs/public.md 'A public release note.' safe-branch-commit)"
feature_line="refs/heads/feature $safe_head refs/heads/feature $zeros"
run_checker "$feature_line" origin "$canonical_url" >/dev/null || fail 'new branch from canonical origin/master was rejected'
run_checker "$feature_line" origin "$renamed_url" >/dev/null || fail 'authenticated renamed canonical URL was rejected'
run_checker "$feature_line" "$canonical_url" "$canonical_url" >/dev/null || fail 'direct canonical URL push was rejected'
unsupported_url='https://test-user:test-secret@example.invalid/riltra.git'
expect_checker_failure 'new branch to an unsupported remote' "$feature_line" backup "$unsupported_url"
if grep -Fq 'test-secret' "$repo/output.txt"; then fail 'remote credentials leaked in an error'; fi
expect_checker_failure 'new branch without remote identity' "$feature_line"
TEST_MASTER_SHA=''
export TEST_MASTER_SHA
expect_checker_failure 'new branch when live master cannot be read' "$feature_line" origin "$canonical_url"
grep -Fq 'could not verify the live master' "$repo/output.txt" || fail 'unavailable remote-head error was not actionable'
TEST_MASTER_SHA="$safe_head"
export TEST_MASTER_SHA
expect_checker_failure 'new branch with stale local origin/master' "$feature_line" origin "$canonical_url"
grep -Fq 'differs from the live remote master' "$repo/output.txt" || {
    sed 's/test-secret/[REDACTED]/g' "$repo/output.txt" >&2
    fail 'stale-base error was not actionable'
}
TEST_MASTER_SHA="$base"
export TEST_MASTER_SHA

protected_head="$(write_commit ROADMAP.md 'internal roadmap fixture' protected-branch-commit)"
protected_line="refs/heads/feature $protected_head refs/heads/feature $zeros"
expect_checker_failure 'new branch with a protected tip path' "$protected_line"

remote_base="$base"
expect_checker_failure 'existing remote branch update containing a protected path' \
    "refs/heads/feature $protected_head refs/heads/feature $remote_base"

git -C "$repo" update-ref -d refs/remotes/origin/master
run_checker "refs/heads/feature $zeros refs/heads/feature $protected_head" >/dev/null || \
    fail 'branch deletion required a path-guard base'
expect_checker_failure 'branch creation without origin/master' "$feature_line" origin "$canonical_url"
grep -Fq 'refs/remotes/origin/master' "$repo/output.txt" || fail 'missing-base error was not actionable'

git -C "$repo" update-ref refs/remotes/origin/master "$base"
git -C "$repo" checkout --orphan unrelated >/dev/null 2>&1
git -C "$repo" rm -rf -q .
unrelated_head="$(write_commit docs/unrelated.md 'unrelated history' unrelated-commit)"
expect_checker_failure 'branch creation with no common base' \
    "refs/heads/unrelated $unrelated_head refs/heads/unrelated $zeros" origin "$canonical_url"
grep -Fq 'no merge-base' "$repo/output.txt" || fail 'no-merge-base error was not actionable'

expect_checker_failure 'existing update with unavailable remote SHA' \
    "refs/heads/feature $safe_head refs/heads/feature 1111111111111111111111111111111111111111"

git -C "$repo" branch private "$protected_head"
multiple_lines="$feature_line
refs/heads/private $protected_head refs/heads/private $zeros"
expect_checker_failure 'multiple refs where one is protected' "$multiple_lines"
bad_line='refs/heads/broken not-a-sha refs/heads/broken 0000000000000000000000000000000000000000'
expect_checker_failure 'malformed local SHA' "$bad_line"
short_line='refs/heads/short deadbeef refs/heads/short 00000000'
expect_checker_failure 'abbreviated object IDs' "$short_line"
grep -Fq 'invalid object ID' "$repo/output.txt" || fail 'abbreviated SHA error was not actionable'

# The actual hook must run both checks with identical input, even when the path guard rejects it.
git -C "$repo" checkout -q feature
rm -f "$repo/gitleaks.log"
if ! (cd "$repo" && TMPDIR="$repo/hooktmp" bash .githooks/pre-push "$renamed_url" "$renamed_url") <<< "$feature_line" >"$repo/hook-success.out" 2>&1; then
    fail 'pre-push hook rejected a public update with a non-executable dispatcher file'
fi
[[ "$(grep -c -- '--log-opts=' "$repo/gitleaks.log")" -eq 1 ]] || \
    fail 'successful pre-push hook did not run the security scan once'
if find "$repo/hooktmp" -mindepth 1 -print -quit | grep -q .; then
    fail 'successful pre-push input spool was not cleaned up'
fi

rm -f "$repo/gitleaks.log"
if (cd "$repo" && TMPDIR="$repo/hooktmp" bash .githooks/pre-push "$renamed_url" "$renamed_url") <<< "$multiple_lines" >"$repo/hook.out" 2>&1; then
    fail 'pre-push hook accepted a protected ref'
fi
[[ -f "$repo/gitleaks.log" ]] || fail 'security scan was skipped after boundary rejection'
[[ "$(grep -c -- '--log-opts=' "$repo/gitleaks.log")" -eq 2 ]] || \
    fail 'security scan did not receive both push refs'
if find "$repo/hooktmp" -mindepth 1 -print -quit | grep -q .; then
    fail 'pre-push input spool was not cleaned up'
fi

echo 'pre-push boundary tests passed'
