# Differential fuzzing — faithful shell tokenization (ADR 020 §6)

A coverage-guided [`cargo-fuzz`](https://rust-fuzz.github.io/book/cargo-fuzz.html)
soak for the hand-rolled shell parser in `src/cli/command_filter`. The target body
is the **same** `oracle::check()` the `proptest` layer drives (tokenizer ticket 05),
reused verbatim through the parent crate's `fuzzing` feature — one copy of the
differential oracle, two harnesses:

- `proptest` on **stable CI** (`make check`) — the everyday guard over structured
  shell-ish inputs.
- `cargo-fuzz` for the **nightly soak** here — the long tail `proptest`'s
  generators won't reach (raw bytes, control characters, deeply nested
  substitutions, adversarial quoting).

`check()` parses an input with our parser and with the `brush-parser` bash-fidelity
reference, projects both to the gate's view (command-position words + the
redirect-operator signature), and asserts they agree. A disagreement `panic!`s,
which libFuzzer records as a crash artifact and minimizes. Per ADR 020 §5/§6 the
property is **correctness**, not robustness: we hunt for over-counting (a
false-deny — the agent's everyday pain) and under-counting (a missed command that
wedges the daemon / hides an edit), not "exploits".

## This is out-of-band — not CI-stable

`fuzz/` is a **detached** cargo workspace (`fuzz/Cargo.toml` declares its own
`[workspace]`, and the parent `Cargo.toml` `exclude`s it). It needs the **nightly**
toolchain and `libfuzzer-sys`, so `make check`, `cargo deny`, and `cargo machete`
on stable never descend into it. `brush-parser` rides in only via the `fuzzing`
feature; it never enters the shipped runtime / `cargo deny` graph.

## Prerequisites

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Running

From the repository root:

```sh
make fuzz                 # default: differential_oracle, 10k runs
make fuzz RUNS=1000000    # longer soak
make fuzz TARGET=differential_oracle RUNS=60   # explicit target
```

Or directly with `cargo-fuzz` (nightly is selected automatically by `cargo fuzz`):

```sh
cargo fuzz build                                      # compile only
cargo fuzz run differential_oracle -- -runs=10000     # bounded soak
cargo fuzz run differential_oracle                    # unbounded soak
```

`cargo fuzz run` replays the seeded `corpus/differential_oracle/` first, then
explores. With a correct parser the seeded corpus replays clean (no crashes).

## Seed corpus

`corpus/differential_oracle/` is seeded with the known bug 11/13/17/20/30/33 repros
(the `'\''` apostrophe idiom, bracketed `-m` message, quoted `>`, `\`-newline line
continuation, `;`/newline separators, pipelines, nested `$()` / process
substitution, here-doc) so they remain permanent regressions and give the fuzzer
interesting starting points. The same inputs are pinned as fixed `check()` cases in
the `proptest` layer's `SEED_CORPUS`.

## On a found counterexample (ADR 020 §6 workflow)

1. libFuzzer writes a crash artifact under `fuzz/artifacts/differential_oracle/`.
2. Minimize it: `cargo fuzz tmin differential_oracle <artifact>`.
3. Graduate the minimized input into an assert-on-value unit test (ticket 01/02
   style) and fix the parser.
4. Keep the artifact in `corpus/differential_oracle/` so it stays a regression.
