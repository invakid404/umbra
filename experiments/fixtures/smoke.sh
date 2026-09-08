#!/bin/sh
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/umbra-fixtures.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
result=0
for command in open-libc open-svc fork-write posix-spawn-write exec-write grandchild-write dup-inherit-write --wnohang-wait; do
    case "$command" in
        fork-write) content=fork ;;
        grandchild-write) content=grandchild ;;
        dup-inherit-write) content=dup ;;
        --wnohang-wait) content=wnohang ;;
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
# --dirfd-rename takes a directory rather than an output path, and leaves its
# result at <root>/db/c. Untraced it runs against the host, so give it a real
# directory; traced, the same argument is a logical namespace root.
mkdir "$tmp/dirfd"
printf 'dirfd\n' > "$tmp/expected"
if ./umbra-test-child --dirfd-rename "$tmp/dirfd" > "$tmp/stdout" 2> "$tmp/stderr" &&
   cmp -s "$tmp/expected" "$tmp/dirfd/db/c" && [ ! -e "$tmp/dirfd/da/a" ]; then
    printf 'PASS %s\n' --dirfd-rename
else
    printf 'FAIL %s\n' --dirfd-rename
    cat "$tmp/stdout" "$tmp/stderr" >&2
    result=1
fi
# --symlink-cycle also takes a directory. Untraced it exercises real symlinks;
# traced, the same checks run against the overlay's logical links.
mkdir "$tmp/symlink"
if ./umbra-test-child --symlink-cycle "$tmp/symlink" > "$tmp/stdout" 2> "$tmp/stderr"; then
    printf 'PASS %s\n' --symlink-cycle
else
    printf 'FAIL %s\n' --symlink-cycle
    cat "$tmp/stdout" "$tmp/stderr" >&2
    result=1
fi
exit "$result"
