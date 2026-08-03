# Test Suite Inventory and Gate Speedup

## Objective

Establish exactly how many tests exist, which gate runs which subset and when,
decide what is worth keeping, and cut wall-clock time from the commit loop and
from CI without losing coverage that matters.

## Current State — the numbers

### Test counts

| Kind | Count |
|---|---|
| Unit tests in `src/` (`#[test]` + `#[tokio::test]`) | 1,946 |
| Integration tests in `crates/*/tests/` (15 files) | 71 |
| **Total test functions** | **2,017** |
| Doctests | 0 (all fences are `text`/`sh`/`ignore`) |
| `#[ignore]`-gated | 24 (22 developer-only, 2 run by CI) |

Per crate: `fono-core` 411, `fono` 372, `fono-tts` 230, `fono-stt` 215,
`fono-assistant` 157, `fono-audio` 136, `fono-net` 91, `fono-mcp-server` 83,
`fono-bench` 63, `fono-overlay` 63, `fono-polish` 62, `fono-hotkey` 41,
`fono-net-codec` 37, `fono-inject` 20, `fono-update` 16, `fono-http` 9,
`fono-tray` 6, `fono-ipc` 5, `fono-download` 0.

### What runs when

| Gate | Trigger | Tests covered |
|---|---|---|
| AGENTS pre-commit (`AGENTS.md:164`) | every commit + push, by discipline (no git hook installed) | all 2,017 minus ignored |
| `tests/check.sh` default (`tests/check.sh:177`, `:180`) | manual | the same 2,017 — **run twice** |
| `tests/check.sh --quick` (`tests/check.sh:167`) | manual | 1,946 lib tests only |
| `tests/check.sh --size-budget` (`tests/check.sh:208-290`) | before every push/tag | zero tests |
| CI `test` (`.github/workflows/ci.yml:102`) | push main + PR | all 2,017 |
| CI latency smoke (`.github/workflows/ci.yml:105`) | push main + PR | 2 ignored tests, own `--release` build |
| CI tiny.en equivalence (`.github/workflows/ci.yml:132-161`) | push main + PR | 0 test fns; runs the binary |
| CI `macos` (`.github/workflows/ci.yml:464`) | push main + PR | same set, darwin |
| CI `windows` (`.github/workflows/ci.yml:703`) | push main + PR, `continue-on-error` | `fono` crate only |
| CI `size-budget` ×3, `size-budget-macos` | push main + PR | zero tests |
| `container.yml` ×2 | every push to main, **no `paths-ignore`** | zero tests |
| `release.yml` cloud gates | tag only | zero test fns |

### Where the time actually goes

Not in running tests. Concentrations of cost, in order:

- **~48 executables get linked** by `--all-targets` (19 lib + 2 bin + 16
  integration + 10 examples + 1 bench); ~46 of them link whisper.cpp, llama.cpp
  and a ~50 MiB `libonnxruntime.a`, because `fono-core` gains `llama-local`
  under workspace feature unification and 17 of 19 crates depend on it.
- **No `[profile.dev]` or `[profile.test]` exists** (`Cargo.toml:283-304` has
  only `release` and `release-slim`), so tests build at `debug = 2` full DWARF,
  and `whisper-rs-sys` builds its C++ as `RelWithDebInfo` under
  `debug_assertions` — DWARF gets copied into every one of those 48 links.
- **`.cargo/config.toml:39-63` applies `--gc-sections` and `--exclude-libs,ALL`
  to every profile**, though both exist solely for the shipped artefact.
- **`check.sh` does everything twice** for `--features fono/interactive`
  (`tests/check.sh:106`, `:155-156`, `:180`) — but `interactive` is already in
  `default` (`crates/fono/Cargo.toml:36`), so the second pass is bit-identical
  work.
- **No fast linker, no nextest, no sccache** configured anywhere.
- Fixture data is **not** a cost: the 3,044 calibration JSONs under
  `docs/bench/calibration/` are read only by `scripts/`, never by a test.

### Do we need them all?

Almost all 2,017 tests are cheap and worth keeping — the problem is gate
topology, not test count. The specific dead weight:

- `tests/check.sh --slim` is **broken**: it passes `--features tray,cloud-all`
  (`tests/check.sh:94`, `:98`, `:145`, `:149`, `:170`, `:173-174`) and
  `cloud-all` no longer exists in `crates/fono/Cargo.toml:22-144`.
- The duplicate `interactive` pass — pure waste.
- Three real-sleep tests burning ~5.6 s every run:
  `crates/fono-stt/src/groq_streaming.rs:687-734`, `:736-780`, `:788-843`.
