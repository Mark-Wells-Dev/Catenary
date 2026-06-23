# Catenary Release Makefile
# Usage:
#   make release-patch   # 0.5.5 -> 0.5.6
#   make release-minor   # 0.5.5 -> 0.6.0
#   make release-major   # 0.5.5 -> 1.0.0
#   make release V=0.6.0 # explicit version

.PHONY: bench bench-test build-release check deny fuzz machete mdbook mutants mutants-stop rustdoc test test-ignored release release-patch release-minor release-major publish tag-current

# Get current version from Cargo.toml
CURRENT_VERSION := $(shell grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Files that contain the version
VERSION_FILES := Cargo.toml .claude-plugin/marketplace.json gemini-extension.json

# Hard per-process address-space cap (KiB) applied to test runs, so a runaway
# allocation (e.g. an unbounded loop in a test) aborts that single process at
# the limit instead of exhausting system RAM. Override with MEMLIMIT_KB=<kib>,
# or MEMLIMIT_KB=unlimited to disable.
MEMLIMIT_KB ?= 8388608

# Run benchmarks. Pass B= to select a specific bench, e.g.: make bench B=logging_overhead
bench:
	@cargo bench $(if $(B),--bench $(B),) --quiet

# Run benchmark tests with stdout visible. Pass T= to filter.
bench-test:
	@if [ "$(MEMLIMIT_KB)" != unlimited ]; then ulimit -v $(MEMLIMIT_KB); fi; \
	 cargo nextest run --workspace --features mockls --no-capture --status-level all --cargo-quiet $(if $(T),-E 'test($(T))',)

# Default target: run all checks
build-release:
	@cargo build --release

check:
	@PINNED=$$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml); \
	 LATEST=$$(rustup run stable rustc --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+' | head -1); \
	 if [ -n "$$LATEST" ] && [ "$$PINNED" != "$$LATEST" ]; then \
	   printf '\033[33mNote: rust-toolchain.toml pins %s, latest stable is %s\033[0m\n' "$$PINNED" "$$LATEST"; \
	 fi
	@cargo update --quiet
	@cargo fmt -- -l | sed 's/^/fmt: formatted /'
	@cargo clippy --tests --features mockls --quiet -- -D warnings
	@tries=0; while true; do \
	   cargo deny --log-level error check; rc=$$?; \
	   if [ $$rc -eq 0 ]; then break; \
	   elif [ $$rc -ne 139 ]; then exit $$rc; \
	   else \
	     tries=$$((tries + 1)); \
	     if [ $$tries -ge 5 ]; then echo "cargo-deny segfaulted 5 times, giving up"; exit 139; fi; \
	     echo "cargo-deny segfaulted (EmbarkStudios/cargo-deny#855), retry $$tries/5..."; \
	   fi; \
	 done
	@cargo machete --skip-target-dir
	@if [ "$(MEMLIMIT_KB)" != unlimited ]; then ulimit -v $(MEMLIMIT_KB); fi; \
	 cargo nextest run --workspace --features mockls --no-fail-fast --status-level fail --final-status-level fail --cargo-quiet --show-progress only

# Detect unused dependencies
machete:
	@cargo machete --skip-target-dir

# Build the mdbook user-facing docs
mdbook:
	@mdbook build docs

# Build internal rustdoc (includes private items)
rustdoc:
	@cargo doc --document-private-items --no-deps --quiet

# Run cargo-deny license and advisory checks
deny:
	@tries=0; while true; do \
	   cargo deny --log-level error check; rc=$$?; \
	   if [ $$rc -eq 0 ]; then break; \
	   elif [ $$rc -ne 139 ]; then exit $$rc; \
	   else \
	     tries=$$((tries + 1)); \
	     if [ $$tries -ge 5 ]; then echo "cargo-deny segfaulted 5 times, giving up"; exit 139; fi; \
	     echo "cargo-deny segfaulted (EmbarkStudios/cargo-deny#855), retry $$tries/5..."; \
	   fi; \
	 done

# Coverage-guided differential fuzz soak for the shell-tokenization oracle
# (ADR 020 §6, tokenizer ticket 06). OUT-OF-BAND: needs the nightly toolchain and
# cargo-fuzz, runs the detached `fuzz/` workspace, and is NOT part of `make check`
# / CI-stable. Reuses the same `oracle::check()` the proptest layer drives.
# Prereqs: `rustup toolchain install nightly` and `cargo install cargo-fuzz`.
# Pass TARGET= to pick a fuzz target, RUNS= to bound iterations (RUNS=0 = forever).
#   make fuzz                 # differential_oracle, 10k runs
#   make fuzz RUNS=1000000    # longer soak
# TRIPLE pins the build to the GNU host triple: cargo-fuzz otherwise defaults to a
# musl target on some hosts, and the ASAN sanitizer is incompatible with musl's
# statically linked libc. Override TRIPLE= for cross-fuzzing.
TARGET ?= differential_oracle
RUNS ?= 10000
TRIPLE ?= $(shell rustc -vV | sed -n 's/host: //p')
fuzz:
	@cd fuzz && cargo +nightly fuzz run --target $(TRIPLE) $(TARGET) -- -runs=$(RUNS)

# Run mutation testing. Expensive — use before releases, not on every commit.
# Pass T= to scope to specific modules, e.g.: make mutants T=command_filter
# Uses nextest (--test-tool nextest) to match the project's test runner. Several
# tests capture tracing events via THREAD-LOCAL subscribers (set_default /
# with_default) and rely on nextest's process-per-test isolation; under
# cargo-test's shared-process threaded harness those dispatchers race across
# concurrent tests and the unmutated baseline flakes red (e.g.
# companions::mount_emits_debug_event_carrying_companion). nextest gives the same
# green baseline as `make check`.
mutants:
	@mkdir -p $(CURDIR)/../.catenary-mutants-tmp && ulimit -v 16777216 && TMPDIR=$$(realpath $(CURDIR)/../.catenary-mutants-tmp) cargo mutants --test-tool nextest $(if $(T),--package catenary-mcp -F $(T),) --timeout 1200 --jobs 8 --features mockls

# Kill a running cargo-mutants AND all its children (test binaries, mockls, etc.).
# ALWAYS use this to stop mutation testing — NEVER `pkill cargo-mutants`, which
# kills only the parent and orphans the test binaries. Orphans keep running with
# no timeout or memory limit; an orphaned mutant binary once caused a 41.8GB OOM
# that crashed the GPU driver. (The `mutants` target caps each run at ulimit -v
# 16G, but a killed-parent orphan can escape the intended lifecycle.)
mutants-stop:
	@if pgid=$$(ps -o pgid= -p $$(pgrep -x cargo-mutants 2>/dev/null | head -1) 2>/dev/null | tr -d ' '); then \
	  kill -- -$$pgid 2>/dev/null && echo "Killed process group $$pgid"; \
	else \
	  echo "No cargo-mutants process found"; \
	fi

# Run tests. Pass T= to filter, N= to repeat, e.g.: make test T=json_diagnostics N=5
# Prefix with ! to exclude: make test T=\!flaky_test
# Run ignored tests (e.g. requiring real LSP): make test-ignored T=ra_symbol_universe
CLEAN_T = $(subst \,,$(subst !,,$(T)))
# Not memory-capped: real language servers (e.g. rust-analyzer) have legitimately
# large, variable footprints, and this is a rarely-run manual target.
test-ignored:
	@cargo nextest run --workspace --features mockls --run-ignored ignored-only --status-level all --final-status-level all --no-capture $(if $(T),-E 'test($(CLEAN_T))',)

test:
	@if [ "$(MEMLIMIT_KB)" != unlimited ]; then ulimit -v $(MEMLIMIT_KB); fi; \
	 cargo nextest run --workspace --features mockls --status-level fail --final-status-level slow --cargo-quiet $(if $(N),--stress-count $(N),) $(if $(T),$(if $(findstring !,$(T)),-E 'not test($(CLEAN_T))',-E 'test($(T))'),)

# Verify we're in a good state for release
pre-release-check:
	@echo "Checking release prerequisites..."
	@# Clean working tree?
	@if [ -n "$$(git status --porcelain)" ]; then \
		echo "Error: Working tree is not clean. Commit or stash changes first."; \
		exit 1; \
	fi
	@# On main branch?
	@if [ "$$(git branch --show-current)" != "main" ]; then \
		echo "Error: Not on main branch."; \
		exit 1; \
	fi
	@# Up to date with remote?
	@git fetch origin main --quiet
	@if [ "$$(git rev-parse HEAD)" != "$$(git rev-parse origin/main)" ]; then \
		echo "Error: Local main is not up to date with origin/main."; \
		exit 1; \
	fi
	@echo "Prerequisites OK."

# Bump version in all files
bump-version:
	@if [ -z "$(V)" ]; then \
		echo "Error: Version not specified. Use V=x.y.z"; \
		exit 1; \
	fi
	@echo "Bumping version: $(CURRENT_VERSION) -> $(V)"
	@# Update Cargo.toml
	@sed -i 's/^version = "$(CURRENT_VERSION)"/version = "$(V)"/' Cargo.toml
	@# Update marketplace.json
	@sed -i 's/"version": "$(CURRENT_VERSION)"/"version": "$(V)"/' .claude-plugin/marketplace.json
	@# Update gemini-extension.json
	@sed -i 's/"version": "$(CURRENT_VERSION)"/"version": "$(V)"/' gemini-extension.json
	@# Update Cargo.lock
	@cargo check --quiet
	@echo "Version bumped to $(V)"

# Calculate next patch version (0.5.5 -> 0.5.6)
next-patch:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1"."$$2"."$$3+1}'))

# Calculate next minor version (0.5.5 -> 0.6.0)
next-minor:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1"."$$2+1".0"}'))

# Calculate next major version (0.5.5 -> 1.0.0)
next-major:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1+1".0.0"}'))

