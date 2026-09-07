"""Bounded LLDB/Seatbelt reproducer; also imported by LLDB to verify stops."""
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent
RESULTS = ROOT / "results"
TWIN = Path("/tmp/umbra-m0/twins/ls-c-test")
WRAPPER = TWIN.with_name("sandbox-exec-c-test")
ENT = Path("/Users/inva/Coding/umbra/experiments/gate-1/ent.plist")
PROFILE = ROOT / "umbra.sb"


def entry_address():
    """Derive the arm64 LC_MAIN file address, rather than guess a stripped symbol."""
    output = subprocess.check_output(
        ["/usr/bin/otool", "-arch", "arm64e", "-l", str(TWIN)], text=True)
    text = re.search(r"segname __TEXT\s+vmaddr (0x[0-9a-fA-F]+).*?fileoff (\d+)",
                     output, re.S)
    entry = re.search(r"cmd LC_MAIN\s+cmdsize \d+\s+entryoff (\d+)", output)
    if not text or not entry:
        raise RuntimeError("cannot identify __TEXT and LC_MAIN in the ls twin")
    return int(text[1], 16) + int(entry[1]) - int(text[2])


def verify_stop(debugger, address):
    """Only a real breakpoint stop in the correct image at LC_MAIN is success."""
    import lldb
    target = debugger.GetSelectedTarget()
    process = target.GetProcess()
    thread = process.GetSelectedThread()
    frame = thread.GetFrameAtIndex(0)
    pc = frame.GetPCAddress()
    module_path = Path(str(frame.GetModule().GetFileSpec())).resolve()
    valid = (process.GetState() == lldb.eStateStopped
             and thread.GetStopReason() == lldb.eStopReasonBreakpoint
             and module_path == TWIN.resolve()
             and pc.GetFileAddress() == address)
    print("UMBRA_STOP module={} file_address={:#x} reason={}".format(
        module_path, pc.GetFileAddress(), thread.GetStopReason()))
    print("UMBRA_LS_MAIN_BREAKPOINT=" + ("PASS" if valid else "FAIL"))


def descendants(root_pid, known):
    # debugserver uses setsid, so killing only LLDB's process group is insufficient.
    # Track only descendants of our own invocation, never all system debuggers.
    listing = subprocess.run(["/bin/ps", "-axo", "pid=,ppid=,comm="],
                             capture_output=True, text=True)
    rows = []
    for line in listing.stdout.splitlines():
        fields = line.strip().split(None, 2)
        if len(fields) == 3:
            rows.append((int(fields[0]), int(fields[1]), fields[2]))
    changed = True
    while changed:
        changed = False
        for pid, parent, command in rows:
            if (parent == root_pid or parent in known) and pid not in known:
                known[pid] = command
                changed = True
    return {pid: command for pid, _, command in rows}


def run_case(name, command, timeout=20):
    path = RESULTS / (name + ".log")
    known = {}
    timed_out = False
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    with path.open("w") as log:
        log.write("COMMAND: " + shlex.join(command) + "\n")
        log.flush()
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                   stdin=subprocess.DEVNULL, start_new_session=True,
                                   env=env)
        deadline = time.monotonic() + timeout
        try:
            while process.poll() is None:
                descendants(process.pid, known)
                if time.monotonic() >= deadline:
                    timed_out = True
                    break
                time.sleep(0.1)
        finally:
            current = descendants(process.pid, known)
            for pid, command_name in reversed(list(known.items())):
                if current.get(pid) == command_name:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        log.write("\nHARNESS exit={} timeout={}\n".format(process.returncode, timed_out))
        for pid, command_name in [(process.pid, command[0])] + list(known.items()):
            log.write("HARNESS process={} executable={}\n".format(pid, command_name))
    output = path.read_text()
    passed = (not timed_out and process.returncode == 0 and
              "UMBRA_LS_MAIN_BREAKPOINT=PASS" in output.splitlines())
    print("{} {} ({}{})".format("PASS" if passed else "FAIL", name,
          "timeout; " if timed_out else "", path.relative_to(ROOT)), flush=True)
    if not passed:
        for line in output.splitlines():
            if line.startswith("error:") or line.startswith("sandbox-exec:"):
                print("  " + line, flush=True)
    return passed


