#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
work=$(mktemp -d /tmp/umbra-demo.XXXXXX)
host=/tmp/umbra-should-not-exist
shadow=/tmp/umbra-nfs-stub/tmp/umbra-should-not-exist
cleanup() {
    rm -rf "$work"
    if [[ ${owned:-0} == 1 ]]; then rm -f "$host" "$shadow"; fi
}
trap cleanup EXIT
if [[ -e "$host" || -L "$host" || -e "$shadow" || -L "$shadow" ]]; then
    echo 'FAIL: demo output already exists; move it aside before running'
    exit 1
fi
owned=1
cat > "$work/hello_writer.c" <<'C'
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int fd = open(argv[1], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open"); return 1; }
    if (write(fd, "hello", 5) != 5) { perror("write"); close(fd); return 1; }
    if (close(fd)) { perror("close"); return 1; }
    return 0;
}
C
cc -O0 -g -Wall -Wextra "$work/hello_writer.c" -o "$work/hello_writer"
mkdir -p /tmp/umbra-nfs-stub/
if /usr/bin/python3 ./umbra_tracer.py "$work/hello_writer" "$host" &&
   [[ ! -e "$host" && ! -L "$host" && -f "$shadow" ]] &&
   [[ $(cat "$shadow") == hello ]]; then
    echo PASS
else
    echo FAIL
    exit 1
fi
