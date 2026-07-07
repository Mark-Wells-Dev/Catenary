# The external signed registry (tui-rework 08)

This directory is **staged, ready-to-instantiate content** for the Catenary
**registry repo — which does not exist yet.** Nothing here runs in this
repository's CI, and creating/pushing the registry repo is a deliberate maintainer
act, not part of landing this ticket. Until the maintainer stands the registry up
and flips `[registry] url` on, Catenary is **seed-only**: it serves the in-binary
recipes + blessed-manifest and never touches the network. Nothing changes for
users.

The client half already ships in the binary (`src/registry.rs`): the loader chain,
the in-binary trust root, and the `[registry]` config table. This directory is the
server half — the gated auto-pinning pipeline, the daily advisory re-scan, the
signing step, and the workflow templates.

## The trust model, stated honestly

A signature proves **immutability** — the tested bytes are the installed bytes; no
substitution, no MITM. It does **not** prove **benignity**. Benignity is
probabilistic and comes from the mechanical gates below, which outperform a human
eyeballing a hash diff (the superseded design). Residual, stated plainly: a
stealthy backdoor in a cooled-off, advisory-clean, attested version defeats this
pipeline, any human review, and every distro packager alike. This is the
industry-best mechanical bar, not immunity.

CI owns the whole pipeline. The maintainer is an **exception handler** — they act
only when a gate goes red — not a reviewer of every bump.

## What's here

| File | Role |
| --- | --- |
| `gate_pipeline.py` | Resolve latest per ecosystem → cooling-off → OSV → provenance → anomaly gates → emit updated pins + a machine-readable gate report. All green ⇒ mergeable; any red ⇒ an exception issue. Prepares only; never merges. |
| `rescan.py` | Daily OSV re-scan of *currently pinned* versions; on an advisory hit, roll the pin back to the last known-clean predecessor and emit the rollback diff. |
| `sign_registry.py` | `openssl`-based Ed25519 sign / verify / trust-root-extraction / keygen. The key is never in CI plaintext (loaded from a job-scoped tempfile). |
| `workflows/gate-pipeline.yml` | Weekly + dispatch: gate → conformance dispatch → sign + publish on green, else open an exception issue. |
| `workflows/rescan.yml` | Daily cron: re-scan → roll back → re-sign + republish + rollback issue. |
| `workflows/conformance-dispatch.yml` | Fire this repo's 07 conformance matrix against a candidate pin set. |

All three scripts are **stdlib-only Python** (matching `tools/` house style) and
exercisable offline: `--self-test` runs the pure gate/rollback logic with no
network, `--fixture DIR` reads canned metadata instead of the network, and
`--dry-run` reports without writing.

```
python3 registry/gate_pipeline.py --self-test
python3 registry/rescan.py --self-test
```

(In this repo, `make registry-selftest` runs both.)

## The gates (real vs recorded-honestly-unavailable)

| Gate | npm | PyPI | crates.io | Go |
| --- | --- | --- | --- | --- |
| **cooling-off** (≥ N days, default 10) | real (`time`) | real (`upload_time`) | real (`created_at`) | real (proxy `Time`) |
| **OSV advisory** (api.osv.dev) | real | real | real | real |
| **provenance / attestation** | real (`dist.attestations`) | n/a (PEP 740 not queried) | n/a (no Sigstore) | n/a (checksum-DB transparency, not provenance) |
| **anomaly: major-version jump** | real | real | real | real |
| **anomaly: artifact-size delta** | real (`unpackedSize`) | n/a | n/a | n/a |
| **anomaly: maintainer change** | real (`maintainers`) | n/a | real (`owners`) | n/a |
| **conformance** (07 matrix) | dispatched back to the Catenary 07 matrix (`conformance-dispatch.yml`) — all ecosystems | | | |

`n/a` is recorded honestly and never blocks a merge; a hard gate (`fail`) blocks.
`provenance` where the ecosystem *offers* it but the version lacks it is a `flag`
(non-blocking) unless `--require-provenance` escalates it to a hard gate.

## Going live — the maintainer's checklist

1. **Create the registry repo** (e.g. `TwoWells/catenary-registry`). Copy this
   `registry/` directory's scripts and `workflows/*.yml` into it (workflows go
   under `.github/workflows/`). Resolve every `<...>` / `TODO` marker.

2. **Seed the data.** Copy this repo's `defaults/recipes.toml` and
   `defaults/blessed-manifest.toml` into the registry repo. Start
   `clean_history.toml` with the currently blessed versions (the rescan rollback
   source):

   ```toml
   [clean]
   bash-language-server = ["5.6.0"]
   # …one entry per package, newest last.
   ```

3. **Mint the keypair — OFFLINE.** On a trusted, offline machine:

   ```
   python3 registry/sign_registry.py keygen --out registry-ed25519.pem
   ```

   The private half **never** leaves that machine except as an encrypted CI
   secret.

4. **Embed the trust root.** Extract the 32-byte raw public key and paste it into
   `src/registry.rs`:

   ```
   python3 registry/sign_registry.py pubkey --key registry-ed25519.pem
   # prints: pub const PRODUCTION_TRUST_ROOT: [u8; 32] = [0x.., …];
   ```

   Replace the placeholder `PRODUCTION_TRUST_ROOT` (all-zero) with that constant
   and cut a Catenary release. The registry left the binary; the key that vouches
   for it did not.

5. **Store the signing secret.** Upload the PEM private key to the registry repo
   as the encrypted Actions secret `CATENARY_REGISTRY_SIGNING_KEY`, and add a
   fine-grained `CONFORMANCE_DISPATCH_TOKEN` (scope `actions: write` on the
   Catenary repo). The workflows materialise the key to a job-scoped tempfile,
   sign, and shred it — it is never echoed and never written to the log.

6. **Publish once by hand** to confirm the endpoint: assemble
   `registry.toml = recipes.toml + blessed-manifest.toml`, sign it
   (`sign_registry.py sign --payload registry.toml`), and put `registry.toml` +
   `registry.toml.sig` at the URL the fleet will fetch.

7. **Flip `[registry] url` on.** Point Catenary at the endpoint. Either set the
   default in the shipped config, or set `DEFAULT_REGISTRY_URL` in
   `src/registry.rs` as the active default. Users can also opt in per-host:

   ```toml
   [registry]
   url = "https://<your-endpoint>/registry.toml"
   # disable = true   # force seed-only even with a url set
   ```

8. **Let CI run it.** The weekly `gate-pipeline` prepares and (on all-green +
   conformance) auto-publishes; the daily `rescan` revokes advised pins. The
   maintainer only acts on the exception issues.

## The client contract (already shipped)

`src/registry.rs` resolves the artifact **fetched-and-verified → cached → seed**,
verifying the detached signature against `PRODUCTION_TRUST_ROOT` *before parsing
for use*. A fetch failure or a bad signature is a loud health finding
(`registry-stale` / `registry-bad-signature`) and degrades one rung down the chain
— never to unpinned behaviour, never a hard failure. The daemon resolves on start
and on a slow hours-class cadence (`DEFAULT_REFRESH_INTERVAL`). The signature must
be a raw RFC 8032 Ed25519 signature over the exact payload bytes, which
`openssl pkeyutl -sign -rawin` (and `sign_registry.py sign`) produces.
