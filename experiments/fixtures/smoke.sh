#!/bin/sh
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/umbra-fixtures.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
result=0
for command in open-libc open-svc fork-write posix-spawn-write exec-write grandchild-write dup-inherit-write; do
    case "$command" in
        fork-write) content=fork ;;
        grandchild-write) content=grandchild ;;
        dup-inherit-write) content=dup ;;
        *) content=libc ;;
    esac
    printf '%s\n' "$content" > "$tmp/expected"
    if ./umbra-test-child "$command" "$tmp/$command" > "$tmp/stdout" 2> "$tmp/stderr" &&
       cmp -s "$tmp/expected" "$tmp/$command"; then
        printf 'PASS %s\n' "$command"
    else
        printf 'FAIL %s\n' "$command"
        cat "$tmp/stdout" "$tmp/stderr" >&2
        result=1
    fi
done
exit "$result"
