#!/usr/bin/env bash
# migrate-archive.sh — move a directory of git repositories into floe, each one proven.
#
#   scripts/migrate-archive.sh [--owner archive] [--big 5GiB] [--lfs-content] [--dry-run] ARCHIVE_DIR
#
# For every repository found under ARCHIVE_DIR (a working tree with .git/, or a bare repo), in
# turn: `git fsck --full` the source → `floe import` (`--direct` from one bitmap'd pack above
# --big) with its LFS objects → `floe verify --against` the source from a cold cache. Writes
# ARCHIVE_DIR/.floe-migrate/report.tsv; a repository that verified once is skipped on a re-run
# (delete its `<name>.ok` to redo it). Exit 1 if any repository did not verify.
# docs/MIGRATION.md is the runbook. The floe config comes from $FLOE_CONFIG (default floe.toml).
set -uo pipefail

owner=archive
big_bytes=$((5 * 1024 * 1024 * 1024))
verify_lfs=()
dry_run=0
floe=${FLOE_BIN:-floe}

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
to_bytes() {
    local v=$1 n u
    n=${v%%[!0-9]*}; u=${v#"$n"}
    case "$u" in
        "" | B) echo "$n" ;;
        K | KiB) echo $((n * 1024)) ;;
        M | MiB) echo $((n * 1024 * 1024)) ;;
        G | GiB) echo $((n * 1024 * 1024 * 1024)) ;;
        *) echo "bad size: $v" >&2; exit 2 ;;
    esac
}
while [ $# -gt 0 ]; do
    case "$1" in
        --owner) owner=$2; shift 2 ;;
        --big) big_bytes=$(to_bytes "$2") || exit 2; shift 2 ;;
        --lfs-content) verify_lfs=(--lfs-content); shift ;;
        --dry-run) dry_run=1; shift ;;
        -h | --help) usage ;;
        -*) echo "unknown option $1" >&2; usage ;;
        *) break ;;
    esac
done
[ $# -eq 1 ] || usage
archive=$(cd "$1" && pwd) || exit 2
state="$archive/.floe-migrate"
mkdir -p "$state"
report="$state/report.tsv"
[ -f "$report" ] || printf 'repo\tsource\tbytes\tmode\tresult\tseconds\n' > "$report"

# Repositories: every git dir (HEAD + objects/ + refs/); a `.git` dir stands for its working
# tree. A repository inside another one (a submodule's .git/modules/…, a nested clone) is not
# listed: migrate those separately.
find_repos() {
    find "$archive" -path "$state" -prune -o -type f -name HEAD -print 2>/dev/null |
        while read -r h; do
            d=$(dirname "$h")
            [ -d "$d/objects" ] && [ -d "$d/refs" ] || continue
            if [ "$(basename "$d")" = .git ]; then dirname "$d"; else echo "$d"; fi
        done | sort -u | awk 'NR == 1 || index($0, prev "/") != 1 { print; prev = $0 }'
}
# floe repository names: [A-Za-z0-9._-], no leading dot, ≤ 100 chars (D5).
repo_name() {
    local b
    b=$(basename "$1"); b=${b%.git}
    b=$(printf '%s' "$b" | tr -c 'A-Za-z0-9._-' '-' | sed 's/^[.-]*//' | cut -c1-100)
    echo "${b:-repo}"
}
git_dir_of() { if [ -d "$1/.git" ]; then echo "$1/.git"; else echo "$1"; fi; }
dir_bytes() { du -sk "$1" | awk '{ print $1 * 1024 }'; }

failed=0; done_n=0; skipped=0
seen="$state/.names-this-run"   # name<TAB>source; bash 3.2 (macOS) has no associative arrays
: > "$seen"
while read -r src; do
    name=$(repo_name "$src")
    id="$owner/$name"
    other=$(awk -F '\t' -v n="$name" '$1 == n { print $2; exit }' "$seen")
    if [ -n "$other" ]; then
        echo "!! $src: name $id already taken by $other — rename one and re-run" >&2
        printf '%s\t%s\t-\t-\tname-clash\t0\n' "$id" "$src" >> "$report"; failed=1; continue
    fi
    printf '%s\t%s\n' "$name" "$src" >> "$seen"
    if [ -f "$state/$name.ok" ]; then skipped=$((skipped + 1)); continue; fi
    gd=$(git_dir_of "$src")
    bytes=$(dir_bytes "$gd/objects")
    mode=import; [ "$bytes" -ge "$big_bytes" ] && mode=direct
    echo "== $id  ($src, $((bytes / 1024 / 1024)) MiB, $mode)"
    if [ "$dry_run" = 1 ]; then continue; fi
    t0=$(date +%s)
    result=ok
    log="$state/$name.log"
    {
        echo "# $(date -u +%FT%TZ) $src -> $id ($mode)"
        if [ -f "$gd/shallow" ]; then
            echo "shallow clone: history incomplete; git -C '$gd' fetch --unshallow, or migrate its origin"
            result=shallow-source
        elif ! git -C "$gd" fsck --full --no-dangling --no-progress; then
            result=source-corrupt
        elif [ "$mode" = direct ]; then
            packs="$state/$name.packs"
            rm -rf "$packs"; mkdir -p "$packs"
            # One pack with a bitmap, every ref's closure (what `import --direct` publishes best).
            git -C "$gd" pack-objects --all --write-bitmap-index --threads=0 "$packs/pack" < /dev/null > /dev/null &&
                "$floe" import --direct --from "$src" --packs "$packs" "$id" || result=import-failed
        else
            "$floe" import --from "$src" "$id" || result=import-failed
        fi
        if [ "$result" = ok ]; then
            "$floe" verify "$id" --against "$src" ${verify_lfs[@]+"${verify_lfs[@]}"} || result=NOT-VERIFIED
        fi
    } > "$log" 2>&1
    secs=$(($(date +%s) - t0))
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "$src" "$bytes" "$mode" "$result" "$secs" >> "$report"
    if [ "$result" = ok ]; then
        touch "$state/$name.ok"; rm -rf "$state/$name.packs"; done_n=$((done_n + 1))
        echo "   verified in ${secs}s"
    else
        failed=1
        echo "   $result — see $log" >&2
        tail -5 "$log" | sed 's/^/   | /' >&2
    fi
done < <(find_repos)

echo "migrated+verified: $done_n, already done: $skipped, report: $report"
exit "$failed"
