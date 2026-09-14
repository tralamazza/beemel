# Plan: resolve `opt`/`llc` to LLVM 18 in the build/check path

> Status: implemented. Superproject commit `72fcef6`. Added `llvm_tool()`
> in `bml/src/main.rs` (mirrors `find_llvm_config` in `bml-core/build.rs`)
> and routed all `opt`/`llc` invocations through it. Resolution order:
> `BML_LLVM_BIN` -> known LLVM 18 dirs -> PATH `<tool>-18` -> bare name.
> Full suite green: 653 tests.rs + 63 exec tests pass at the default `-Os`
> (were 39 red). Unit test pins the `BML_LLVM_BIN` override.

## Goal
`bml build` and `bml check` must use the LLVM 18 `opt`/`llc` the project targets,
so the default `-Os` (size) pipeline works. Today they use whatever `opt`/`llc`
is on PATH; on this machine that is LLVM 23, which removed `-Os`, breaking 39
build/check tests with:
`LLVM ERROR: The optimization level "Os" is no longer supported. Use O2 in
conjunction with the optsize attribute instead.`

## Root cause
`bml/src/main.rs` invokes `Command::new("opt")` / `Command::new("llc")`
directly (PATH). The project is an LLVM-18 project: the IKOS fork is LLVM 18 and
the verify path already resolves LLVM 18 via `find_llvm_config()`
(bml-core/build.rs), which checks `BML_LLVM_CONFIG`, then
`/opt/homebrew/opt/llvm@18`, `/usr/local/opt/llvm@18`, `/usr/lib/llvm-18`,
then `llvm-config-18` on PATH. The build path never got the same treatment.

## Fix
Add a runtime resolver in `main.rs` mirroring `find_llvm_config()`, returning the
bin directory for `opt`/`llc`:

1. `BML_LLVM_BIN` env var (explicit override) -> that dir.
2. Known LLVM 18 dirs: `/opt/homebrew/opt/llvm@18/bin`,
   `/usr/local/opt/llvm@18/bin`, `/usr/lib/llvm-18/bin` (first that has `opt`).
3. A PATH dir containing `opt-18`/`llc-18` (Debian/Ubuntu versioned names).
4. Fallback: bare `opt`/`llc` on PATH.

Use the resolved paths for all `opt`/`llc` invocations (the three codegen modes:
no-opt, file mode, pipe mode). `--Os` then works because LLVM 18 supports it.

## Test
- The 39 currently-red build/check tests go green (default `--opt=s` now runs
  against LLVM 18).
- `BML_LLVM_BIN` override is honored (spot check).

## Verification
1. `cargo test -p beemel` -- build/check tests pass.
2. `bml build` a fixture with `--save-temps`; confirm the `.opt.ll` is produced
   by LLVM 18 (no `-Os` error).

## Risk / tradeoffs
- Behavior change: builds now prefer LLVM 18 over PATH's newer LLVM. This is
  intended (reproducible codegen matching the verify toolchain), but it is a
  change for anyone who deliberately built with a newer LLVM. Mitigation:
  `BML_LLVM_BIN` override + PATH fallback.
- LLVM-23-only machines (no 18 anywhere) still hit `-Os`. Out of scope: the
  project requires LLVM 18 for verify anyway. If needed later, add version-aware
  `-Os` -> `-O2`+`optsize` translation as a follow-up.
- Hardcoded Homebrew/Linuxbrew paths mirror the existing `find_llvm_config()`
  convention rather than inventing a new one.
