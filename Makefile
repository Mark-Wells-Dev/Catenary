# Catenary Release Makefile
# Usage:
#   make release-patch   # 0.5.5 -> 0.5.6
#   make release-minor   # 0.5.5 -> 0.6.0
#   make release-major   # 0.5.5 -> 1.0.0
#   make release V=0.6.0 # explicit version

.PHONY: bench bench-test build-release check deny fuzz machete mdbook mutants mutants-stop mutants-flag-runaways rustdoc test test-ignored release release-patch release-minor release-major publish tag-current

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
#
# Concurrency: cargo-mutants runs J mutant pipelines in parallel; each runs
# nextest with TT test threads, and each integration test spawns a daemon +
# mockls. Two peaks: the BUILD phase scales with J alone (J cold-ish rustc/link
# jobs — the RAM hog); the TEST phase with J x TT (that many daemon+mockls trees).
# Keep J x TT at or below the core count for CPU; lower J to cut peak RAM.
#   make mutants T=command_filter J=3 TT=4
#
# RAM safety. `ulimit -v 16G` caps any SINGLE process — it does NOT cap the
# aggregate (J trees can still sum past RAM and thrash zram/swap), and a
# memory-runaway mutant (a mutation that drops a loop bound / alloc guard) can
# thrash for the whole --timeout window. Set MEMCAP for a hard, aggregate,
# swap-proof cap: the run executes in a transient systemd cgroup scope with swap
# FORBIDDEN, so the kernel OOM-kills *inside* the run instead of thrashing the
# box. Requires systemd (cgroup v2); unset = per-process ulimit only.
#   make mutants T=command_filter MEMCAP=48G
#
# Each run then auto-runs the runaway scan (the same logic as the standalone
# `make mutants-flag-runaways` target — inlined here rather than a recursive
# $(MAKE) so that `make -n mutants` stays a true dry-run), scanning the per-mutant
# logs for OOM-kills / SIGKILLs / allocation failures / timeouts and writing
# mutants.out/runaway.txt. A flagged mutant's "caught" outcome (if any) came from
# a resource kill, not an assertion — audit each (a bounding test, or a
# documented #[mutants::skip]). Under an aggregate MEMCAP the kernel may kill a
# bystander, so attribution is best-effort; a per-mutant cap would be exact.
#
# --iterate is ON by default (ITERATE ?= 1): skip mutants caught in a previous
# run (read from the prior mutants.out) so an interrupted run resumes cheaply
# instead of restarting. Safe on a fresh run — with nothing previously caught it
# is a no-op and tests everything; it also accumulates caught mutants across runs.
# Force a from-scratch run with an empty override:
#   make mutants T=command_filter ITERATE=
J  ?= 3
TT ?= 4
ITERATE ?= 1
MEMCAP ?=
# Wrap the run in a swap-forbidden cgroup scope when MEMCAP is set (e.g. 48G).
MUTANTS_MEMCAP_WRAP = $(if $(MEMCAP),systemd-run --user --scope -p MemoryMax=$(MEMCAP) -p MemorySwapMax=0 --,)
# Per-mutant log signatures of a resource runaway (OOM / SIGKILL / alloc fail / timeout).
MUTANTS_RUNAWAY_RE := SIGKILL|signal:? 9|[Oo]ut of [Mm]emory|memory allocation of [0-9]+ bytes failed|cannot allocate memory|oom.?kill|TIMEOUT
# Reuse compiled dependencies across the per-job scratch target dirs (cargo-mutants
# makes one per --jobs, each otherwise rebuilding the full dep graph cold) and
# across runs, via sccache when present. Auto-detected — a no-op without sccache.
SCCACHE := $(shell command -v sccache 2>/dev/null)
mutants:
	@mkdir -p $(CURDIR)/../.catenary-mutants-tmp && ulimit -v 16777216 && { \
	   $(MUTANTS_MEMCAP_WRAP) env $(if $(SCCACHE),RUSTC_WRAPPER=$(SCCACHE) CARGO_INCREMENTAL=0 ,)NEXTEST_TEST_THREADS=$(TT) TMPDIR=$$(realpath $(CURDIR)/../.catenary-mutants-tmp) \
	     cargo mutants --test-tool nextest $(if $(T),--package catenary-mcp -F $(T),) --timeout 1200 --jobs $(J) --features mockls $(if $(ITERATE),--iterate,) ; \
	   rc=$$? ; \
	   out=$(CURDIR)/mutants.out ; \
	   if [ -d $$out/log ]; then \
	     : > $$out/runaway.txt ; \
	     for f in $$out/log/*.log ; do \
	       [ -e "$$f" ] || continue ; \
	       if grep -Eqi '$(MUTANTS_RUNAWAY_RE)' "$$f" ; then basename "$$f" .log >> $$out/runaway.txt ; fi ; \
	     done ; \
	     sort -u -o $$out/runaway.txt $$out/runaway.txt ; \
	     echo "runaway/killed/timed-out mutants flagged: $$(wc -l < $$out/runaway.txt) (see mutants.out/runaway.txt; timeouts also in timeout.txt)" ; \
	   else echo "no mutants.out/log — nothing to scan" ; fi ; \
	   exit $$rc ; \
	 }

# Flag resource-runaway mutants: scan the per-mutant logs for OOM-kills /
# SIGKILLs / allocation failures / timeouts and write mutants.out/runaway.txt
# (one mutant per line). A flagged mutant's "caught" outcome, if any, is a
# resource kill — not a meaningful assertion — so audit each: it is either a
# latent unbounded loop / allocation (give it a bounding test) or a documented
# #[mutants::skip].
mutants-flag-runaways:
	@out=$(CURDIR)/mutants.out ; \
	 if [ ! -d $$out/log ]; then echo "no mutants.out/log — nothing to scan" ; exit 0 ; fi ; \
	 : > $$out/runaway.txt ; \
	 for f in $$out/log/*.log ; do \
	   [ -e "$$f" ] || continue ; \
	   if grep -Eqi '$(MUTANTS_RUNAWAY_RE)' "$$f" ; then basename "$$f" .log >> $$out/runaway.txt ; fi ; \
	 done ; \
	 sort -u -o $$out/runaway.txt $$out/runaway.txt ; \
	 echo "runaway/killed/timed-out mutants flagged: $$(wc -l < $$out/runaway.txt) (see mutants.out/runaway.txt; timeouts also in timeout.txt)"

# Kill a running cargo-mutants AND its orphans. Two kinds of children escape a
# naive `pkill cargo-mutants`: (1) test binaries in cargo-mutants' process group
# — handled by the process-group kill (step 1); and (2) the `catenary daemon` /
# `mockls` an integration test spawns, which DAEMONIZE into their own session and
# so survive a group kill (step 2). Orphans keep running with no timeout or memory
# limit; an orphaned mutant binary once caused a 41.8GB OOM that crashed the GPU
# driver. Step 2 sweeps the daemonized orphans by their scratch-dir executable
# path ($(CURDIR)/../.catenary-mutants-tmp/cargo-mutants-*), which is unique to a
# mutation run — so it can NEVER hit your interactive session daemon (that does
# not run from there). ALWAYS use this to stop mutation testing; NEVER a bare
# `pkill cargo-mutants`.
mutants-stop:
	@if pgid=$$(ps -o pgid= -p $$(pgrep -x cargo-mutants 2>/dev/null | head -1) 2>/dev/null | tr -d ' ') && [ -n "$$pgid" ]; then \
	  kill -- -$$pgid 2>/dev/null && echo "Killed cargo-mutants process group $$pgid"; \
	else \
	  echo "No cargo-mutants process found"; \
	fi
	@scratch=$$(realpath $(CURDIR)/../.catenary-mutants-tmp 2>/dev/null) ; \
	 if [ -n "$$scratch" ] && [ -d "$$scratch" ]; then \
	   orphans=$$(pgrep -f "$$scratch/cargo-mutants" 2>/dev/null | grep -vx "$$$$" 2>/dev/null) ; \
	   if [ -n "$$orphans" ]; then \
	     echo "Killing orphaned mutation processes (daemonized out of the group):" ; \
	     pgrep -af "$$scratch/cargo-mutants" 2>/dev/null ; \
	     kill $$orphans 2>/dev/null ; sleep 2 ; \
	     leftover=$$(pgrep -f "$$scratch/cargo-mutants" 2>/dev/null | grep -vx "$$$$" 2>/dev/null) ; \
	     [ -n "$$leftover" ] && kill -9 $$leftover 2>/dev/null && echo "SIGKILLed stragglers" ; \
	     echo "Orphans cleared." ; \
	   else \
	     echo "No scratch-dir orphans found." ; \
	   fi ; \
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
