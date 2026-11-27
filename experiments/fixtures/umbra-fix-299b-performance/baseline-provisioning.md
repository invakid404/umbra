# Performance baseline provisioning

The managed performance workflow runs only by manual dispatch. D6 acceptance for PR #313 uses the local paired series under owner option A; a dispatched SID-1 performance run is not a merge requirement. The first manual dispatch is tracked in #315.

Provision the original executable bytes in a durable runner directory. Do not rebuild the baseline.

The paths below use `/Users/runner/umbra-baselines/pr313` as an example durable directory. Choose an existing directory on the runner, copy the preserved artifacts there, and update the paths together before setting repository variables.

| Variable | Example durable value |
|---|---|
| UMBRA_D6_BASE_MANIFEST | /Users/runner/umbra-baselines/pr313/manifest.json |
| UMBRA_D6_BASE_REGISTRY_MANIFEST | /Users/runner/umbra-baselines/pr313/registry-manifest.json |
| UMBRA_D6_BASE_REGISTRY | /Users/runner/umbra-baselines/pr313/registry.json |

Every artifact in the table below lives directly under the example directory `/Users/runner/umbra-baselines/pr313`; for example, the CLI path is `/Users/runner/umbra-baselines/pr313/cli`. These are example destinations, not claims that the runner has already been provisioned.

The runner must have readable manifests/registry and executable artifacts at every absolute path below. Hashes were computed from retained bytes. A relocated manifest/registry has a new JSON hash; record it while keeping every executable hash identical. The routed manifest maps storage to registry_storage; the native manifest maps storage to storage. Registry executable byte arrays must equal canonical manifest paths for platform/storage/journal. Preserve descriptor options and capabilities, including the raw server address and 12-second timeout.

| Preserved artifact | Original SHA-256 |
|---|---|
| `manifest.json` | `921886d10d21efd6fb1b1c51cfdd153dd23859501ea290cea6e6b9b7e2d11a5f` |
| `registry-manifest.json` | `398dcf4fbcee7712fa3667322b70ebc9490be6176ac0140cd9a6397c49104871` |
| `registry.json` | `fe95ca5671b7591a043af4bd86354f137c08ce07e4feff8a372f9c0c92e4f079` |
| `admission_driver` | `9a6144e5fded359a854f7aa0e512b7b6f80a7540d466be149464a8880ffcad19` |
| `cli` | `18b9d28a116c3379589f13cdab579ef13927fff2628710e6af0f3e3502730c5c` |
| `namespace` | `18b9d28a116c3379589f13cdab579ef13927fff2628710e6af0f3e3502730c5c` |
| `journal` | `8f209ae1747d17de28a88e7e30a6d1933596b48cd63115c64b5b5ebc53792cc9` |
| `platform` | `fd354cae1a3c7f7b37bb5ec6eba2dc3e287aa5c691e4e3aefcef308dd58a729e` |
| `registry_storage` | `323848c96ae9a7dbbdd28c8bcb717b466708df5927091f47e06a79e61a79e9a4` |
| `storage` | `e058462c7ac9b091665bf9ca0b1ba4232a4470f76eeadbd6117cfbd1b9250ed6` |
| `tree5` | `06518cc5273e152e4b20392c77f8d3f0a27c524222cd9cbad17f21b67f2848c1` |

The manifests pin source c85722bf4bb6f6818f42172e65b4bfce2cae2b17, tree 8d90414784dd8539855f824b3668c4f0748ffa37, debug aarch64-apple-darwin, rustc 1.100.0 (0ef1bc386059596dada8fb65b50425aab2ea637a), and observation patch dec3fad489421167d41350af20afd3174ea6f97dfbdc24613e18a0b168d7c52e. The runner compiler must match the full rustc_version string. Its /usr/bin/true must hash to 0d7233119152bf4785f3ef148abcab615f4a4ae3ee8b2f1e72eed51c35212db3. ci.py preflight validates these before sampling. The workflow provisions pinned libnfs and the raw server separately.

After provisioning, verify the manifests and registry with `ci.py` preflight, then dispatch `managed-performance.yml` on the reviewed head as part of #315. SID-1 readings run only in CI. Retain failed preflight records: a failed preflight measures no workload.
