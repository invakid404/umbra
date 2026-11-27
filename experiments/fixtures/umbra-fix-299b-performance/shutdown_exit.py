"""Check process exit separately from a retained managed cleanup failure."""


def provider_exit_succeeded(final, transport, rows):
    waits = [
        row["receipt"]
        for row in rows
        if row["event"] == "provider-parent-wait"
        and row["writer_instance"] == final["writer_instance"]
        and row["writer_pid"] == final["writer_pid"]
    ]
    if not waits:
        # Older direct clients have no independent cleanup-owner receipt.
        return not final.get("managed", False) and final["exit_ok"]
    receipt = waits[0]
    if any(other != receipt for other in waits):
        return False
    provider = receipt["provider"]
    return (
        provider["role"] == "Provider"
        and provider["pid"] == transport["writer_pid"]
        and receipt["owner"]["role"] == "CleanupOwner"
        and receipt["status"] == 0
    )


if __name__ == "__main__":
    import copy
    import unittest

    class ExitObservation(unittest.TestCase):
        def test_retained_failure_does_not_change_actual_wait_status(self):
            # Classification from checkpoint9-shutdown-receipt: injected acquired
            # backend drop, unconfirmed Final, clean actual provider parent wait.
            final = {
                "managed": True, "exit_ok": False,
                "writer_instance": "worker-instance", "writer_pid": 84762,
            }
            transport = {"writer_pid": 84764}
            wait = {
                "event": "provider-parent-wait",
                "writer_instance": "worker-instance", "writer_pid": 84762,
                "receipt": {
                    "failed": True, "status": 0,
                    "provider": {"role": "Provider", "pid": 84764},
                    "owner": {"role": "CleanupOwner", "pid": 84763},
                },
            }
            self.assertTrue(provider_exit_succeeded(final, transport, [wait]))
            self.assertFalse(provider_exit_succeeded(final, transport, []))
            for field, value in (("status", 256), ("status", 9)):
                changed = copy.deepcopy(wait)
                changed["receipt"][field] = value
                self.assertFalse(provider_exit_succeeded(final, transport, [changed]))
            changed = copy.deepcopy(wait)
            changed["writer_instance"] = "another-worker"
            self.assertFalse(provider_exit_succeeded(final, transport, [changed]))
            self.assertFalse(provider_exit_succeeded(final, {"writer_pid": 1}, [wait]))
            changed = copy.deepcopy(wait)
            changed["receipt"]["status"] = 256
            self.assertFalse(provider_exit_succeeded(final, transport, [wait, changed]))

    unittest.main()
