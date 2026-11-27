"""Timeline from observed root exit through actual caller collection."""
import json
from pathlib import Path


def table(directory):
    directory = Path(directory)
    receipt = json.loads((directory / "parent-receipt.json").read_text())
    rows = [json.loads(line) for path in (directory / "events").glob("*.jsonl")
            for line in path.read_text().splitlines()]
    def unique(name):
        selected = [r for r in rows if r["event"] == name]
        if len(selected) != 1: raise ValueError("missing or duplicate " + name)
        return selected[0]
    root = unique("first_root_exit_observed")
    start = root["boot_nanos"]
    end = receipt["invocation_return"]
    stages = [root]
    for name in ("cleanup-start-requested", "provider-collection-begin", "provider-collection-received",
                 "writer-finish-begin", "writer-finish-durable", "namespace-destroyed"):
        stages.append(unique(name))
    children = [r for r in rows if r["event"] == "child-collected"]
    if sorted(r["details"]["child"]["role"] for r in children) != ["CleanupOwner", "Guardian", "Worker"]:
        raise ValueError("missing actual child collection")
    stages.extend(children)
    for r in stages:
        if r["run_nonce"] != receipt["nonce"] or not start <= r["boot_nanos"] <= end:
            raise ValueError("cross-run or out-of-interval stage")
    result = []
    for r in sorted(stages, key=lambda r: r["boot_nanos"]):
        label = r["event"]
        if label == "child-collected": label += "-" + r["details"]["child"]["role"]
        result.append(dict(stage=label, pid=r["pid"], offset_ms=(r["boot_nanos"]-start)/1e6))
    result.append(dict(stage="caller-collected",pid=receipt["pid"],offset_ms=(end-start)/1e6))
    return result

if __name__ == "__main__":
    import sys
    print(json.dumps(table(sys.argv[1]), indent=2))
