#!/usr/bin/env python3
"""Probe NFS directly over TCP/2049: v4 NULL succeeds and v3 is rejected."""
import socket
import struct
import sys


def call(host, version):
    xid = 0x554D4200 + version
    request = struct.pack("!10I", xid, 0, 2, 100003, version, 0, 0, 0, 0, 0)
    with socket.create_connection((host, 2049), timeout=2) as sock:
        sock.sendall(struct.pack("!I", 0x80000000 | len(request)) + request)

        def read(size):
            data = b""
            while len(data) < size:
                part = sock.recv(size - len(data))
                if not part:
                    raise RuntimeError("RPC connection closed")
                data += part
            return data

        response = b""
        while True:
            marker, = struct.unpack("!I", read(4))
            size = marker & 0x7FFFFFFF
            if size > 65536:
                raise RuntimeError("oversize RPC reply")
            response += read(size)
            if marker & 0x80000000:
                break
    reply_xid, kind, status, _, verifier_size = struct.unpack_from("!5I", response)
    if (reply_xid, kind, status) != (xid, 1, 0):
        raise RuntimeError("RPC reply not accepted")
    offset = 20 + ((verifier_size + 3) & ~3)
    return struct.unpack_from("!I", response, offset)[0]


if __name__ == "__main__":
    try:
        host = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
        if call(host, 4) != 0:
            raise RuntimeError("NFSv4 NULL failed")
        # PROG_UNAVAIL or PROG_MISMATCH; no NFSv3 operation is performed.
        if call(host, 3) not in (1, 2):
            raise RuntimeError("NFSv3 was not rejected")
        print("PASS NFSv4 RPC ready; NFSv3 rejected on TCP/2049")
    except (OSError, RuntimeError, struct.error) as exc:
        print(f"FAIL RPC readiness: {exc}", file=sys.stderr)
        sys.exit(1)
