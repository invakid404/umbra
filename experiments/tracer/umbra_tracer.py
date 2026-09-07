#!/usr/bin/python3
"""Disposable native-arm64 LLDB Gate 2 experiment; not a filesystem sandbox."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import plistlib
import platform
import shutil
import subprocess
import struct
import sys
import tempfile
import time

LLDB_PYTHON = "/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Resources/Python"
sys.path.insert(0, LLDB_PYTHON)
import lldb

MAX_PATH = 4096
CACHE = Path.home() / "Library/Caches/umbra/twins"
ENTITLEMENTS = {
    "com.apple.security.get-task-allow": True,
    "com.apple.security.cs.allow-jit": True,
    "com.apple.security.cs.allow-unsigned-executable-memory": True,
}


def log(message):
    print(message, flush=True)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def command(argv):
    result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} failed: {os.fsdecode(result.stderr).strip()}")
    return result.stdout


def resign(source):
    source = Path(source).absolute()
    digest = sha256(source)
    folder = CACHE / digest
    twin = folder / source.name
    meta = folder / (source.name + ".json")
    try:
        saved = json.loads(meta.read_text())
        if saved["source_sha256"] == digest and saved["twin_sha256"] == sha256(twin):
            command(["/usr/bin/codesign", "--verify", "--strict", str(twin)])
            ent = plistlib.loads(command(["/usr/bin/codesign", "-d", "--entitlements", ":-", str(twin)]))
            if all(ent.get(key) is True for key in ENTITLEMENTS):
                log(f"TWIN[hit]: {source} -> {twin}")
                return str(twin)
    except (OSError, ValueError, KeyError, RuntimeError, plistlib.InvalidFileException):
        pass
    folder.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".sign-", dir=folder) as staging:
        candidate = Path(staging) / source.name
        shutil.copy2(source, candidate)
        if sha256(candidate) != digest:
            raise RuntimeError("source changed while copying; retry")
        ent = Path(staging) / "ent.plist"
        ent.write_bytes(plistlib.dumps(ENTITLEMENTS))
        command(["/usr/bin/codesign", "-f", "-s", "-", "--entitlements", str(ent),
                 "--preserve-metadata=identifier,flags,runtime", str(candidate)])
        command(["/usr/bin/codesign", "--verify", "--strict", str(candidate)])
        metadata = {"source_sha256": digest, "twin_sha256": sha256(candidate)}
        os.replace(candidate, twin)
        staged_meta = Path(staging) / "metadata.json"
        staged_meta.write_text(json.dumps(metadata, indent=2) + "\n")
        os.replace(staged_meta, meta)
    log(f"TWIN[resigned]: {source} -> {twin}")
    return str(twin)


class Tracer:
    def __init__(self, root, timeout):
        self.root = os.path.abspath(root)
        self.timeout = timeout
        self.debugger = lldb.SBDebugger.Create()
        self.debugger.SetAsync(True)
        self.listener = self.debugger.GetListener()
        self.sessions = []
        self.opens = 0
        self.execs = 0
        self.forks = 0
        self.spawn_count = 0
        self.cli("settings set target.process.stop-on-exec true")
        self.cli("settings set target.process.follow-fork-mode child")
        self.cli("settings set target.skip-prologue false")

    def cli(self, text):
        result = lldb.SBCommandReturnObject()
        self.debugger.GetCommandInterpreter().HandleCommand(text, result)
        if not result.Succeeded():
            log(f"SETTING[unsupported]: {text}: {result.GetError().strip()}")
        return result

    def install(self, target):
        kinds = {}
        for name, kind, register in [
            ("__open", "open", "x0"),
            ("__open_nocancel", "open", "x0"),
            ("__openat", "open", "x1"),
            ("__openat_nocancel", "open", "x1"),
            ("__execve", "exec", "x0"),
            ("__posix_spawn", "spawn", "x1"),
            # On this build both libc fork() and vfork() call __fork.
            ("__fork", "fork", None),
        ]:
            contexts = target.FindFunctions(name)
            found = False
            for context in contexts:
                if context.GetModule().GetFileSpec().GetFilename() != "libsystem_kernel.dylib":
                    continue
                start = context.GetSymbol().GetStartAddress()
                for instruction in target.ReadInstructions(start, 24):
                    if instruction.GetMnemonic(target) == "svc":
                        bp = target.BreakpointCreateBySBAddress(instruction.GetAddress())
                        kinds[bp.GetID()] = (kind, register, name)
                        found = True
                        log(f"BREAKPOINT: {name} svc file-address={instruction.GetAddress().GetFileAddress():#x} path={register}")
                        break
            if not found:
                raise RuntimeError(f"cannot find {name} syscall stub in local libsystem_kernel")
        # Scan native executable code, not a fixture-specific symbol. This is
        # deliberately limited to the main Mach-O; dyld/JIT coverage is future work.
        def sections(section):
            if section.GetNumSubSections():
                for child in section:
                    yield from sections(child)
            else:
                yield section
        raw_sites = 0
        executable = target.GetModuleAtIndex(0)
        for index in range(executable.GetNumSections()):
            top = executable.GetSectionAtIndex(index)
            for section in sections(top):
                if section.GetSectionType() != lldb.eSectionTypeCode:
                    continue
                if section.GetByteSize() > 16 * 1024 * 1024:
                    raise RuntimeError("main executable code section exceeds prototype scan limit")
                error = lldb.SBError()
                data = section.GetSectionData().ReadRawData(error, 0, section.GetByteSize())
                if error.Fail():
                    raise RuntimeError(f"read executable section: {error}")
                for offset in range(0, len(data) - 3, 4):
                    if data[offset:offset + 4] == b"\x01\x10\x00\xd4":  # svc #0x80
                        address = lldb.SBAddress(section, offset)
                        bp = target.BreakpointCreateBySBAddress(address)
                        kinds[bp.GetID()] = ("raw", None, "main-executable svc")
                        raw_sites += 1
        log(f"RAW-SCAN: main executable svc sites={raw_sites}")
        return {"target": target, "process": None, "kinds": kinds, "stop": -1,
                "scratch": [], "abi": set(), "done": False, "pending_spawn": {}}

    @staticmethod
    def reg(frame, name):
        value = frame.FindRegister(name)
        if not value.IsValid():
            raise RuntimeError(f"missing register {name}; native arm64 required")
        return value.GetValueAsUnsigned()

    @staticmethod
    def setreg(frame, name, value):
        error = lldb.SBError()
        if not frame.FindRegister(name).SetValueFromCString(hex(value), error):
            raise RuntimeError(f"write {name}: {error}")

    @staticmethod
    def read(process, address, size):
        error = lldb.SBError()
        data = process.ReadMemory(address, size, error)
        if error.Fail() or len(data) != size:
            raise RuntimeError(f"read {address:#x}/{size}: {error}")
        return data

    def path(self, process, pointer):
        # Byte reads preserve non-UTF-8 paths and avoid crossing unmapped pages.
        result = bytearray()
        for index in range(MAX_PATH):
            byte = self.read(process, pointer + index, 1)
            if byte == b"\0":
                return os.fsdecode(bytes(result))
            result.extend(byte)
        raise RuntimeError(f"unterminated path exceeds {MAX_PATH} bytes")

    def allocate(self, session, data):
        process = session["process"]
        error = lldb.SBError()
        # Keep each allocation alive until exec/exit; no in-flight path reuse.
        size = max(16384, len(data))
        blocks = session["scratch"]
        if not blocks or blocks[-1][2] + len(data) > blocks[-1][1]:
            address = process.AllocateMemory(size, lldb.ePermissionsReadable | lldb.ePermissionsWritable, error)
            if error.Fail() or address == lldb.LLDB_INVALID_ADDRESS:
                raise RuntimeError(f"allocate scratch: {error}")
            blocks.append([address, size, 0])
        block = blocks[-1]
        address = block[0] + block[2]
        count = process.WriteMemory(address, data, error)
        if error.Fail() or count != len(data):
            raise RuntimeError(f"write scratch: {error}")
        block[2] += (len(data) + 15) & ~15
        return address

    def inspect_abi(self, session, frame, name):
        if name in session["abi"]:
            return
        session["abi"].add(name)
        insns = session["target"].ReadInstructions(frame.GetPCAddress(), 8)
        text = "; ".join(f"{i.GetMnemonic(session['target'])} {i.GetOperands(session['target'])}" for i in insns)
        log(f"ABI[{name}]: {text}")

    def hit(self, session, thread, kind, register, name):
        frame = thread.GetFrameAtIndex(0)
        process = session["process"]
        log(f"INTERCEPT: pid={process.GetProcessID()} tid={thread.GetThreadID()} {name}")
        self.inspect_abi(session, frame, name)
        if kind == "spawn-return":
            self.spawn_return(session, thread, name)
            return
        if kind == "raw":
            number = self.reg(frame, "x16")
            mapping = {5: ("open", "x0"), 398: ("open", "x0"),
                       463: ("open", "x1"), 464: ("open", "x1"),
                       59: ("exec", "x0"), 244: ("spawn", "x1"),
                       2: ("fork", None), 66: ("fork", None)}
            if number not in mapping:
                raise RuntimeError(f"unsupported raw syscall {number}")
            kind, register = mapping[number]
            log(f"RAW[svc]: pid={process.GetProcessID()} syscall={number}")
        if kind == "fork":
            self.forks += 1
            log(f"FORK[entry]: pid={process.GetProcessID()} {name}; follow-fork-mode=child (capture unproven)")
            return
        old = self.path(process, self.reg(frame, register))
        if kind == "open":
            self.opens += 1
            log(f"OPEN[old]: {old}")
            if not os.path.isabs(old):
                raise RuntimeError(f"relative path/dirfd semantics not implemented: {old!r}")
            new = os.path.join(self.root, os.path.normpath(old).lstrip("/"))
            Path(new).parent.mkdir(parents=True, exist_ok=True)
            pointer = self.allocate(session, os.fsencode(new) + b"\0")
            self.setreg(frame, register, pointer)
            log(f"OPEN[new]: {new}")
        else:
            new = resign(old)
            pointer = self.allocate(session, os.fsencode(new) + b"\0")
            self.setreg(frame, register, pointer)
            self.execs += 1
            log(f"{kind.upper()}[rewrite]: {old} -> {new}")
            if kind == "spawn":
                self.spawn_count += 1
                self.prepare_spawn(session, thread, new)

    def prepare_spawn(self, session, thread, twin):
        """Pinned, null-attributes/file-actions spawn probe; runtime unverified.

        The local 25F80 posix_spawn disassembly passes a 144-byte descriptor
        with (size, pointer) pairs, and sizeof(*attr) == 248. Use the host's
        public API for default fields rather than guessing their defaults.
        """
        frame = thread.GetFrameAtIndex(0)
        if platform.mac_ver()[0] != "26.5.1":
            raise RuntimeError("spawn descriptor experiment is pinned to macOS 26.5.1")
        if self.reg(frame, "x2"):
            raise RuntimeError("spawn with non-null attributes/file-actions not implemented")
        libc = ctypes.CDLL(None)
        attr = ctypes.c_void_p()
        if libc.posix_spawnattr_init(ctypes.byref(attr)):
            raise RuntimeError("host posix_spawnattr_init failed")
        try:
            if libc.posix_spawnattr_setflags(ctypes.byref(attr), ctypes.c_short(0x80)):
                raise RuntimeError("host posix_spawnattr_setflags failed")
            data = ctypes.string_at(attr, 248)
            if any(data[192:]) or data[:2] != b"\x80\0":
                raise RuntimeError("spawn attribute layout differs from inspected local ABI")
        finally:
            libc.posix_spawnattr_destroy(ctypes.byref(attr))
        attr_pointer = self.allocate(session, data)
        descriptor = struct.pack("<QQ", 248, attr_pointer) + bytes(128)
        self.setreg(frame, "x2", self.allocate(session, descriptor))
        pid_pointer = self.reg(frame, "x0")
        if not pid_pointer:
            pid_pointer = self.allocate(session, bytes(4))
            self.setreg(frame, "x0", pid_pointer)
        bp = session["target"].BreakpointCreateByAddress(frame.GetPC() + 4)
        bp.SetThreadID(thread.GetThreadID())
        # Delete explicitly at the return stop, after inspecting its ID.
        key = str(bp.GetID())
        session["pending_spawn"][key] = (pid_pointer, twin)
        session["kinds"][bp.GetID()] = ("spawn-return", None, key)
        log("SPAWN[suspended-request]: POSIX_SPAWN_START_SUSPENDED; awaiting syscall return")

    def spawn_return(self, session, thread, key):
        frame = thread.GetFrameAtIndex(0)
        pid_pointer, twin = session["pending_spawn"].pop(key)
        session["target"].BreakpointDelete(int(key))
        session["kinds"].pop(int(key))
        # At svc+4, Darwin carry (CPSR bit 29) indicates positive errno in x0.
        if self.reg(frame, "cpsr") & (1 << 29):
            raise RuntimeError(f"spawn syscall errno={self.reg(frame, 'x0')}")
        pid = struct.unpack("<i", self.read(session["process"], pid_pointer, 4))[0]
        if pid <= 0:
            raise RuntimeError(f"invalid spawn child PID: {pid}")
        log(f"SPAWN[attach]: suspended pid={pid}")
        target = self.debugger.CreateTarget(twin)
        child = self.install(target)
        self.sessions.append(child)
        error = lldb.SBError()
        child["process"] = target.AttachToProcessWithID(self.listener, pid, error)
        if error.Fail():
            raise RuntimeError(f"attach suspended child {pid}: {error}")
        log(f"SPAWN[attached]: pid={pid}; pending initial debugger stop")

    def drain(self, process):
        for getter, stream in [(process.GetSTDOUT, sys.stdout), (process.GetSTDERR, sys.stderr)]:
            while True:
                value = getter(4096)
                if not value:
                    break
                stream.write(value)
                stream.flush()

    def run(self, source, args):
        twin = resign(source)
        target = self.debugger.CreateTarget(twin)
        if not target.IsValid() or not target.GetTriple().startswith("arm64"):
            raise RuntimeError(f"expected native arm64 executable: {target.GetTriple()}")
        session = self.install(target)
        self.sessions.append(session)
        info = lldb.SBLaunchInfo([os.path.abspath(source)] + args)
        info.SetExecutableFile(lldb.SBFileSpec(twin), False)
        info.SetLaunchFlags(lldb.eLaunchFlagDebug | lldb.eLaunchFlagStopAtEntry)
        info.SetWorkingDirectory(os.getcwd())
        error = lldb.SBError()
        log(f"LAUNCH: {twin}")
        process = target.Launch(info, error)
        session["process"] = process
        if error.Fail() or not process.IsValid():
            raise RuntimeError(f"launch: {error}")
        log(f"LAUNCHED: pid={process.GetProcessID()}")
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            active = False
            for current in list(self.sessions):
                if current["done"]:
                    continue
                active = True
                p = current["process"]
                self.drain(p)
                state = p.GetState()
                if state == lldb.eStateExited:
                    current["done"] = True
                    current["exit"] = p.GetExitStatus()
                    log(f"EXIT: pid={p.GetProcessID()} status={p.GetExitStatus()} {p.GetExitDescription() or ''}")
                elif state in (lldb.eStateCrashed, lldb.eStateDetached, lldb.eStateInvalid):
                    raise RuntimeError(f"unexpected process state {lldb.SBDebugger.StateAsCString(state)}")
                elif state == lldb.eStateStopped and current["stop"] != p.GetStopID():
                    current["stop"] = p.GetStopID()
                    for thread in p:
                        reason = thread.GetStopReason()
                        if reason == lldb.eStopReasonExec:
                            current["scratch"].clear()
                            for bid in list(current["kinds"]):
                                current["target"].BreakpointDelete(bid)
                            replacement = self.install(current["target"])
                            current["kinds"] = replacement["kinds"]
                            current["pending_spawn"].clear()
                            log(f"EXEC[stop]: pid={p.GetProcessID()}")
                        elif reason == lldb.eStopReasonBreakpoint:
                            seen = set()
                            for index in range(0, thread.GetStopReasonDataCount(), 2):
                                bid = thread.GetStopReasonDataAtIndex(index)
                                if bid in current["kinds"]:
                                    item = current["kinds"][bid]
                                    if item[:2] not in seen:
                                        self.hit(current, thread, *item)
                                        seen.add(item[:2])
                        elif reason in (lldb.eStopReasonSignal, lldb.eStopReasonException):
                            if not (current["stop"] <= 1 and reason == lldb.eStopReasonSignal and
                                    thread.GetStopReasonDataAtIndex(0) in (5, 17)):
                                raise RuntimeError(f"pid={p.GetProcessID()} stopped: {thread.GetStopDescription(1024)}")
                    err = p.Continue()
                    if err.Fail():
                        raise RuntimeError(f"continue: {err}")
            if not active:
                log(f"SUMMARY: opens={self.opens} exec_rewrites={self.execs} fork_entries={self.forks} spawn_entries={self.spawn_count}")
                return max(s.get("exit", 1) for s in self.sessions)
            # Drain events to advance LLDB's public state without blocking polling.
            event = lldb.SBEvent()
            while self.listener.GetNextEvent(event):
                pass
            time.sleep(0.01)
        raise RuntimeError(f"timed out after {self.timeout}s")

    def close(self):
        for session in reversed(self.sessions):
            process = session["process"]
            if process and process.IsValid() and process.GetState() not in (lldb.eStateExited, lldb.eStateDetached):
                process.Kill()
        lldb.SBDebugger.Destroy(self.debugger)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--redirect-root", default="/tmp/umbra-nfs-stub/")
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("target")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if options.timeout <= 0:
        parser.error("--timeout must be positive")
    if not options.worker:
        from bounded_run import run
        try:
            return run([sys.executable, str(Path(__file__).resolve()), "--worker"] + sys.argv[1:], options.timeout)
        except (OSError, subprocess.SubprocessError) as error:
            log(f"ERROR: watchdog needs permission to inspect its subprocess tree: {error}")
            return 1
    tracer = Tracer(options.redirect_root, options.timeout)
    try:
        return tracer.run(options.target, options.args)
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        log(f"ERROR: {error}")
        return 1
    finally:
        tracer.close()


if __name__ == "__main__":
    sys.exit(main())
