#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
"""Sign (or verify, or emit the trust root for) the registry artifact (tui-rework 08).

STAGED, NOT LIVE. Belongs in the external registry repo (see `registry/README.md`).

Signing is delegated to `openssl` (RFC 8032 Ed25519, the same scheme Catenary's
in-binary `ed25519-dalek` verifier expects) — never hand-rolled, and no
third-party Python crypto dependency. The signature is the raw 64-byte detached
Ed25519 signature over the exact payload bytes; the client fetches `<url>` and
`<url>.sig` and verifies before parsing.

The SIGNING KEY IS NEVER IN CI PLAINTEXT. The registry repo's CI loads the private
key from an encrypted GitHub Actions secret into a job-scoped tempfile, signs, and
deletes it (see `registry/workflows/gate-pipeline.yml`). This wrapper reads the key
path from `--key` or the `$CATENARY_REGISTRY_KEY_FILE` environment variable so the
same code runs locally (against a key on an offline machine) and in CI (against the
secret materialised to a tempfile).

Subcommands:
    sign     --payload registry.toml [--key FILE] [--out registry.toml.sig]
    verify   --payload registry.toml --sig registry.toml.sig --pub PUBKEY.pem
    pubkey   --key FILE            # print the 32-byte raw public key as hex
                                    # (paste into PRODUCTION_TRUST_ROOT)
    keygen   --out registry-ed25519.pem   # mint a keypair (do this OFFLINE)

`--dry-run` prints the openssl invocations without running them, so the flow is
inspectable without a key present.
"""

from __future__ import annotations

import argparse
import os
import subprocess  # noqa: S404 (openssl, argv-only, never a shell string)
import sys
from pathlib import Path


def _key_path(arg: str | None) -> Path:
    """Resolve the signing-key path from --key or $CATENARY_REGISTRY_KEY_FILE."""
    path = arg or os.environ.get("CATENARY_REGISTRY_KEY_FILE")
    if not path:
        raise SystemExit(
            "no signing key: pass --key or set CATENARY_REGISTRY_KEY_FILE "
            "(in CI, materialise the secret to a job-scoped tempfile)"
        )
    return Path(path)


def _run(argv: list[str], dry_run: bool, *, capture: bool = False) -> bytes:
    """Run an argv (never a shell string). Prints it under --dry-run instead."""
    printable = " ".join(argv)
    if dry_run:
        print(f"[dry-run] {printable}", file=sys.stderr)
        return b""
    result = subprocess.run(argv, check=True, capture_output=capture)  # noqa: S603
    return result.stdout if capture else b""


def cmd_keygen(args) -> int:
    """Mint an Ed25519 keypair. DO THIS OFFLINE — the private half never leaves a
    trusted machine except as an encrypted CI secret."""
    _run(
        ["openssl", "genpkey", "-algorithm", "ed25519", "-out", str(args.out)],
        args.dry_run,
    )
    print(f"minted {args.out} — keep the private key OFFLINE; upload only as a "
          f"CI secret. Run `pubkey` to get the trust root to embed.", file=sys.stderr)
    return 0


def cmd_sign(args) -> int:
    """Produce the detached raw Ed25519 signature over the payload bytes."""
    key = _key_path(args.key)
    out = args.out or Path(str(args.payload) + ".sig")
    _run(
        [
            "openssl", "pkeyutl", "-sign",
            "-inkey", str(key),
            "-rawin", "-in", str(args.payload),
            "-out", str(out),
        ],
        args.dry_run,
    )
    print(f"signed {args.payload} -> {out} (raw 64-byte Ed25519)", file=sys.stderr)
    return 0


def cmd_verify(args) -> int:
    """Verify a detached signature (a local sanity check; the client re-verifies
    against the in-binary trust root)."""
    _run(
        [
            "openssl", "pkeyutl", "-verify",
            "-pubin", "-inkey", str(args.pub),
            "-rawin", "-in", str(args.payload),
            "-sigfile", str(args.sig),
        ],
        args.dry_run,
    )
    print("signature OK", file=sys.stderr)
    return 0


def cmd_pubkey(args) -> int:
    """Print the 32-byte raw Ed25519 public key as hex bytes for
    PRODUCTION_TRUST_ROOT.

    An Ed25519 SubjectPublicKeyInfo DER is a fixed 44 bytes whose final 32 bytes
    are the raw public key, so slicing the DER tail is exact and dependency-free.
    """
    key = _key_path(args.key)
    der = _run(
        ["openssl", "pkey", "-in", str(key), "-pubout", "-outform", "DER"],
        args.dry_run,
        capture=True,
    )
    if args.dry_run:
        return 0
    raw = der[-32:]
    if len(raw) != 32:
        raise SystemExit(f"unexpected public-key DER length {len(der)} (need >=32)")
    as_rust = ", ".join(f"0x{b:02x}" for b in raw)
    print(f"raw public key ({len(raw)} bytes): {raw.hex()}")
    print("\nPRODUCTION_TRUST_ROOT (paste into src/registry.rs):")
    print(f"pub const PRODUCTION_TRUST_ROOT: [u8; 32] = [{as_rust}];")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Sign/verify the registry artifact.")
    parser.add_argument("--dry-run", action="store_true",
                        help="print openssl invocations without running them")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("keygen", help="mint an Ed25519 keypair (OFFLINE)")
    p.add_argument("--out", type=Path, default=Path("registry-ed25519.pem"))
    p.set_defaults(func=cmd_keygen)

    p = sub.add_parser("sign", help="produce the detached signature")
    p.add_argument("--payload", type=Path, required=True)
    p.add_argument("--key", default=None)
    p.add_argument("--out", type=Path, default=None)
    p.set_defaults(func=cmd_sign)

    p = sub.add_parser("verify", help="verify a detached signature")
    p.add_argument("--payload", type=Path, required=True)
    p.add_argument("--sig", type=Path, required=True)
    p.add_argument("--pub", type=Path, required=True)
    p.set_defaults(func=cmd_verify)

    p = sub.add_parser("pubkey", help="print the trust root to embed")
    p.add_argument("--key", default=None)
    p.set_defaults(func=cmd_pubkey)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