def lldb_command(executable, arguments, address, raw=False):
    commands = ["settings set target.disable-aslr false"]
    if raw:
        commands += ["b main", "run"]
    else:
        commands += ["settings set target.disable-stdio true",
                     "settings set target.process.stop-on-exec false",
                     "breakpoint set -s ls-c-test -a {:#x}".format(address),
                     "process launch -X false"]
    commands += ["process status",
                 "script import sys; sys.path.insert(0, {!r}); "
                 "import lldb_composition; "
                 "lldb_composition.verify_stop(lldb.debugger, {})".format(str(ROOT), address),
                 "process kill"]
    result = ["/usr/bin/lldb", "--no-lldbinit", "--batch"]
    for command in commands:
        result += ["-o", command]
    return result + ["--", str(executable)] + list(map(str, arguments))


def main():
    RESULTS.mkdir(exist_ok=True)
    # Keep all setup and original signatures in the report artifacts.
    with (RESULTS / "setup.log").open("w") as log:
        for command in (["/usr/bin/sw_vers"], ["/usr/bin/uname", "-m"],
                        ["/usr/bin/csrutil", "status"], ["/usr/bin/lldb", "--version"],
                        ["/sbin/mount"]):
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
        TWIN.parent.mkdir(parents=True, exist_ok=True)
        for source, destination in (("/bin/ls", TWIN), ("/usr/bin/sandbox-exec", WRAPPER)):
            if destination.is_symlink():
                raise RuntimeError("refusing to replace symlink: " + str(destination))
            subprocess.run(["/bin/cp", "-f", source, str(destination)], check=True)
            subprocess.run(["/usr/bin/codesign", "-f", "-s", "-", "--entitlements", str(ENT),
                            "--preserve-metadata=identifier,flags,runtime", str(destination)],
                           stdout=log, stderr=subprocess.STDOUT, check=True)
            subprocess.run(["/usr/bin/codesign", "--verify", "--strict", str(destination)],
                           stdout=log, stderr=subprocess.STDOUT, check=True)
            subprocess.run(["/usr/bin/codesign", "-dvv", "--entitlements", ":-", str(destination)],
                           stdout=log, stderr=subprocess.STDOUT, check=True)
    address = entry_address()
    print("ls LC_MAIN file address: {:#x} (stripped main symbol fallback)".format(address), flush=True)
    arguments = ["-f", PROFILE, TWIN, "-d", ROOT]
    cases = [
        ("baseline-entry", lldb_command(TWIN, ["-d", ROOT], address)),
        ("lldb-outside-literal", lldb_command("/usr/bin/sandbox-exec", arguments, address, raw=True)),
        ("sandbox-outside-literal", ["/usr/bin/sandbox-exec", "-f", str(PROFILE)] +
         lldb_command(TWIN, ["-d", ROOT], address, raw=True)),
        ("lldb-outside-resigned-launcher", lldb_command(WRAPPER, arguments, address)),
        ("sandbox-outside-entry", ["/usr/bin/sandbox-exec", "-f", str(PROFILE)] +
         lldb_command(TWIN, ["-d", ROOT], address)),
    ]
    outcomes = [(name, run_case(name, command)) for name, command in cases]
    with (RESULTS / "verify-lldb-composition.log").open("w") as log:
        for name, passed in outcomes:
            log.write(("PASS" if passed else "FAIL") + " " + name + "\n")
    # Diagnostic literal failures remain visible even if an adapted order works.
    return 0 if dict(outcomes)["lldb-outside-resigned-launcher"] and dict(outcomes)["sandbox-outside-entry"] else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print("FAIL LLDB composition setup: " + str(error), file=sys.stderr)
        sys.exit(1)
