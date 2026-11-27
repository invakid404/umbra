"""Retained startup timeline; concurrent role spans must not be summed."""
import json
from pathlib import Path


def table(directory):
    directory = Path(directory)
    receipt = json.loads((directory / "parent-receipt.json").read_text())
    start = receipt["invocation_start"]
    rows = [json.loads(line) for path in sorted((directory / "events").glob("*.jsonl"))
            for line in path.read_text().splitlines()]
    go = [r for r in rows if r["event"] == "helpers_ready_go"]
    if len(go) != 1:
        raise ValueError("exactly one worker GO required")
    end = go[0]["boot_nanos"]
    events = [r for r in rows if r["role"] == "startup" and r["boot_nanos"] <= end]
    if not events:
        raise ValueError("startup instrumentation missing")
    processes = {}
    for r in sorted(events, key=lambda r: (r["pid"], r["sequence"])):
        if r["run_nonce"] != receipt["nonce"]:
            raise ValueError("cross-run startup record")
        key = str(r["pid"])
        process = processes.setdefault(key, {"birth": r["birth"], "events": []})
        prior = process["events"]
        if process["birth"] != r["birth"] or r["sequence"] != len(prior):
            raise ValueError("birth or sequence mismatch")
        if r["boot_nanos"] < (prior[-1]["boot_nanos"] if prior else start):
            raise ValueError("startup clock reordered")
        prior.append({"event": r["event"], "boot_nanos": r["boot_nanos"],
                      "offset_ms": (r["boot_nanos"] - start) / 1e6,
                      "since_previous_ms": (r["boot_nanos"] - (prior[-1]["boot_nanos"] if prior else start)) / 1e6,
                      "role": r["details"]["managed_role"]})
    return {"helper_start_ms": (end-start)/1e6, "processes": processes,
            "note": "First entry includes spawn, program load and runtime start; kernel birth is retained in wall-clock units. Concurrent spans are not additive."}


if __name__ == "__main__":
    import sys
    print(json.dumps(table(sys.argv[1]), indent=2))
