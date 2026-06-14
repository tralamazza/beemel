# BML Tutorials

A hands-on path into BML. The [language specification](../language.md) is the
exhaustive reference; these tutorials are the on-ramp. Each one builds something
you can run, introduces a few concepts at a time, and links back to the spec for
the full rules.

## Who this is for

You write C or Rust (or both) and want to drive bare-metal ARM Cortex-M
hardware. We assume you know what a pointer is, what an interrupt is, and that
hardware is poked through memory-mapped registers. We do **not** assume you know
BML, ARM linker scripts, or the NVIC in detail -- those are explained as they
come up.

Throughout, two callouts orient you against what you already know:

> **From C:** how BML differs from the C idiom you'd reach for.
>
> **From Rust:** how BML's model relates to ownership, `Send`/`Sync`, slices.

BML is closer to C in surface syntax and closer to Rust in what it refuses to
compile. The interesting part is a third axis C and Rust mostly leave to the
programmer: *where data physically lives* (MMIO, DMA RAM, ISR-shared RAM,
exclusive RAM) and who is allowed to touch it from which interrupt priority.
That axis is what the tutorials keep coming back to.

## Conventions

- Every tutorial is hands-on: you build the code, and run it where the effect is
  observable. QEMU emulates the Cortex-M3 core and semihosting (great for the
  CPU-level tutorials) but **not** most peripherals -- so peripheral behavior is
  confirmed by reading the generated IR or on real hardware (tutorial 04), not by
  watching an emulated LED.
- Code blocks are complete unless marked `// ...`. Commands assume the compiler
  is on your `PATH` as `bml` (see tutorial 01).
- We use the STM32F103 "Blue Pill" / QEMU `stm32vldiscovery` as the running
  example because QEMU emulates it for free.

## The series

| # | Tutorial | What you learn |
|---|----------|----------------|
| 01 | [Getting Started](01-getting-started.md) | Install the toolchain, write a self-contained blinky, build it, watch it run in QEMU. The `check` and `build` commands. |
| 02 | [Values and Control Flow](02-values-and-control-flow.md) | `var`/`const`, the integer/float types, why there are **no** implicit conversions and how `as` works, `b1` vs `b8`, `if`/`while`/`loop`/`for`, `match` over ints and enums, block- and `if`-expressions, compound and wrapping assignment. |
| 03 | [Peripherals and MMIO](03-peripherals-and-mmio.md) | `peripheral`/`reg`/`field`, bit-fields and read-modify-write, read-only/write-only access, bit-band, and why BML has no `volatile` keyword. Rebuild blinky from first principles. |
| 04 | [Targets and Building](04-targets-and-building.md) | Anatomy of a `.target` file, the auto-generated linker script, optimization levels, `--save-temps`, `--out-dir`, flashing real hardware vs QEMU, `bml cflags`. |
| 05 | [Interrupts and Contexts](05-interrupts-and-contexts.md) | `@isr`/`@context`/`@naked`, the vector table, the call-graph rules (ISR cannot call thread, etc.), `@exclusive` ownership, `@shared` + the priority-ceiling protocol, and `claim` windows. |
| 06 | [Data: Structs, Enums, Modules](06-structs-enums-modules.md) | Structs with explicit layout and visible padding, `@repr(C)`/`@repr(packed)`, field endianness (`@be`/`@le`), enums, the Move/Copy rule, and the `import`/`export` module system. |
| 07 | [Pointers and Views](07-pointers-and-views.md) | `*T` vs `*mut T`, `&`/`&mut`, `null`, pointer arithmetic, function pointers; then `view`/`ring`/`bits` -- bounds-checked descriptors -- and how their Move/Copy behavior differs. |
| 08 | [Regions and Agents (DMA safety)](08-regions-and-agents.md) | The core differentiator: declaring what a DMA engine or second core may touch, `@dma`/`@external`, why reading agent-shared memory is blocked, and `reclaim` after a completion handshake. |
| 09 | [Verifying with `bml verify`](09-verifying.md) | The IKOS static analyzer: `assume`/`assert` vs `comptime_assert`, the integer-overflow contract and wrapping operators, proving view bounds across calls, and auditing suppressions. |
| 10 | [C Interop](10-c-interop.md) | `extern fn`, the ABI-safe subset BML checks at the boundary, passing structs and callbacks, and linking C objects (`--link`, `bml cflags`). |

All ten tutorials are written.

## See also

- [Language specification](../language.md) -- the complete reference.
- [Design decisions](../design-decisions.md) -- *why* the language is shaped this way.
- [Regions/agents model](../regions-agents.md) -- the full memory-safety model (tutorial 08 is the gentle version).
- [`bml verify`](../verify.md) -- the verifier in depth (tutorial 09 is the gentle version).
- [Example projects](../../bml/examples) -- blue-pill, micro:bit, rp2350, and a Nucleo-H723ZG PTP demo.
