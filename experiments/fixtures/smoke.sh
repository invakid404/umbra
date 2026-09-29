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
# The multithreaded cases take a second output path of their own: they write two
# destinations concurrently, which is what makes a cross-thread mix-up visible.
# Untraced they run against the host and only prove the fixture itself -- both
# threads write, with the right bytes in the right file. The tracer-side
# measurement is `mt_write`/`mt_spawn` in
# crates/umbra-platform-macos/tests/fixtures.rs.
#
# `--mt-spawn` deliberately does not reap its spawned child: a blocking wait
# over a live child is `WaitPlan::Park`, which `single_thread()` gates, and the
# case has to stay off that path to measure the ungated spawn one. So untraced
# the child may still be writing after the parent has exited, and this waits for
# the result instead of racing it.
wait_for_content() {
    i=0
    while [ "$i" -lt 200 ]; do
        if cmp -s "$1" "$2"; then
            return 0
        fi
        sleep 0.05
        i=$((i + 1))
    done
    return 1
}
for command in --mt-write --mt-spawn; do
    case "$command" in
        --mt-write) first=one ;;
        *) first=libc ;;
    esac
    printf '%s\n' "$first" > "$tmp/expected-first"
    printf 'two\n' > "$tmp/expected-second"
    if ./umbra-test-child "$command" "$tmp/$command-a" "$tmp/$command-b" \
           > "$tmp/stdout" 2> "$tmp/stderr" &&
       wait_for_content "$tmp/expected-first" "$tmp/$command-a" &&
       wait_for_content "$tmp/expected-second" "$tmp/$command-b"; then
        printf 'PASS %s\n' "$command"
    else
        printf 'FAIL %s\n' "$command"
        cat "$tmp/stdout" "$tmp/stderr" >&2
        result=1
    fi
done
exit "$result"