# Main release target (requires V=x.y.z)
# Rolls back the version bump if checks or commit fail, so it is safe
# to re-run after fixing the issue.
release: pre-release-check
	@if [ -z "$(V)" ]; then \
		echo "Error: Version not specified. Use 'make release V=x.y.z' or 'make release-patch'"; \
		exit 1; \
	fi
	@cargo update --quiet
	@$(MAKE) bump-version V=$(V)
	@if ! $(MAKE) check; then \
		echo "Checks failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock .claude-plugin/marketplace.json gemini-extension.json; \
		exit 1; \
	fi
	@git add Cargo.toml Cargo.lock .claude-plugin/marketplace.json gemini-extension.json
	@if ! git commit -m "chore: Bump version to $(V)"; then \
		echo "Commit failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock .claude-plugin/marketplace.json gemini-extension.json; \
		exit 1; \
	fi
	@git tag -a "v$(V)" -m "Release v$(V)"
	@echo ""
	@echo "Release v$(V) prepared locally."
	@echo "Run 'make publish' to build, push, and create the release."

# Convenience targets
release-patch: pre-release-check next-patch
	@$(MAKE) release V=$(V)

release-minor: pre-release-check next-minor
	@$(MAKE) release V=$(V)

release-major: pre-release-check next-major
	@$(MAKE) release V=$(V)

# Push tags — CD workflow handles builds, releases, and crates.io publishing.
# Requires: release commit + tag already created (via make release-*)
publish:
	@echo "Pushing to origin..."
	@git push && git push --tags
	@echo ""
	@echo "Release v$(CURRENT_VERSION) pushed. CD workflow will build and publish."

# Tag current version (for when you forgot to tag)
tag-current:
	@if git rev-parse "v$(CURRENT_VERSION)" >/dev/null 2>&1; then \
		echo "Tag v$(CURRENT_VERSION) already exists."; \
		exit 1; \
	fi
	@echo "Creating tag v$(CURRENT_VERSION) for current version..."
	@git tag -a "v$(CURRENT_VERSION)" -m "Release v$(CURRENT_VERSION)"
	@echo "Tag created. Run 'make publish' to build and release."

# Show current version info
version:
	@echo "Current version: $(CURRENT_VERSION)"
	@echo "Latest tag:      $$(git describe --tags --abbrev=0 2>/dev/null || echo 'none')"
	@echo ""
	@echo "Version in files:"
	@grep -H 'version' Cargo.toml | head -1
	@grep -H 'version' .claude-plugin/marketplace.json | grep -v schema
	@grep -H 'version' gemini-extension.json
