# Catenary Release Makefile
# Usage:
#   make release-patch   # 0.5.5 -> 0.5.6
#   make release-minor   # 0.5.5 -> 0.6.0
#   make release-major   # 0.5.5 -> 1.0.0
#   make release V=0.6.0 # explicit version

.PHONY: bench bench-test build-release bump-guard check clippy conformance conformance-matrix deny flake-hunt fuzz install machete mdbook mockgrep mockglob mdgrep mdglob publish-backstop refresh-recipes registry-selftest rustgrep rustglob mutants mutants-stop mutants-flag-runaways rustdoc sweep test test-ignored release release-patch release-minor release-major publish publish-check tag-current

# Get current version from Cargo.toml
CURRENT_VERSION := $(shell grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Files that contain the version
VERSION_FILES := Cargo.toml .claude-plugin/marketplace.json

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

check: sweep
	@PINNED=$$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml); \
	 LATEST=$$(rustup run stable rustc --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+' | head -1); \
	 if [ -n "$$LATEST" ] && [ "$$PINNED" != "$$LATEST" ]; then \
	   printf '\033[33mNote: rust-toolchain.toml pins %s, latest stable is %s\033[0m\n' "$$PINNED" "$$LATEST"; \
	 fi
	@cargo update --quiet
	@cargo fmt -- -l | sed 's/^/fmt: formatted /'
	@$(MAKE) clippy
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

# Clippy — CI's single source of truth for the lint gate (misc 202).
#
# The lint SET (the pedantic/nursery/cargo groups + the hard denials) lives in
# Cargo.toml's `[workspace.lints.clippy]`, so it is a PROJECT property the editor
# and the `catenary diagnostics` receipt read too (rust-analyzer.toml runs clippy
# for flycheck). This invocation therefore carries NO group flags — only `-D
# warnings`, CI's deny lever: the same warnings live in the editor/receipt, and go
# red here. `--all-targets --all-features` is the broadest surface (lib, bins,
# tests, benches; both `mockls` and `fuzzing`), a superset of the test/build
# feature sets, so no gated module escapes the gate. Both `make check` and
# `.github/workflows/ci.yml` call this target — the flags live in one place.
clippy:
	@cargo clippy --all-targets --all-features --quiet -- -D warnings

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
	     cargo mutants --test-tool nextest $(if $(T),--package catenary-cli -F $(T),) --timeout 1200 --jobs $(J) --features mockls $(if $(ITERATE),--iterate,) ; \
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

# ── flake hunt (misc 194) ─────────────────────────────────────────────
# Loop the FULL parallel test suite up to N times, stopping on the FIRST red
# run. Some flakes (conformance_taplo's "dirty fixture renders [clean]") only
# fire under the peak-parallel full-suite load, and pass standalone — so a single
# `make test` cannot reproduce them; this hunts by repetition. On a catch the
# keep-on-panic guard (tests/common/mod.rs) has ALREADY preserved the failing
# test's tempdirs (daemon firehose, daemon.log, bridge_stderr*.log) and eprintln'd
# their paths into nextest's captured failure output shown below — no rerun, so the
# evidence is not lost to a passing retry. N= overrides the iteration cap (default
# 10); each iteration is one full suite (~90s), so budget accordingly. Default
# nextest capture is deliberate: it keeps the run fully parallel (the flake's
# precondition) while still printing a failing test's captured output.
#   make flake-hunt            # up to 10 full-suite passes, stop on first red
#   make flake-hunt N=12       # up to 12
#   make flake-hunt N=1        # one full pass
HUNT_N ?= $(if $(N),$(N),10)
flake-hunt:
	@if [ "$(MEMLIMIT_KB)" != unlimited ]; then ulimit -v $(MEMLIMIT_KB); fi; \
	 i=1; while [ $$i -le $(HUNT_N) ]; do \
	   echo "── flake-hunt: iteration $$i/$(HUNT_N) ──"; \
	   start=$$(date +%s); \
	   if ! cargo nextest run --workspace --features mockls --no-fail-fast --cargo-quiet; then \
	     echo "── flake-hunt: RED on iteration $$i (evidence pinned above by keep-on-panic) ──"; \
	     exit 1; \
	   fi; \
	   echo "── flake-hunt: iteration $$i green in $$(( $$(date +%s) - start ))s ──"; \
	   i=$$((i + 1)); \
	 done; \
	 echo "── flake-hunt: $(HUNT_N) full-suite passes all green — no catch ──"

# ── conformance harness (tui-rework 07) ───────────────────────────────
# Runs ONLY the language-server conformance harness. On the maintainer host the
# dogfooded-fleet sentinels exercise it (each skips where its binary is absent);
# select one server for an exact run — mirroring a CI matrix job — with
# CATENARY_CONFORMANCE, e.g.:
#   make conformance                        # dogfooded fleet, skip-if-missing
#   make conformance CONFORMANCE=pyright    # exactly pyright, require its binary
conformance:
	@$(if $(CONFORMANCE),CATENARY_CONFORMANCE=$(CONFORMANCE),) \
	 cargo nextest run --workspace --features mockls --no-fail-fast --status-level all \
	   -E 'binary(conformance_harness)'

# Print a conformance CI matrix exactly as the discover job generates it
# (stdout is the JSON; stderr carries the job census and skip annotations) —
# the local pre-flight for pin-file edits. PLATFORM=macos emits the Homebrew
# leg (misc 164), including its partition validation against the
# Linux-conformed set; the default is the Linux matrix.
#   make conformance-matrix
#   make conformance-matrix PLATFORM=macos
conformance-matrix:
	@python3 tools/conformance_matrix.py $(if $(PLATFORM),--platform $(PLATFORM),)

# Re-resolve, re-hash, and rewrite the CI-internal install-recipe pins in
# defaults/recipes.toml, then show the reviewable diff. This PREPARES a pin bump
# for review; it never merges — the merge policy (mechanical gates, auto-merge on
# green) is tui-rework 08's concern. Offline-tolerant: a per-server resolve
# failure keeps that pin and is reported; the file is never partially rewritten.
refresh-recipes:
	@python3 tools/refresh_recipes.py
	@git --no-pager diff -- defaults/recipes.toml || true

# Exercise the STAGED external-registry pipeline scripts (tui-rework 08). These
# live under registry/ as ready-to-instantiate content for the registry repo (see
# registry/README.md); they never run in this repo's CI. --self-test runs their
# pure gate/rollback logic with no network, so the maintainer can validate the
# staged scripts here before instantiating the registry repo.
registry-selftest:
	@python3 registry/gate_pipeline.py --self-test
	@python3 registry/rescan.py --self-test

# ── isolated server harness (tickets 00b / 00c) ───────────────────────
# A hermetic "build and see" harness for eyeballing enriched `catenary
# grep`/`glob` output against a language server under a PRIVATE daemon. Each
# fixture binds the run to its OWN XDG bases under target/ (its own daemon +
# socket, so it never touches the user's live daemon), points CATENARY_SERVERS
# at the one server it exercises, and CATENARY_ROOTS at its fixture dir. NONE of
# these targets are part of `make check`/`make test` — manual inspection only,
# so the default build never depends on Lattice or rust-analyzer.
#
# Three fixtures share one parameterized recipe ($(call iso_run,...)):
#   mockgrep/mockglob   mockls       over tests/fixtures/mock_edges  (offline, 00b)
#   mdgrep/mdglob       Lattice      over tests/fixtures/md_repo     (real,    00c)
#   rustgrep/rustglob   rust-analyzer over tests/fixtures/rust_repo  (real,    00c)
# The mock fixture is fully self-contained; the two real-server fixtures need
# `lattice` / `rustup` on PATH.
#
# $(call iso_run,<home-dir>,<env-prefix>,<catenary subcommand + args>)
# Builds the catenary + mockls binaries, starts an isolated daemon under
# <home-dir>'s XDG bases, waits for its socket, runs the given catenary
# subcommand against the daemon (the daemon blocks the query on server
# readiness, so the first query is already enriched — real servers cold-start +
# index, so the first run can take a while), then tears the daemon down on exit
# via the trap. Unlike the test harness's `isolate_env`, PATH/HOME are inherited,
# so `lattice` / `rustup` resolve and rust-analyzer finds its toolchain.
ISO_CAT := $(CURDIR)/target/debug/catenary
define iso_run
@cargo build --quiet --features mockls --bin catenary --bin mockls
@mkdir -p $(1)/config $(1)/state $(1)/data $(1)/cache $(1)/runtime
@sock=$(1)/state/catenary/catenary.sock; \
	rm -f $$sock; \
	$(2) $(ISO_CAT) daemon >$(1)/daemon.log 2>&1 & \
	dpid=$$!; \
	trap 'kill $$dpid 2>/dev/null; wait $$dpid 2>/dev/null' EXIT INT TERM; \
	for i in $$(seq 1 150); do [ -S $$sock ] && break; sleep 0.1; done; \
	if [ ! -S $$sock ]; then \
		echo "iso_run: daemon failed to start (see $(1)/daemon.log):"; \
		cat $(1)/daemon.log; exit 1; \
	fi; \
	$(2) $(ISO_CAT) $(3)
endef

# ── mock_edges (ticket 00b): mockls, offline, deterministic ───────────
# The fixture under tests/fixtures/mock_edges is written in the `mockls`
# mini-language; mockls (absolute path) stands in for a real LSP so the run is
# deterministic and offline, never routing to a real server.
#
#   make mockgrep                 # default Q=Shape: impls / super / sub + refs
#   make mockgrep Q=process       # outgoing + incoming calls, incl. cross-file
#   make mockgrep Q=log_event     # cross-file incoming call
#   make mockgrep Q=buried_fn     # a 2-levels-deep nested symbol
#   make mockgrep Q=loose_marker  # bare match with no enclosing symbol
#   make mockgrep Q='Shape|process'  # alternation: one section PER arm
#   make mockglob                 # structural map (top-level outlines)
MOCK_EDGES_EXT    := yX4Za
MOCK_EDGES_DIR    := $(CURDIR)/tests/fixtures/mock_edges
MOCK_EDGES_HOME   := $(CURDIR)/target/mock_edges
MOCK_EDGES_MOCKLS := $(CURDIR)/target/debug/mockls
MOCK_EDGES_ENV     = env XDG_CONFIG_HOME=$(MOCK_EDGES_HOME)/config \
	XDG_STATE_HOME=$(MOCK_EDGES_HOME)/state \
	XDG_DATA_HOME=$(MOCK_EDGES_HOME)/data \
	XDG_CACHE_HOME=$(MOCK_EDGES_HOME)/cache \
	XDG_RUNTIME_DIR=$(MOCK_EDGES_HOME)/runtime \
	CATENARY_ROOTS=$(MOCK_EDGES_DIR) \
	CATENARY_SERVERS='$(MOCK_EDGES_EXT):$(MOCK_EDGES_MOCKLS) $(MOCK_EDGES_EXT) --scan-roots'

# Grep query override; the default surfaces the type-hierarchy cluster (the
# interface Shape: its impls, supertypes, subtypes, and cross-file refs).
Q ?= Shape

# Run `catenary grep` over the fixture under the isolated daemon. Q= overrides.
mockgrep:
	$(call iso_run,$(MOCK_EDGES_HOME),$(MOCK_EDGES_ENV),grep '$(Q)' $(MOCK_EDGES_DIR))

# Run `catenary glob` over the fixture under the isolated daemon. The quoted
# recursive pattern resolves to the fixture's files, so each renders its
# documentSymbol outline (the structural map, incl. the ≥2-level nesting in
# nested.yX4Za) rather than a bare directory listing.
mockglob:
	$(call iso_run,$(MOCK_EDGES_HOME),$(MOCK_EDGES_ENV),glob '$(MOCK_EDGES_DIR)/**/*.$(MOCK_EDGES_EXT)')

# ── md_repo (ticket 00c): real Lattice over markdown ──────────────────
# Lattice codifies markdown into the LSP edge model (headings -> documentSymbols;
# inline links + frontmatter `referenced_by` -> references). The repo is a small
# backlink graph in which core.md links to the README five times, so the README
# has many incoming links. `catenary grep` fires the full nav suite on every
# match that resolves to a Lattice symbol and prints the matched document's
# incoming references beneath it — so a query that lands on the README's title
# region reprints that whole incoming-ref block under each such match. That
# per-match repetition of document-level refs is the faithful demonstrator for
# the "24-page" doc-scoped-refs bloat that mockls (symbol-scoped) cannot
# reproduce. Requires `lattice` on PATH (the markdown default server).
#
#   make mdgrep                 # default Q=Core: README's incoming-ref block, repeated
#   make mdgrep Q=pipeline      # heading matches: full nav suite fired, refs come back empty
#   make mdglob                 # documentSymbol outlines (frontmatter + headings)
MD_REPO_EXT  := md
MD_REPO_DIR  := $(CURDIR)/tests/fixtures/md_repo
MD_REPO_HOME := $(CURDIR)/target/md_repo
MD_REPO_ENV   = env XDG_CONFIG_HOME=$(MD_REPO_HOME)/config \
	XDG_STATE_HOME=$(MD_REPO_HOME)/state \
	XDG_DATA_HOME=$(MD_REPO_HOME)/data \
	XDG_CACHE_HOME=$(MD_REPO_HOME)/cache \
	XDG_RUNTIME_DIR=$(MD_REPO_HOME)/runtime \
	CATENARY_ROOTS=$(MD_REPO_DIR) \
	CATENARY_SERVERS='md:lattice serve'

mdgrep: Q = Core
mdgrep:
	$(call iso_run,$(MD_REPO_HOME),$(MD_REPO_ENV),grep '$(Q)' $(MD_REPO_DIR))

mdglob:
	$(call iso_run,$(MD_REPO_HOME),$(MD_REPO_ENV),glob '$(MD_REPO_DIR)/**/*.$(MD_REPO_EXT)')

# ── rust_repo (ticket 00c): real rust-analyzer over a tiny crate ──────
# A minimal crate (trait Animal, subtrait Pet, structs Dog/Cat, a small call
# graph) so impl / super / sub / caller / callee edges all have something to
# show — a sanity check that the format holds against a production LSP. The
# crate is `workspace.exclude`d so cargo never compiles it as part of the build.
# Requires `rustup` on PATH; cold-start + indexing makes the first run slow.
#
#   make rustgrep               # default Q=Animal: impls + subtype (Pet)
#   make rustgrep Q=describe    # callers / callees in the call graph
#   make rustglob               # documentSymbol outline of src/lib.rs
RUST_REPO_EXT  := rs
RUST_REPO_DIR  := $(CURDIR)/tests/fixtures/rust_repo
RUST_REPO_HOME := $(CURDIR)/target/rust_repo
RUST_REPO_ENV   = env XDG_CONFIG_HOME=$(RUST_REPO_HOME)/config \
	XDG_STATE_HOME=$(RUST_REPO_HOME)/state \
	XDG_DATA_HOME=$(RUST_REPO_HOME)/data \
	XDG_CACHE_HOME=$(RUST_REPO_HOME)/cache \
	XDG_RUNTIME_DIR=$(RUST_REPO_HOME)/runtime \
	CATENARY_ROOTS=$(RUST_REPO_DIR) \
	CATENARY_SERVERS='rs:rustup run stable rust-analyzer'

rustgrep: Q = Animal
rustgrep:
	$(call iso_run,$(RUST_REPO_HOME),$(RUST_REPO_ENV),grep '$(Q)' $(RUST_REPO_DIR))

rustglob:
	$(call iso_run,$(RUST_REPO_HOME),$(RUST_REPO_ENV),glob '$(RUST_REPO_DIR)/**/*.$(RUST_REPO_EXT)')

# Install the working tree's binary onto PATH and bounce the daemon — the
# maintainer's nightly distribution channel as one word, now including the
# bounce (the v2 release day ran an entire session against a 30-commit-stale
# binary and daemon because the raw ritual was skipped). Prints the version
# pair first so the skew being fixed is visible, then stops the daemon: the
# respawn heals the rename-swap marker (misc 182), so live bridges revive
# the NEW build and replay their sessions on their own — the bounce is safe
# to automate. `--force` skips the TTY confirmation (the bounce is the
# point here); no daemon running is a clean no-op.
install: sweep
	@cargo install --path . --locked
	@catenary version
	@catenary stop --force

# Continuous target-dir garbage collection. Cargo never deletes artifacts, and
# on a nightly-rebuild machine every toolchain bump orphans the previous
# vintage wholesale (observed: 500 GB of dead incremental/deps). `cargo sweep
# --installed` deletes every artifact not produced by a currently-installed
# toolchain — a no-op scan (seconds) when nothing is stale, a bulk reclaim the
# morning after a toolchain bump. Riding `check` and `install` makes the GC
# continuous; absent cargo-sweep it skips with a hint (CI runners and fresh
# machines never break on it). One-time setup: cargo install cargo-sweep.
sweep:
	@if command -v cargo-sweep >/dev/null 2>&1; then \
	   cargo sweep --installed | tail -1; \
	 else \
	   echo "sweep: cargo-sweep not installed — skipping stale-artifact GC (cargo install cargo-sweep)"; \
	 fi

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
	@$(MAKE) publish-check
	@echo "Prerequisites OK."

# ── ws41-03: bridge-crate bump guard + workspace publish backstop ─────
# The per-PR guard: any PR whose diff touches the catenary-mcp bridge crate
# (docs INCLUDED) must bump that crate's `version` — the daemon<->bridge
# handshake comparand — in the same PR. NO escape hatch (maintainer-ruled): a
# false positive costs one harmless /mcp, a false negative is a silent wire
# divergence, and an override is a judgment surface through which that failure
# re-enters. Runs in CI on pull_request (see .github/workflows/ci.yml,
# bridge-bump-guard job); BASE is the diff base (locally origin/main, in CI the
# PR base sha). HEAD overrides the head ref for local testing against synthetic
# commits.
#   make bump-guard BASE=origin/main
#   make bump-guard BASE=<base-sha> HEAD=<head-sha>
BASE ?= origin/main
bump-guard:
	@sh scripts/bridge-bump-guard.sh "$(BASE)" $(if $(HEAD),"$(HEAD)",)

# The workspace-wide, fail-not-skip publish backstop: for EVERY published crate
# (catenary-proc included, not just the bridge crate), FAIL — never skip — when a
# crate's content differs from its already-published version. crates.io protects
# the registry entry, never the version claim compiled into a binary via a path
# dep, so a changed-but-unbumped crate would ship as a stale registry artifact
# under a version a from-source build then compiles against new code. Read-only:
# for an already-published crate it GETs the sparse-index line (as publish-check
# does) and the static-CDN .crate, then compares payloads; no credential, no
# write. Runs in CD before the publish (see cd.yml). Pass crate names to scope it
# (tests drive one crate).
#   make publish-backstop
#   make publish-backstop CRATES=catenary-proc
publish-backstop:
	@sh scripts/publish-backstop.sh $(CRATES)

# Verify the crates will publish cleanly BEFORE any tag exists (the v2.0.0
# crates.io failure: catenary-proc's API grew under a frozen, already-published
# version, so catenary-mcp's publish-time verify — which resolves the PUBLISHED
# proc, not the local one — could not compile; the tag-bound publish job was
# the first place reality could object). Two modes, keyed on whether the local
# catenary-proc version already exists in the crates.io index:
#   already published  ->  catenary-mcp must verify against the INDEX
#                          (cargo publish --dry-run), because the release will
#                          not publish a new proc first
#   new version        ->  the release publishes proc first, so the pair is
#                          verified TOGETHER against the local packaged crates
#                          (cargo package --workspace) — an index dry-run here
#                          would false-positive on every legitimate proc bump
publish-check:
	@echo "Verifying crates publish cleanly..."
	@proc_ver=$$(grep -m1 '^version' crates/catenary-proc/Cargo.toml | cut -d'"' -f2); \
	if curl -fsSL https://index.crates.io/ca/te/catenary-proc | grep -q "\"vers\":\"$$proc_ver\""; then \
		echo "catenary-proc $$proc_ver is already published — verifying catenary-mcp against the index..."; \
		if ! cargo publish --dry-run -p catenary-mcp; then \
			echo "Error: catenary-mcp does not build against the PUBLISHED catenary-proc $$proc_ver."; \
			echo "Bump catenary-proc's version and the workspace dep spec so the release publishes it first."; \
			exit 1; \
		fi; \
	else \
		echo "catenary-proc $$proc_ver is new — verifying the workspace package set together..."; \
		cargo package --workspace; \
	fi

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
		git checkout HEAD -- Cargo.toml Cargo.lock .claude-plugin/marketplace.json; \
		exit 1; \
	fi
	@git add Cargo.toml Cargo.lock .claude-plugin/marketplace.json
	@if ! git commit -m "chore: Bump version to $(V)"; then \
		echo "Commit failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock .claude-plugin/marketplace.json; \
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

# Push the release — CD workflow handles builds, releases, and crates.io
# publishing. Requires: release commit + tag already created (via make
# release-*). Pushes only the CURRENT version's tag: `git push --tags` tried
# to re-push every historical tag and exited nonzero on ancient local tags
# whose objects differ from the remote's — after the pushes that mattered had
# already succeeded (v2.0.0 release papercut).
publish:
	@echo "Pushing to origin..."
	@git push && git push origin "v$(CURRENT_VERSION)"
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
