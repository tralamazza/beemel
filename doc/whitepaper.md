# BML: Whole-System Memory Correctness for Bare-Metal Firmware

*A whitepaper on BML and its compiler, beemel.*

> Status: alpha, exploratory. This document describes a working but unstable
> research compiler. The checks it performs are compile-time analyses plus a
> test suite, with optional static verification. They are not proofs and the
> implementation has not been audited.

## Summary

A compiler for an MCU normally models one bus master: the CPU. But memory
correctness on a modern microcontroller is a property of the whole system --
CPU cores, DMA engines, caches, and the bus matrix that connects them. The
bugs that follow from that gap are silent: a buffer placed in memory the DMA
engine cannot reach, a cache line the DMA never sees, an ISR and a thread
racing over a shared flag, a descriptor armed before the CPU is done writing
it. They compile, link, and produce a board that wedges.

BML models every bus master as a first-class **agent** with its own *reach*
(which memory it can address over the interconnect), its own *cache view*, and
its own *permissions*. It groups shared memory into **regions** and checks, at
compile time, that the agents sharing a region agree about it. Its distinctive
claim is a unification: the CPU priority-ceiling protocol (mutual exclusion
between ISRs and threads) and the asynchronous DMA release/reclaim handshake
are **one concept -- region ownership** -- with the synchronization mechanism
*derived from the set of sharers* rather than declared. The model was built
failure-first against real silicon (NUCLEO-H723ZG, Pico 2 W, micro:bit v1) and
validated on all three.

## 1. The problem: the compiler only sees the CPU

C's `volatile` conflates four distinct concerns -- hardware register access,
ISR-shared data, atomicity, and optimizer barriers -- into one type qualifier
you can forget, cast away, or apply inconsistently. BML removes `volatile`
entirely: volatility is a property of *where a thing lives*, not how you access
it, so the compiler always knows and never forgets.

But that is the shallow problem. The deeper one surfaced on a bench. The first
Ethernet driver for an STM32H723 produced these failures, all real:

- **Unreachable placement.** Moving the TX descriptor ring to DTCM compiled and
  linked, and produced a board that never transmitted -- nothing in the
  toolchain knew the Ethernet DMA cannot address DTCM over the bus matrix.
- **Unchecked cache discipline.** `dmb`-only synchronization was sound only
  because the D-cache happened to be off; enabling it would have broken RX
  silently, with no diagnostic.
- **Hand-written address encoding.** Descriptor index arithmetic and bit-shift
  register encodings that were undetectable when wrong.
- **A posted-write hazard.** A DMA tail-pointer write left in flight while the
  bus stayed busy was an intermittent imprecise BusFault source.

The common shape: each is a disagreement between two bus masters about memory --
reachability, visibility, ordering, or encoding -- and a CPU-only compiler has
no vocabulary to even state the disagreement, let alone check it.

## 2. Agents

**An agent is anything that touches memory on its own initiative.** The test is
three questions, all of which must hold:

1. Does it initiate memory accesses itself (not just decode them)?
2. Does it act concurrently, on its own clock, once set up?
3. Does it have its own view of memory -- reach, caching, permissions?

| kind       | example                          | who answers for its accesses          |
|------------|----------------------------------|---------------------------------------|
| `cpu`      | Cortex-M core, RP2350 core1      | the compiler -- it emits every access |
| `dma`      | Ethernet DMA, MDMA, nRF EasyDMA  | the owning module, at handoff sites   |
| `debug`    | SWD probe                        | host-side; inert to concurrency       |
| `external` | other-vendor firmware            | nobody -- channels only               |

A `cpu` agent's accesses are fully visible in the compiler's IR, so its rules
are per-access checks. A `dma` agent's accesses are invisible -- the compiler
sees only the address flowing into a register. **Handoffs** recover, for opaque
agents, the visibility the compiler gets for free on the CPU.

The schema was falsified across three DMA architectures -- ST's central
crossbar (H723), Raspberry Pi's per-channel controller (RP2350), and Nordic's
per-peripheral fixed-block engines (nRF51) -- and adding them required no new
questions of the model.

