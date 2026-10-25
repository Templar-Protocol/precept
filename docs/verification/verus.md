# Verus operational guide

[`VERIFICATION.md`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md) owns proof results, mathematical domains
and the production-refinement boundary. This guide covers the pinned Verus tools
and their invocation through the [shared local gate](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md#reproducing-this).

## Toolchain

Use Linux and Python 3.11+. Provision the Verus release, bundled solver/libraries
and isolated Rust compiler pinned in
[`verification/verus/toolchain.json`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/verification/verus/toolchain.json).
That file contains immutable downloads, checksums and compiler/component pins.
Provision explicitly under containment; existing verified installations are reused:

```sh
python3 scripts/verify_local.py --seconds 900 -- python3 scripts/provision_verus.py
```

Do not substitute a newer release or the workspace compiler.

The default home is `$XDG_CACHE_HOME/blend-verification-tools/verus/<pinned-version>`
(`~/.cache` when `XDG_CACHE_HOME` is unset). Use disk-backed storage, not a
memory-backed `/tmp`, for the compiler. `VERUS_HOME` may select the same layout
elsewhere; version-specific homes preserve existing verified installations:

```text
<VERUS_HOME>/verus-x86-linux/   # verifier, solver and bundled libraries
<VERUS_HOME>/rustup/           # pinned compiler
<VERUS_HOME>/cargo/            # isolated Cargo home
```

The home and its parent must be owned by the operator and not group-/world-writable.
The runner validates custody, file hashes and compiler identity before execution.
Provisioning uses a private Rustup umask; unsafe existing homes are rejected, not
repaired. Installation is explicit; verification does not install missing tools.

## Execution and verdicts

Run `make verify-local` from the repository root after the shared prerequisites
are satisfied. [`scripts/verify_verus.py`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/scripts/verify_verus.py) selects
[`supply_delta.rs`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/verification/verus/supply_delta.rs), enables `--no-cheating`,
uses one verifier thread and checks the structured JSON result against the exact
required function set and pinned commit.

The gate also generates an isolated floor-to-ceil mint mutation. It must produce
assertion failures in the exact expected functions; a timeout, parser error or
compiler failure is not an accepted negative control. The original proof source
is not modified.

Read `verus-positive.log` and `verus-negative.log` under
`target/verification-local/`. Do not infer success from process exit status alone
or transfer a mathematical theorem to production Rust without the refinement
identified in the main report.