- `crates/fono-net/tests/discovery_round_trip.rs:55-72` can burn 5 s and pass
  vacuously when multicast is blocked.
- Five integration files silently compile to zero tests when their feature is
  off; `crates/fono-net/tests/llm_server_round_trip.rs:14` gates 13 tests on
  `llm-server`, which `crates/fono/Cargo.toml:171` does not enable.
- `AGENTS.md:165-166` claims CI runs doctests. It does not — and there are none.

## Implementation Plan

### Phase 1 — free wins, no coverage change

- [ ] Task 1. Add a dev/test profile to the root `Cargo.toml` beside
      `[profile.release]` (`Cargo.toml:283`) setting `debug = "line-tables-only"`
      for the workspace and `debug = 0` for `[profile.dev.package."*"]`.
      Rationale: ~46 links currently copy full DWARF out of every rlib and out
      of the `RelWithDebInfo` whisper archives; line tables keep usable
      backtraces at a fraction of the link cost. Expected the largest single
      saving on the local loop.
- [ ] Task 2. Delete the three `--features fono/interactive` passes from
      `tests/check.sh:106`, `:155-156` and `:180`. Rationale: `interactive` is
      a default feature, so these re-run build, clippy and the entire suite for
      zero additional coverage. Roughly halves default `check.sh` runtime.
- [ ] Task 3. Remove the broken `--slim` mode (`tests/check.sh:45-48` and its
      six call sites) or re-point it at a feature set that still exists.
      Rationale: it cannot succeed today; a mode that always fails is worse
      than no mode.
- [ ] Task 4. Drop `cargo build --workspace --all-targets`
      (`tests/check.sh:102`) as a separate step. Rationale: the later
      `cargo test --workspace --all-targets` (`:177`) builds a superset; the
      build step only front-loads errors that arrive seconds later anyway.

### Phase 2 — link-time attack

- [ ] Task 5. Add a dev-only fast-linker configuration for
      `x86_64-unknown-linux-gnu` (clang + `-fuse-ld=mold`, keeping the
      multiple-definition allowance that the dual-ggml link needs per
      `.cargo/config.toml:5-13`). Rationale: GNU `ld` is doing garbage
      collection over a 50 MiB archive plus two ggml copies, ~46 times per run.
      Compounds multiplicatively with Task 1.
- [ ] Task 6. Scope `--gc-sections` and `--exclude-libs,ALL`
      (`.cargo/config.toml:44-62`) to release profiles only. Rationale: their
      documented justification is exclusively the shipped artefact's NEEDED
      allowlist and size budget; paying for them in debug links buys nothing.
- [ ] Task 7. Standardise the routine loop on `--tests --lib` rather than
      `--all-targets` (`tests/check.sh:177`), matching what `AGENTS.md:164` and
      `ci.yml:464` already prescribe. Rationale: drops 13 of 48 links —
      the `fono-overlay` winit/softbuffer examples, the criterion bench, the
      two bin-test binaries — none of which assert anything. Keep
      `--all-targets` in CI so example/bench rot is still caught.

### Phase 3 — reconcile the three gate definitions

- [ ] Task 8. Pick one canonical local gate and have `AGENTS.md:153-171`,
      `CONTRIBUTING.md:54-64` and `tests/check.sh` all express it identically.
      Rationale: three near-identical command triples have already drifted
      (`--tests --lib` versus `--all-targets`); drift means one of them is
      wrong at any moment.
- [ ] Task 9. Correct the doctest claim in `AGENTS.md:165-166` — no gate runs
      doctests and no executable doctests exist.
- [ ] Task 10. Verify whether `crates/fono-net/tests/llm_server_round_trip.rs`
      (13 tests) actually executes under a bare `cargo test --workspace`, and
      make the feature-gated integration files fail loudly rather than
      vanish when their feature is off. Rationale: silent zero-test compilation
      is coverage we believe we have and do not.

### Phase 4 — reclaim seconds inside the suite

- [ ] Task 11. Convert the three `groq_streaming` cadence tests
      (`crates/fono-stt/src/groq_streaming.rs:687`, `:736`, `:788`) to
      `#[tokio::test(start_paused = true)]` with explicit clock advance.
      Rationale: ~5.6 s of real wall-clock sleep per run, in every CI job and
      every local commit, guarding an `Instant`-based cadence check that
      tokio's paused clock controls just as well.
- [ ] Task 12. Give `crates/fono-net/tests/discovery_round_trip.rs:55` a much
      shorter deadline and make the multicast-unavailable path an explicit
      skip. Rationale: today it can spend 5 s and then pass without asserting.