## 3. Regions: a three-layer model

Correctness is declared and checked in three layers, separated by who owns the
truth.

### Layer 1 -- physics (the target file)

The chip's immutable facts: memory blocks, agents, and the bus matrix. Written
once per chip, shipped with the compiler, verbose by design.

```ini
arch = armv7em
cpu  = cortex-m7

[mem.dma_pool]
base = 0x30007000
size = 4K
cacheable = false          # enforced via a generated MPU region

[agent.mdma]
kind        = dma
reach       = dma_pool, dtcm                       # cross-checked against bus
bus         = axi: 0x08000000..0x08100000, ahbs: 0x00000000..0x00040000
enabled_by  = RCC.C1_AHB3ENR.MDMAEN

[agent.mdma.ch0]
completes_by = MDMA.MDMA_C0ISR.CTCIF0              # the "done" signal
extent       = MDMA.MDMA_C0BNDTR.BNDT              # the transfer-count field
handoff      = MDMA.MDMA_C0SAR port_by MDMA.MDMA_C0TBR.SBUS ahbs

[region.dma_shared]
mem    = dma_pool
agents = mdma                                      # the CPU is always implicit
```

No key takes a raw address: every register is a path resolved against the SVD
the program imports, so an accidental literal fails at resolution rather than
silently configuring hardware. The agent contract is a closed schema -- each key
answers one of six questions about a bus master (may it run / where can it touch
/ which buffer / how much / when done / what code).

### Layer 2 -- policy (regions)

A region names a memory block and the agents that share it. From that, the
compiler *derives* obligations:

- **Reach check.** The region's memory must lie within every listed agent's
  reach. The DTCM footgun dies at target load, before any source compiles.
- **Cache discipline.** A cacheable block shared by a cached CPU and a
  non-snooping DMA agent is rejected; declaring it `cacheable = false`
  *generates* a non-cacheable MPU region in the reset handler (PMSAv7 on
  M4/M7, PMSAv8 on M33). The H723 runs with D-cache on and the generated MPU
  keeping Ethernet and MDMA coherent.
- **Alignment as derived physics.** A static placed in a cacheable,
  non-coherently-shared region is floored to the CPU's cache-line alignment --
  the alignment was never a property of the DMA engine, always of the cache.

### Layer 3 -- software binding (source)

- **`owns P` / `owns P.R`** -- module-exclusive register access, at register
  granularity (three modules can legitimately share one peripheral).
- **`in <region>`** -- placement as a checked claim; membership becomes a fact
  the handoff checker uses.
- **Derived-Move.** An array placed in a region a DMA agent mutates is wrapped
  in a move-typed carrier at resolution -- index *reads* are restricted (the
  value may be stale) while fill-before-release index *writes* stay legal. The
  restriction comes from *placement*, not an annotation.
- **`addr in R` fields** -- in-memory handoffs (descriptor pointers), checked
  for transitive reach: a descriptor delivered to an agent must not point into
  a region that agent cannot reach.

