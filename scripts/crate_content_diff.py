#!/usr/bin/env python3
"""Compare the PAYLOAD content of two .crate tarballs (ws41-03 publish backstop).

A .crate is a gzipped tar whose single top-level dir is `<name>-<version>/`. The
raw tarball sha256 is NOT a reliable content-equality signal: cargo regenerates a
normalized `Cargo.toml`, injects `.cargo_vcs_info.json` (commit sha, dirty flag)
and `Cargo.toml.orig`, and the gzip framing/order can shift across cargo
releases. So two byte-different tarballs can carry identical source.

This comparator hashes only the PAYLOAD — every archived file EXCEPT the
cargo-generated envelope — plus `Cargo.toml.orig` (the verbatim pre-normalization
manifest, which both tarballs carry and which changes iff the real manifest
changed). That digest changes iff the crate's actual shipped content changes, so
comparing it across the local package and the published artifact is the honest
drift signal the backstop needs.

Excluded (cargo-generated, legitimately non-reproducible):
  Cargo.toml               regenerated / normalized by cargo at package time
  .cargo_vcs_info.json     embeds the commit sha + dirty flag of the packaging run

Usage:
  crate_content_diff.py <local.crate> <published.crate>

Exit status:
  0  payload digests match  -> content is identical
  1  payload digests differ -> DRIFT; prints the differing file set to stderr
  2  usage / IO error
"""

from __future__ import annotations

import hashlib
import sys
import tarfile

# Envelope files cargo (re)generates at package time; excluded from the payload
# digest because they are legitimately non-reproducible across packaging runs.
_ENVELOPE = {"Cargo.toml", ".cargo_vcs_info.json"}


def _payload(crate_path: str) -> dict[str, bytes]:
    """Map payload-relative path -> sha256 of its bytes, for one .crate.

    Strips the leading `<name>-<version>/` component so two crates compare by
    payload-relative path. Skips the cargo envelope and any non-regular member.
    """
    digests: dict[str, bytes] = {}
    with tarfile.open(crate_path, "r:gz") as tar:
        for member in tar.getmembers():
            if not member.isfile():
                continue
            parts = member.name.split("/", 1)
            if len(parts) != 2:
                # A file at the archive root (no `<name>-<ver>/` prefix) is
                # unexpected; keep it under its full name so a real difference
                # still shows rather than being silently dropped.
                rel = member.name
            else:
                rel = parts[1]
            if rel in _ENVELOPE:
                continue
            extracted = tar.extractfile(member)
            if extracted is None:
                continue
            digests[rel] = hashlib.sha256(extracted.read()).digest()
    return digests


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        sys.stderr.write(
            "usage: crate_content_diff.py <local.crate> <published.crate>\n"
        )
        return 2
    local_path, published_path = argv[1], argv[2]
    try:
        local = _payload(local_path)
        published = _payload(published_path)
    except (OSError, tarfile.TarError) as exc:
        sys.stderr.write(f"crate_content_diff: {exc}\n")
        return 2

    if local == published:
        return 0

    local_files = set(local)
    published_files = set(published)
    only_local = sorted(local_files - published_files)
    only_published = sorted(published_files - local_files)
    changed = sorted(
        f for f in (local_files & published_files) if local[f] != published[f]
    )
    if only_local:
        sys.stderr.write("  files only in the local package:\n")
        for f in only_local:
            sys.stderr.write(f"    + {f}\n")
    if only_published:
        sys.stderr.write("  files only in the published artifact:\n")
        for f in only_published:
            sys.stderr.write(f"    - {f}\n")
    if changed:
        sys.stderr.write("  files whose content changed:\n")
        for f in changed:
            sys.stderr.write(f"    ~ {f}\n")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
