#!/usr/bin/env bash
# Reject outgoing commits that add, change, or retain local-only documentation.
set -u
set +x

if [[ $# -gt 2 ]]; then
    echo 'public-doc pre-push: unexpected remote arguments; refusing push' >&2
    exit 2
fi
push_remote_name="${1:-}"
push_remote_url="${2:-}"
if [[ -z "$push_remote_url" && "$push_remote_name" == *://* ]]; then
    push_remote_url="$push_remote_name"
fi

object_format="$(git rev-parse --show-object-format=storage 2>/dev/null)" || object_format=''
case "$object_format" in
    sha1) expected_sha_length=40 ;;
    sha256) expected_sha_length=64 ;;
    *)
        echo 'public-doc pre-push: cannot determine Git object format; refusing push' >&2
        exit 2
        ;;
esac

is_supported_remote_url() {
    local url=$1 remainder path
    [[ "$url" != *\?* && "$url" != *\#* ]] || return 1
    case "$url" in
        https://*)
            remainder=${url#https://}
            remainder=${remainder#*@}
            [[ "$remainder" == github.com/* ]] || return 1
            path="/${remainder#github.com/}"
            ;;
        ssh://*)
            remainder=${url#ssh://}
            remainder=${remainder#*@}
            [[ "$remainder" == github.com/* ]] || return 1
            path="/${remainder#github.com/}"
            ;;
        git://github.com/*)
            path="/${url#git://github.com/}"
            ;;
        git@github.com:*)
            path="/${url#git@github.com:}"
            ;;
        *) return 1 ;;
    esac
    path=${path%/}
    path=${path%.git}
    path=$(printf '%s' "$path" | tr '[:upper:]' '[:lower:]')
    [[ "$path" == /terapetralabs/riltra || "$path" == /ryanaraaful/riltra ]]
}

root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo 'public-doc pre-push: cannot locate repository root' >&2
    exit 2
}
guard="$root/security/check-public-doc-boundary.sh"
if [[ ! -f "$guard" ]]; then
    echo "public-doc pre-push: boundary checker not found: $guard" >&2
    exit 2
fi

failed=0
line_number=0
while IFS=' ' read -r local_ref local_sha remote_ref remote_sha extra || \
    [[ -n "${local_ref:-}${local_sha:-}${remote_ref:-}${remote_sha:-}${extra:-}" ]]; do
    line_number=$((line_number + 1))
    local_sha="${local_sha%$'\r'}"
    remote_sha="${remote_sha%$'\r'}"

    if [[ -z "${local_ref:-}" && -z "${local_sha:-}" && -z "${remote_ref:-}" && -z "${remote_sha:-}" ]]; then
        continue
    fi
    if [[ -z "${local_ref:-}" || -z "${local_sha:-}" || -z "${remote_ref:-}" || -z "${remote_sha:-}" || -n "${extra:-}" ]]; then
        printf 'public-doc pre-push: malformed ref update on input line %s; refusing push\n' "$line_number" >&2
        failed=1
        continue
    fi
    if [[ ! "$local_sha" =~ ^[0-9a-fA-F]+$ || ! "$remote_sha" =~ ^[0-9a-fA-F]+$ || \
          ${#local_sha} -ne $expected_sha_length || ${#remote_sha} -ne $expected_sha_length ]]; then
        printf 'public-doc pre-push: invalid object ID on input line %s; refusing push\n' "$line_number" >&2
        failed=1
        continue
    fi

    if [[ "$local_sha" =~ ^0+$ ]]; then
        # A remote deletion uploads no commit or tree.
        continue
    fi

    local_commit="$(git rev-parse --verify --quiet "$local_sha^{commit}" 2>/dev/null)" || local_commit=''
    if [[ -z "$local_commit" ]]; then
        printf 'public-doc pre-push: local target for %s is not an available commit; refusing push\n' "$local_ref" >&2
        failed=1
        continue
    fi

    if [[ "$remote_sha" =~ ^0+$ ]]; then
        default_ref='refs/remotes/origin/master'
        default_commit="$(git rev-parse --verify --quiet "$default_ref^{commit}" 2>/dev/null)" || default_commit=''
        if [[ -z "$default_commit" ]]; then
            printf 'public-doc pre-push: %s is unavailable for new branch %s; fetch origin/master and retry\n' "$default_ref" "$local_ref" >&2
            failed=1
            continue
        fi
        if ! is_supported_remote_url "$push_remote_url"; then
            printf 'public-doc pre-push: new branch %s targets an unsupported or unverified remote; refusing push\n' "$local_ref" >&2
            failed=1
            continue
        fi
        live_master="$(GIT_TERMINAL_PROMPT=0 git ls-remote "$push_remote_url" refs/heads/master 2>/dev/null)" || live_master=''
        live_master=${live_master%$'\r'}
        if [[ -z "$live_master" || "$live_master" == *$'\n'* ]]; then
            printf 'public-doc pre-push: could not verify the live master for new branch %s; refusing push\n' "$local_ref" >&2
            failed=1
            continue
        fi
        live_sha=''
        live_ref=''
        live_extra=''
        read -r live_sha live_ref live_extra <<< "$live_master"
        if [[ ! "${live_sha:-}" =~ ^[0-9a-fA-F]+$ || ${#live_sha} -ne $expected_sha_length || \
              "${live_ref:-}" != refs/heads/master || -n "${live_extra:-}" ]]; then
            printf 'public-doc pre-push: live master response is invalid for new branch %s; refusing push\n' "$local_ref" >&2
            failed=1
            continue
        fi
        live_commit="$(git rev-parse --verify --quiet "$live_sha^{commit}" 2>/dev/null)" || live_commit=''
        if [[ -z "$live_commit" ]]; then
            printf 'public-doc pre-push: live master is not available locally for new branch %s; fetch and retry\n' "$local_ref" >&2
            failed=1
            continue
        fi
        if [[ "$live_commit" != "$default_commit" ]]; then
            printf 'public-doc pre-push: local %s differs from the live remote master; fetch and retry\n' "$default_ref" >&2
            failed=1
            continue
        fi
        base="$(git merge-base "$default_commit" "$local_commit" 2>/dev/null)" || base=''
        if [[ -z "$base" ]]; then
            printf 'public-doc pre-push: no merge-base between %s and %s; refusing push\n' "$default_ref" "$local_ref" >&2
            failed=1
            continue
        fi
    else
        remote_commit="$(git rev-parse --verify --quiet "$remote_sha^{commit}" 2>/dev/null)" || remote_commit=''
        if [[ -z "$remote_commit" ]]; then
            printf 'public-doc pre-push: remote target for %s is unavailable locally; fetch it and retry\n' "$remote_ref" >&2
            failed=1
            continue
        fi
        base="$remote_commit"
    fi

    if ! bash "$guard" "$base" "$local_commit"; then
        printf 'public-doc pre-push: rejected update %s -> %s\n' "$local_ref" "$remote_ref" >&2
        failed=1
    fi
done

exit "$failed"
