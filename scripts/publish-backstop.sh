#!/usr/bin/env sh
# publish-backstop.sh — the fail-not-skip, workspace-wide publish backstop.
#
# WHY THIS EXISTS (ws41-03). crates.io protects the REGISTRY ENTRY, never the
# version CLAIM compiled into a binary through a path dependency. The CD publish
# step used to skip a crate whose version already exists (`|| echo skipping`).
# That skip hides a specific, dangerous case: a crate whose CONTENT CHANGED but
# whose VERSION DID NOT. The registry keeps the old artifact under the pinned
# version, and a from-source build of the published package then compiles the OLD
# code against a NEW caller — usually a loud compile error, but "usually loud" is
# no guarantee for a code-generating crate (catenary-proc's platform FFI, macro
# territory). So this backstop must FAIL, never skip, on content-drift-without-a-
# bump, and it must cover EVERY published crate — catenary-proc included, not just
# the bridge crate.
#
# MECHANISM (honest about network). For each published workspace crate:
#   1. Read its local `version`.
#   2. Read-only query of the crates.io sparse index for that crate+version.
#        - version NOT in the index  -> a NEW version. The release publishes it
#          fresh; no already-published artifact exists to drift from -> PASS.
#        - version IS in the index    -> compare content:
#             a. `cargo package -p <crate> --no-verify` builds the local .crate.
#             b. Download the PUBLISHED .crate from the read-only static CDN
#                (https://static.crates.io/crates/<name>/<name>-<ver>.crate),
#                and verify it against the sparse index `cksum` so we know we
#                fetched the real artifact.
#             c. crate_content_diff.py compares the two by PAYLOAD, not by
#                tarball bytes: it hashes every archived source file plus the
#                verbatim `Cargo.toml.orig`, and EXCLUDES the cargo-generated
#                envelope (`Cargo.toml`, which cargo re-normalizes, and
#                `.cargo_vcs_info.json`, which embeds the packaging commit sha).
#               payload EQUAL     -> published content == local content -> PASS.
#               payload DIFFERENT -> CONTENT DRIFT WITHOUT A BUMP -> FAIL LOUD.
#
# WHY PAYLOAD, NOT TARBALL CHECKSUM. The index `cksum` is the sha256 of the
# published .crate tarball, but `cargo package` does NOT reproduce that tarball
# byte-for-byte across cargo releases (regenerated Cargo.toml, injected VCS info,
# gzip framing), so a raw-checksum compare false-positives on IDENTICAL content.
# The payload digest is stable across those envelope differences, so it fires iff
# the shipped source actually changed.
#
# NETWORK. Two read-only GETs per already-published crate: the sparse-index line
# (the same endpoint `make publish-check` uses) and the static-CDN .crate. No
# credential, no write, no publish. If either is unreachable the crate cannot be
# proven safe, so the backstop FAILS rather than skipping (offline is not a pass).
#
# WHAT CD DOES WITH THIS. In cd.yml the publish job runs this backstop BEFORE the
# `cargo publish` calls. A drift makes the release RED at the backstop instead of
# green-with-a-silent-skip. See the cd.yml `Publish crates` step.
#
# USAGE
#   scripts/publish-backstop.sh [crate ...]
# With no arguments, checks the default published set (below). Pass explicit
# crate names to scope it (used by tests to drive one crate).
#
# EXIT STATUS
#   0  every checked crate is either a new version or byte-identical to its
#      already-published artifact.
#   1  at least one crate drifted from its already-published version without a
#      bump — the failure this backstop exists for.
#   2  environment error (index unreachable, cargo/sha256 tool missing, unreadable
#      manifest).

set -eu

# The published set. catenary-cli is the root binary package; catenary-mcp and
# catenary-proc are its workspace members. All three are published (catenary-cli
# has PATH dependencies on the other two, so cargo will not publish it unless they
# are on the registry). None carries `publish = false`.
DEFAULT_CRATES="catenary-proc catenary-mcp catenary-cli"

if [ "$#" -ge 1 ]; then
	CRATES="$*"
else
	CRATES="$DEFAULT_CRATES"
fi

# Resolve the sha256 tool once (sha256sum on Linux, shasum -a 256 on macOS — the
# same split cd.yml's sidecar step already handles).
sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		printf 'publish-backstop: no sha256sum or shasum available\n' >&2
		exit 2
	fi
}

# The crates.io sparse-index path shards a name into 1/2/3-char prefixes:
#   len 1      -> 1/<name>
#   len 2      -> 2/<name>
#   len 3      -> 3/<first char>/<name>
#   len >= 4   -> <chars 1-2>/<chars 3-4>/<name>
index_url() {
	name="$1"
	n="${#name}"
	case "$n" in
	1) printf 'https://index.crates.io/1/%s\n' "$name" ;;
	2) printf 'https://index.crates.io/2/%s\n' "$name" ;;
	3) printf 'https://index.crates.io/3/%s/%s\n' "$(printf '%s' "$name" | cut -c1)" "$name" ;;
	*) printf 'https://index.crates.io/%s/%s/%s\n' \
		"$(printf '%s' "$name" | cut -c1-2)" \
		"$(printf '%s' "$name" | cut -c3-4)" \
		"$name" ;;
	esac
}