**Design principle: usage dictates declaration.** Only declare what usage cannot
express -- *physics* (the bus matrix) and *exclusivity* (`owns`, a claim about
other modules' absence). Everything else is derived: ceilings from accessor
sets, move-typing from placement, ISR cores from where their enable is written.
Optional clauses act as pins that must agree with usage, never as the source of
truth.

## 4. The unification thesis

This is the conceptual core. Two mechanisms that the literature treats as
unrelated are, in BML, the same concept seen from two sides.

- A **CPU** sharer is excluded from a region by raising priority -- the **ceiling
  protocol**. `claim X { ... }` opens a masked window (BASEPRI raised to the
  static's derived ceiling on ARMv7-M so unrelated higher-priority ISRs keep
  running; `cpsid`/`cpsie` on ARMv6-M; a hardware spinlock added cross-core).
- A **DMA** sharer is excluded by an **asynchronous handshake**. **Release** is
  the handoff register write; **reclaim** is `reclaim(BUF)`, which yields a
  bounds-checked view and is legal only once the channel's completion flag has
  been observed.

Both are **region ownership**. The compiler picks the transfer mechanism from
*who shares the region*: a masked critical section for a CPU co-sharer, a
release/reclaim handshake for a DMA co-sharer, both composed when both are
present. The ceiling is computed from the accessor set; the completion guard is
proven by lexical containment of the reclaim under the flag observation. Neither
is something the programmer declares -- they fall out of placement and use.

The practical payoff is that the dangerous cases become unrepresentable. Under
`@shared in R`, the move carrier nests inside the ceiling carrier, so *every*
access -- reads included -- is blocked outside a `claim`, and the masked window
is required by construction:

```
claim X {                 // masked: the CPU co-sharer is excluded here
    if done() {           // the DMA co-sharer's completion handshake
        reclaim(X)        // now, and only now, a view over X is sound
    }
}
```

## 5. Optional verification

`bml verify` lowers the program to LLVM IR and runs IKOS (NASA's
abstract-interpretation analyzer) for buffer overflows, null dereferences,
division by zero, integer overflow, and user `assert`s. The regions model adds
its own obligations on top: handoff provenance and reach, `addr in R` stores,
and transfer extents (`count * unit <= capacity`).

Making these obligations discharge in a non-relational interval domain forced a
set of concrete encodings, each an empirical finding rather than a derivation:
the provenance `assume` must sit at the address-of site (two integer casts of
one global do not unify across functions); capacity is encoded as a constant
against a count interval, because a base/limit shadow *pair* is unprovable in a
non-relational domain; alignment is carried by exact addresses (the compiler
owns the linker script) because the domain does no backward congruence
narrowing. Inside a `claim`, reads of the claimed static are *not* invalidated
-- the mask provides exactly the stability that makes an in-window
write-then-read-back provable, while the same sequence outside a window
correctly is not.

## 6. Validation on silicon

The model is failure-driven: no check exists speculatively. Each was added when
hardware demonstrated the hole, and the result was checked back on the board.

- **NUCLEO-H723ZG (Cortex-M7).** Ethernet TX/RX with typed descriptors and
  D-cache **on** under the generated MPU; MDMA copy to DTCM through the
  software-selected bus port; the full claim/reclaim composition holding an
  exact cross-context invariant; descriptor extents proven and confirmed by the
  DMA write-back.
- **Pico 2 W (dual Cortex-M33).** First-flash boot via the Arm IMAGE_DEF block;
  PMSAv8 MPU read back exactly as emitted; core1 launch handshake; cross-core
  `claim` with a hardware spinlock surviving tens of millions of contended
  windows with zero in-window invariant violations.
- **micro:bit v1 (nRF51, Cortex-M0).** AES-ECB known-answer test through the
  full chain -- handoff, fixed-size extent, guarded reclaim -- on first flash;
  ARMv6-M word-composed interrupt-priority programming live.

The sharpest single result concerns volatility. A plain load of a DMA
descriptor's OWN bit was hoisted by the optimizer out of a spin loop into an
infinite `b .`, freezing the Ethernet controller. BML lowers every access
through an agent-derived pointer as `volatile` (seeded syntactically at the
address-of site, with an escape check, E620, so the taint cannot be silently
lost across a call). Under the old lowering the spin froze at frame 5; under the
derived-volatile lowering it runs indefinitely. The bug and its fix were both
falsified on the board.

## 7. Related work, and what is new

The pieces exist in isolation; the combination does not.

- **CPU mutual exclusion.** The priority-ceiling / Stack Resource Policy is the
  CPU-side half, and it is not new -- Baker's SRP (1991), Ada's Ravenscar
  `Ceiling_Locking`, and RTIC for embedded Rust all implement it. None extends
  the protocol to a non-CPU sharer or a completion handshake.
- **DMA-as-principal with a memory view.** The closest prior art is the ETH
  Zurich "decoding net" / least-privilege model (Achermann et al., 2019), which
  treats DMA engines and cores as first-class subjects in a network of address
  spaces and even pre-computes fixed-topology translations at compile time. But
  it is an *OS memory-management model* enforced by a kernel reference monitor
  via runtime capabilities (Barrelfish, with a Linux sketch); its reach handling
  *enables* access by finding a translation path, assuming reconfigurable
  MMU/IOMMU hardware. BML's reach handling *rejects* a program on a fixed MCU
  bus matrix where no translation hardware exists to route around the problem.
- **DMA buffer handoff.** Embedded Rust encodes it as affine move semantics
  (the Embedonomicon `Transfer` type): the buffer is consumed by value and
  returned only after a busy-wait on completion. This is a library idiom; it
  does not check transfer length against buffer capacity, and reachability is
  assumed, never modeled. Per-driver functional proofs (e.g. the Pancake
  verification of an i.MX Ethernet driver, 2025) verify one descriptor ring in
  Hoare logic, single-threaded, with no ceiling protocol.
- **Formal DMA isolation.** Haglund & Guanciale (FMCAD 2022) give a HOL4
  framework proving sufficient conditions under which a DMA controller confines
  its accesses to allowed regions -- a security-policy property checked by
  theorem proving for a driver or runtime monitor, modeling neither cache nor
  CPU-side concurrency.
- **The `volatile` critique.** Eide & Regehr (EMSOFT 2008) established that
  `volatile` is misused and miscompiled. BML's response -- volatility as a
  property of placement under a concurrent non-CPU writer, with escape analysis
  -- is a concrete language design in that line.

Notably, Singularity -- the canonical safe-language OS -- examined exactly this
interface and *excluded* it: "The only unsafe aspect of the driver-device
interface is DMA... DMA currently is inherently unsafe and... cannot be
encapsulated or virtualized by a system." Twenty years on, no source language
surveyed unifies the CPU-side ceiling protocol with the DMA-side handshake under
one region-ownership abstraction. That unification, checked at compile time in
the firmware source language with reach derived from the bus matrix and
synchronization derived from the sharer set, is the novel contribution.

## 8. Limitations and non-goals

Honesty here is load-bearing.

- **Checks, not proofs.** The compile-time analyses plus the test suite (which
  runs programs end-to-end under QEMU) plus optional IKOS verification reduce a
  class of bugs. They are not soundness guarantees, and the compiler is not
  audited or proven correct.
- **Cortex-M only.** 32-bit ARM Thumb. No other architecture today.
- **Known blind spots, recorded.** Among them: an address laundered through
  integer arithmetic defeats the syntactic volatile taint; system-exception
  priorities (SysTick/PendSV) are unmodeled; a flag cleared via a *separate*
  register (e.g. an STM32 IFCR) is invisible to the straight-line staleness
  check; facts carried across a loop back-edge are a lexical blind spot. Each is
  written down where it was found rather than papered over.
- **No standard library, no package manager.** Modules are files. A library of
  per-MCU *chip* definitions ships, but there is no application runtime.
- **Provenance.** BML was developed iteratively with AI assistance rather than
  from an up-front specification, gated by the test suite and hardware bring-up
  rather than a separate design review. The documentation was written from that
  process; the slice history is in git.

## 9. Status and availability

beemel compiles BML through type and ownership checking to LLVM IR, then to ELF
via `opt`/`llc`/`ld.lld` with a linker script generated from the target file.
It ships ten tutorials, a language specification, an LSP server, per-board
example projects, and a no-panic fuzzer. `bml verify` additionally links the
IKOS submodule. The compiler is Apache-2.0; the optional verification build
embeds third-party components under their own licenses.

The project is alpha and exploratory: syntax and semantics change without
notice, and nothing here is offered as production-ready. It is a position --
that whole-system memory correctness on an MCU is a checkable property, and that
the CPU and the DMA engine want the same ownership discipline -- worked out far
enough against real hardware to be worth writing down.
