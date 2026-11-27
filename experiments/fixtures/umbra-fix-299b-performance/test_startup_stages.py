import copy
import json
import tempfile
import unittest
from pathlib import Path
from startup_stages import table

class Stages(unittest.TestCase):
    def test_identity_sequence_and_clock(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "events").mkdir()
            (root / "parent-receipt.json").write_text(json.dumps({"invocation_start": 10, "nonce": "run"}))
            rows = [dict(role="startup", event="executable-entry", pid=1, birth=[1, 2], sequence=0,
                         boot_nanos=20, run_nonce="run", details={"managed_role": None}),
                    dict(role="startup", event="go-received", pid=1, birth=[1, 2], sequence=1,
                         boot_nanos=30, run_nonce="run", details={"managed_role": "Worker"}),
                    dict(role="worker", event="helpers_ready_go", boot_nanos=40, run_nonce="run")]
            def write(values):
                (root / "events" / "1.jsonl").write_text("\n".join(map(json.dumps, values)))
            write(rows)
            self.assertEqual(table(root)["helper_start_ms"], 30/1e6)
            for index, key, value in [(1, "sequence", 2), (1, "birth", [1, 3]),
                                      (1, "boot_nanos", 19), (0, "run_nonce", "other"),
                                      (2, "run_nonce", "other")]:
                changed = copy.deepcopy(rows)
                changed[index][key] = value
                write(changed)
                with self.assertRaises(ValueError): table(root)
            write(rows[:-1])
            with self.assertRaises(ValueError): table(root)

if __name__ == "__main__": unittest.main()