version_of() {
	# $1 = crate name. Locate its Cargo.toml via cargo metadata is overkill for a
	# fixed workspace; read the manifest directly. catenary-cli is the root, the
	# others live under crates/<name>.
	name="$1"
	case "$name" in
	catenary-cli) manifest="Cargo.toml" ;;
	*) manifest="crates/$name/Cargo.toml" ;;
	esac
	[ -f "$manifest" ] || {
		printf 'publish-backstop: manifest not found for %s (%s)\n' "$name" "$manifest" >&2
		exit 2
	}
	# First `version = "..."` line is the [package] version (the table opens the
	# file for every workspace member here).
	sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$manifest" | head -1
}

# Fetch the index cksum for an exact crate+version. Emits the 64-hex checksum on
# stdout when that version is published, nothing when it is not; exits 2 when the
# index itself is unreachable (offline is not a pass).
index_cksum() {
	name="$1"
	want="$2"
	url="$(index_url "$name")"
	# -f: fail (nonzero) on HTTP 404 etc.; but a 404 means "crate name not on the
	# registry yet", which for a first publish is a legitimate new-version case, so
	# distinguish transport failure from a clean 404.
	http_body="$(curl -sSL -w '\n%{http_code}' "$url" 2>/dev/null)" || {
		printf 'publish-backstop: index unreachable for %s (%s)\n' "$name" "$url" >&2
		exit 2
	}
	code="$(printf '%s' "$http_body" | tail -1)"
	body="$(printf '%s' "$http_body" | sed '$d')"
	case "$code" in
	200) : ;;
	404)
		# Crate name unknown to the registry -> no published version of anything ->
		# treat as new. Emit nothing.
		return 0
		;;
	*)
		printf 'publish-backstop: index returned HTTP %s for %s (%s)\n' "$code" "$name" "$url" >&2
		exit 2
		;;
	esac
	# The body is newline-delimited JSON, one object per published version. Pull
	# the object for the wanted version and read its cksum. jq is allowed in
	# pipelines; select the exact vers.
	printf '%s\n' "$body" \
		| jq -r --arg v "$want" 'select(.vers == $v) | .cksum' 2>/dev/null \
		| head -1
}

rc=0
for crate in $CRATES; do
	ver="$(version_of "$crate")"
	[ -n "$ver" ] || {
		printf 'publish-backstop: could not read version for %s\n' "$crate" >&2
		exit 2
	}
	published_cksum="$(index_cksum "$crate" "$ver")"

	if [ -z "$published_cksum" ]; then
		printf 'publish-backstop: %s %s is NOT yet published — new version, OK\n' \
			"$crate" "$ver"
		continue
	fi

	# Already published at this version. Package locally and compare PAYLOAD
	# against the published artifact (tarball bytes are not reproducible; see the
	# header note — the payload digest is).
	printf 'publish-backstop: %s %s IS published — comparing packaged content...\n' \
		"$crate" "$ver"

	# a. Local package. --no-verify: we want the tarball for a content compare,
	#    not a compile against published deps (publish-check already does that).
	cargo package -p "$crate" --no-verify --allow-dirty --quiet
	local_crate="target/package/$crate-$ver.crate"
	[ -f "$local_crate" ] || {
		printf 'publish-backstop: expected packaged crate not found: %s\n' "$local_crate" >&2
		exit 2
	}

	# b. Fetch the published artifact from the read-only static CDN and verify it
	#    against the index cksum, so the comparison is against the real published
	#    bytes and not a corrupted/substituted download.
	pub_crate="target/package/$crate-$ver.published.crate"
	cdn_url="https://static.crates.io/crates/$crate/$crate-$ver.crate"
	curl -sSL -o "$pub_crate" "$cdn_url" || {
		printf 'publish-backstop: could not download published artifact for %s %s (%s)\n' \
			"$crate" "$ver" "$cdn_url" >&2
		exit 2
	}
	pub_cksum="$(sha256_of "$pub_crate")"
	if [ "$pub_cksum" != "$published_cksum" ]; then
		printf 'publish-backstop: downloaded artifact for %s %s failed its index checksum\n' \
			"$crate" "$ver" >&2
		printf '  index cksum:      %s\n' "$published_cksum" >&2
		printf '  downloaded cksum: %s\n' "$pub_cksum" >&2
		exit 2
	fi

	# c. Compare payloads (source files + verbatim Cargo.toml.orig; cargo-generated
	#    envelope excluded). Exit 0 = identical, 1 = drift, 2 = comparator error.
	if python3 scripts/crate_content_diff.py "$local_crate" "$pub_crate"; then
		printf 'publish-backstop: %s %s content matches the published artifact — OK\n' \
			"$crate" "$ver"
	else
		diff_rc=$?
		if [ "$diff_rc" -eq 1 ]; then
			printf 'publish-backstop: FAIL\n' >&2
			printf '  %s %s CONTENT DIFFERS from its already-published artifact\n' \
				"$crate" "$ver" >&2
			printf '  (differing files listed above).\n' >&2
			printf '  crates.io will not overwrite a published version, so this content\n' >&2
			printf '  would ship only as a stale registry artifact under a version claim\n' >&2
			printf '  that a from-source build then compiles against new code. Bump\n' >&2
			printf '  %s and the dependent version specs.\n' "$crate" >&2
			rc=1
		else
			printf 'publish-backstop: content comparator errored for %s %s (rc=%s)\n' \
				"$crate" "$ver" "$diff_rc" >&2
			exit 2
		fi
	fi
done

if [ "$rc" -ne 0 ]; then
	printf 'publish-backstop: at least one crate drifted from its published version without a bump.\n' >&2
fi
exit "$rc"
