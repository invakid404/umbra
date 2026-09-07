#!/bin/sh
set -eu
cd "$(dirname "$0")"
set -x
cc -O0 -g -Wall -Wextra -o umbra-test-child umbra-test-child.c
