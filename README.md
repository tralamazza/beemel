# beemel

`beemel` is the compiler for **BML**, a language for bare-metal ARM Cortex-M
firmware.

> **Status: alpha; exploratory.** Syntax, semantics, and the CLI change without
> notice. There is no stability guarantee, and the implementation has not been
> audited. Not intended for production use.

## Getting started

Requires a Rust toolchain and an LLVM install providing `opt`, `llc`, and
`ld.lld` (`brew install llvm`, `apt install llvm lld`, `dnf install llvm lld`).

```bash
git clone https://github.com/tralamazza/beemel
cd beemel
cargo install --path bml      # installs `bml` (check, build)
```

`cargo install bml` (without `--path`) installs an unrelated crate of the same
name; do not use it.

[Tutorial 01](doc/tutorials/01-getting-started.md) covers install through a first
program.

## Goal

BML is an attempt at a safe and simple language for embedded development. It
targets a class of bug rooted in *where* and *when* data is accessed: a DMA engine
and the CPU touching the same buffer, an ISR and a thread racing over a shared
flag, a register written from the wrong place, an index past the end of a buffer.

The approach: the program states the hardware facts — where each byte lives (MMIO,
DMA RAM, ISR-shared RAM, exclusive RAM) and which interrupt priority may touch it
— and the compiler enforces the access rules that follow and infers the rest from
usage. These are compile-time checks plus a test suite, with optional static
verification. They are not guarantees.

## Example

A blinking LED on an STM32F103 ("Blue Pill"):

```bml
import stm32f103.rcc;
import stm32f103.gpioc;

fn main() @context(thread) {
    RCC.APB2ENR.IOPCEN = 1;          // enable the GPIO-C clock
    GPIOC.CRH.MODE8 = 2;             // PC8: 2 MHz push-pull output
    GPIOC.CRH.CNF8 = 0;

    loop {
        GPIOC.ODR.ODR8 = !GPIOC.ODR.ODR8;   // toggle the LED
        // crude delay; the empty asm {} keeps the optimizer from deleting the loop
        var i: u32 = 0;
        while i < 500_000 { asm {} i += 1; }
    }
}
```

`stm32f103.rcc` / `stm32f103.gpioc` are ready-made chip definitions shipped in
[lib/](lib) (one of several MCUs). They start life generated from the vendor's
CMSIS-SVD with [bml-svd](https://github.com/tralamazza/bml-svd), then are curated
against the datasheet -- the source of truth.

## Status

- Pipeline: source → type and ownership checking → LLVM IR → `opt`/`llc`/`ld.lld`
  → ELF, with the linker script generated from a `.target` file.
- Examples target the STM32F103 Blue Pill, micro:bit v1 (nRF51), RP2350 (Pico 2
  W), and a NUCLEO-H723ZG PTP demo. CPU behavior is also run under QEMU.
- The regions/agents DMA model was checked against H723 Ethernet DMA on hardware.
- `bml-lsp`: diagnostics, hover, completion, go-to-definition.
- Ten tutorials and a language specification.
- Tests: unit, end-to-end QEMU execution, a no-panic fuzzer.

## Limitations

- Alpha: syntax and semantics change without notice; no semver, no migration
  notes.
- Cortex-M only (32-bit ARM Thumb). Other architectures are listed in
  [doc/future.md](doc/future.md).
- No language standard library and no package manager; modules are files. (A
  library of reusable *chip* definitions — per-MCU physics targets and
  peripherals — does ship in [lib/](lib).)
- Not audited and not proven correct; the compiler can have bugs.

### Verification (optional)

`bml verify` also needs the IKOS fork (the `ikos` submodule) and its native
dependencies. Build `bml` with the `ikos-static` feature, which links IKOS in and
runs it in-process:

```bash
git submodule update --init ikos
# native deps, e.g. macOS: brew install llvm@18 cmake boost gmp tbb
cargo install --path bml --features ikos-static --force
```

The first build compiles IKOS from the submodule (a one-time C++ build via cmake).
`bml build` and `bml check` do not need this. See
[doc/ikos-setup.md](doc/ikos-setup.md).

## Repository layout

| Path | Contents |
|---|---|
| [bml-core](bml-core) | Compiler library: lexer, parser, resolver, type checker, ownership checker, IR emitter |
| [bml](bml) | CLI: `bml check` / `build` / `verify` / `cflags` |
| [bml-lsp](bml-lsp) | Language server |
| [doc](doc) | Specification, tutorials, design notes |
| [lib](lib) | Shipped chip library: per-MCU physics targets + peripherals, and shared Cortex-M core peripherals |
| [bml/examples](bml/examples) | Per-board example projects |
| `ikos` | Submodule: LLVM-18 IKOS fork, used by `bml verify` |

Related repositories: [bml-svd](https://github.com/tralamazza/bml-svd) (CMSIS-SVD
to BML peripherals) and [bml-zed](https://github.com/tralamazza/bml-zed) (Zed
editor support).

## Documentation

- [Tutorials](doc/tutorials)
- [Language specification](doc/language.md)
- [Regions and agents](doc/regions-agents.md) — memory model, DMA, multi-core
- [Target library](lib/README.md) — shipped chip targets and peripherals, and how `bml` finds them
- [Verification](doc/verify.md)
- [C interop](doc/c-interop.md), [STM32/CMSIS workflow](doc/stm32-cmsis.md),
  [Design decisions](doc/design-decisions.md), [Hacking](doc/hacking.md),
  [Future ideas](doc/future.md)

## How this was built

BML was vibe-coded: developed iteratively with AI assistance rather than from an
up-front specification, with no separate design review. Changes are gated by the
test suite (which runs programs end-to-end under QEMU); hardware-facing parts are
checked on the example boards. The documentation was written from that process;
the history is in git.

Issues and suggestions are welcome.

## License

BML (the `bml-core`, `beemel`, and `bml-lsp` crates; the `beemel` crate ships the
`bml` binary) is [Apache-2.0](LICENSE).
`bml check` and `bml build` invoke `opt`, `llc`, and `ld.lld` as external
processes and link no third-party code beyond their Rust dependencies.

The optional `ikos-static` build (for `bml verify`) embeds third-party libraries,
each under its own license:

| Component | License | Linkage |
|---|---|---|
| [IKOS fork](https://github.com/tralamazza/ikos) | NOSA 1.3 | static |
| LLVM 18 | Apache-2.0 with LLVM exceptions | static |
| Boost | BSL-1.0 | static |
| TBB | Apache-2.0 | static |
| GMP | LGPL | dynamic |
| SQLite (via rusqlite) | public domain | bundled |

APRON/PPL (GPL) is excluded, so the `apron-*` verification domains are
unavailable in this build. A distributed `ikos-static` binary must carry the
IKOS (NOSA 1.3) notices.