- [ ] Task 13. Serialise the `PATH`-mutating test at
      `crates/fono-core/src/notify.rs:154-170`. Rationale: its safety comment
      asserts serial execution that the test harness does not provide.

### Phase 5 — CI scheduling

- [ ] Task 14. Fold the two `#[ignore]` latency smoke tests
      (`ci.yml:105`) into the existing debug test run, or move them to a
      nightly schedule. Rationale: they currently justify an entire extra
      `--release` build — including both C++ cmake trees — for two
      `FakeStt` timing assertions.
- [ ] Task 15. Scope the tiny.en equivalence gate (`ci.yml:112-161`) by path
      (`crates/fono-stt/**`, `tests/fixtures/equivalence/**`) or move it to a
      schedule. Rationale: a cached 76 MiB model, a fourth distinct build, and
      real inference on every PR, guarding behaviour that changes rarely.
- [ ] Task 16. Move the `gpu` size-budget row (`ci.yml:235-242`) to
      schedule/tag only. Rationale: LunarG setup plus 150+ shader compiles,
      25–40 min per PR, to check a deliberately loose 72 MiB ceiling that
      `release.yml` re-checks at tag time anyway.
- [ ] Task 17. Add `paths-ignore` to `container.yml:3-9` mirroring
      `ci.yml:7-13`. Rationale: doc-only pushes to main currently trigger two
      full Vulkan container builds that `ci.yml` deliberately skips.
- [ ] Task 18. Reconsider running the non-blocking `windows` job
      (`ci.yml:586`, up to 60 min including a Vulkan SDK install) on every PR
      rather than on a schedule. Rationale: it is explicitly documented as
      progress-surfacing, not a gate.

### Phase 6 — optional tooling

- [ ] Task 19. Evaluate `cargo-nextest` for the run half only. Rationale: it
      does not touch the 48 links, but it parallelises per-test processes and
      would let the llama live tests declare serialisation instead of relying
      on the `--test-threads=1` advice in comments.
- [ ] Task 20. Consider moving `docs/bench/calibration/runs/` out of the repo.
      Rationale: zero test-time cost, but ~3,044 files slow checkout, `git
      status` and every ripgrep. Hygiene, not speed — lowest priority.

## Verification Criteria

- `cargo test --workspace --tests --lib` from a warm `target/` completes in
  measurably less wall time than the current baseline, with the same pass count.
- `./tests/check.sh` runs each phase exactly once and no longer references a
  non-existent feature.
- The test count reported by `cargo test --workspace -- --list` before and after
  Phases 1–3 is identical or higher (Task 10 may reveal tests that were silently
  absent).
- CI `test` job duration drops, and the `size-budget`/`container` jobs no longer
  trigger on doc-only changes.
- `AGENTS.md`, `CONTRIBUTING.md` and `tests/check.sh` describe the same gate.

## Potential Risks and Mitigations

1. **Reducing debuginfo degrades backtrace quality during debugging.**
   Mitigation: use `"line-tables-only"` for workspace crates rather than `0`;
   keep dependencies at `debug = 0` where symbols are rarely needed.
2. **mold interacts badly with the dual-ggml link that requires
   `--allow-multiple-definition`.** Mitigation: scope the linker change to dev
   profiles only, keep release links on the current toolchain, and verify with
   a full `cargo test` before adopting.
3. **Scoping the equivalence and gpu gates lets a regression reach a tag.**
   Mitigation: keep both as blocking on the release workflow and add a nightly
   schedule so the window is at most one day.
4. **Paused-clock conversion changes what the cadence tests actually prove.**
   Mitigation: assert the same observable outputs and verify each converted
   test still fails when the cadence guard is deliberately broken.
5. **Removing `--all-targets` from the local loop lets example/bench rot in.**
   Mitigation: CI keeps `--all-targets`, so rot surfaces within one push.

## Alternative Approaches

1. **Do only Phase 1.** Four small edits, no new tooling, no CI change; likely
   recovers the majority of the local-loop time. Lowest risk, and the right
   first move if appetite is limited.
2. **Split the workspace feature graph so `fono-core` does not pull
   `llama-local` by default.** This attacks the root cause — 46 of 48 binaries
   linking llama.cpp — rather than the symptom, but it is a much larger
   refactor with real risk to the shipped feature matrix.
3. **Adopt `cargo-nextest` as the primary runner.** Better ergonomics and
   per-test timing data, but it leaves the compile/link half untouched, which
   is where the time is; useful mainly as measurement instrumentation.
