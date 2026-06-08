# Regions and Agents Plan

## Goal

Make whole-system memory correctness on MCUs a checked property. The compiler
learns which hardware actors ("agents") touch memory, what each can reach, and
where software hands addresses to them ("handoffs"). Placement, reachability,
and address-encoding bugs become compile errors or discharged verification
obligations instead of silent board lockups.

Status 2026-06-08: design settled. Implemented: slice 0 (target physics:
`[mem.*]`/`[agent.*]`/`[region.*]` parsing + reach validation), slice 1
(`in <region>` placement: parser, IR section, mem-block-driven linker script,
E600/E601/E602 checks, QEMU exec proof), and slice 2 (`owns` parsing, path
resolution E603, cross-module exclusivity E604, handoff-ownership rule E605
with an exhaustive write walker), slice 3 (`word_addr` handoff encoding:
compiler-inserted `>> 2`, double-shift guard E606, QEMU exec proof), and slice
4 (verify obligations: provenance assume at `&X as u32`, reachability assert at
the handoff write, discharged by IKOS -- DTCM footgun caught at the value
level). Slice 5 partial: `doc/language.md` corrected; full `@dma` retirement
re-scoped (blocked on in-memory handoffs + a Move-typing replacement decision +
hardware validation). Next unblocker: design in-memory handoffs.

## Problem

All failure modes below are real, taken from
`bml/examples/nucleo-h723zg-ptp/eth_dma.bml` (the board currently on the
bench):

1. **Unreachable placement.** ETH DMA on the H723 cannot reach DTCM. Moving
   `TX_DESC` from D2 SRAM to DTCM compiles, links, and produces a board that
   never transmits. Nothing in the language knows about bus topology.
2. **Hand-written address encoding.** `Ethernet_DMA.DMACTxDLAR.TDESLA =
   desc0 >> 2` -- the SVD field starts at bit 2, so the value is a word
   address. Forgetting the shift, or shifting twice, is undetectable.
3. **Unchecked cache discipline.** The `dmb`-only synchronization in
   `eth_send_heartbeat` is sound only because D-cache is off for these
   buffers. Nothing checks that; enabling the cache later breaks RX silently.
4. **`@dma` over-promises.** `doc/language.md` claims no elision/caching, but
   codegen does not make `@dma` volatile. It is a placement hint pretending to
   be a contract.
5. **Address juggling.** The `tx_desc_addr(word_index)` arithmetic and
   `TX_DESC_INDEX` wraparound are exactly what IKOS flags today (V130, V101,
   V113 warnings on the example).

The common shape: memory correctness on an MCU is a property of the *system*
(CPU + DMA engines + caches + bus matrix), but the compiler only models the
CPU.

## The agent model

**Definition (plain English):** an agent is anything that touches memory on
its own initiative. Three-question test, all must hold:

1. Does it initiate memory accesses itself (not just decode them)?
2. Does it act concurrently, on its own clock, once set up?
3. Does it have its own view of memory -- reach, caching, permissions?

Every pair of agents is a potential disagreement about memory: visibility
(cache vs no cache), ordering (write buffers vs bus order), staleness (who
wrote last), permission (who may write at all). The compiler's job is to
referee these disagreements at compile time where possible and emit
verification obligations where not.

### Kinds

Kind is a treatment class -- it decides which machinery applies, not what the
silicon is called.

| kind       | example                | binding to software       | who answers for its accesses                |
|------------|------------------------|---------------------------|---------------------------------------------|
| `cpu`      | cm7                    | all modules (implicit)    | the compiler: it emits every load/store     |
| `dma`      | ETH DMA, MDMA, BDMA    | module owning handoffs    | that module, at handoff write sites         |
| `debug`    | SWD probe (AHB-AP)     | none (built-in)           | host-side test harness, via manifest        |
| `external` | other-vendor firmware  | none                      | nobody -- channels only                      |

The asymmetry that justifies handoffs: a `cpu` agent's accesses are fully
visible in the IR, so region rules are ordinary per-access checks. A `dma`
agent's accesses are invisible at compile time -- the compiler only sees the
address flowing into a register. Handoffs recover, for opaque agents, the
visibility the compiler gets for free on `cpu` agents.

### Non-agents

- **Contexts** (thread/ISR) are scheduling identities *within* a cpu agent,
  not agents: same reach, same cache, same view. Agent checks ask "can it see
  this memory correctly"; context checks ask "can these preempt each other
  mid-update". Orthogonal axes, existing `@context` machinery unchanged.
- **RP2350 PIO** executes code but has zero memory reach (DMA moves its FIFO
  data) -- fails question 3, not an agent. PIO stresses a different axis:
  compile-time resource allocation (state machines, instruction memory, DMA
  channels, pins). Out of scope here.
- **TrustZone security states** are attribute pairs on a cpu agent
  (cm33 secure / cm33 non-secure), not separate kinds.

### Reality check (counts)

STM32H723: ~12 agents. cm7, probe, and one per DMA controller (not per
stream/channel -- streams share the controller's reach and cache behavior):
MDMA (only DMA reaching TCM, via the CM7 AHBS port), DMA1, DMA2, BDMA (reach
limited to D3 SRAM4/backup SRAM), ETH DMA, SDMMC1, SDMMC2, OTG_HS, DMA2D,
LTDC (read-only master: `access = read`).

RP2350: ~5 agents, and the dominant checks invert -- no D-cache and a uniform
crossbar mean staleness mostly disappears; security attribution and resource
allocation dominate. The model has to fit both.

## Design principle: usage dictates declaration

Stated by the project owner and applied throughout: **only require a
declaration for what usage cannot express.** Two things qualify:

1. **Physics** -- the compiler cannot see the bus matrix; the target file
   declares it.
2. **Exclusivity** -- `owns` is a claim about *other* modules' code (an
   absence), which no amount of local usage can establish.

Everything else is derived from usage: who drives an agent (from handoff
register ownership), what a module uses (from its accesses), cache discipline
(from which agents share a region), eventually placement (from where
addresses flow). Optional clauses act as pins/assertions, never as the source
of truth. A consequence already settled: there is no `drives` clause and no
`uses` clause.

## Layer 1: physics (target file)

Extends the existing INI format of `stm32h723zg.target` with `[mem.*]` and
`[agent.*]` sections. `[mem.*]` generalizes today's `ram_base`/`ram_size`
pair (which the H723 file already bends -- it points `ram_base` at D2 SRAM as
a workaround for exactly the reachability problem this plan solves).

```ini
arch = armv7em
cpu = cortex-m7
caches = i, d            # makes the implicit cpu agent's cache behavior explicit

[mem.flash]
base = 0x08000000
size = 1M

[mem.itcm]
base = 0x00000000
size = 64K

[mem.dtcm]
base = 0x20000000
size = 128K

[mem.axi_sram]
base = 0x24000000
size = 320K

[mem.sram1]
base = 0x30000000
size = 16K

[mem.sram2]
base = 0x30004000
size = 16K

[mem.sram4]
base = 0x38000000
size = 16K

[agent.eth_dma]
kind = dma
reach = axi_sram, sram1, sram2       # NOT dtcm/itcm: ETH DMA cannot cross the CM7 core
cached = false
handoff = Ethernet_DMA.DMACTxDLAR.TDESLA : word_addr align 4
handoff = Ethernet_DMA.DMACRxDLAR.RDESLA : word_addr align 4
handoff = Ethernet_DMA.DMACTxDTPR.TDT : word_addr
handoff = Ethernet_DMA.DMACRxDTPR.RDT : word_addr
enabled_by = RCC.C1_AHB1ENR.ETH1MACEN, RCC.C1_AHB1ENR.ETH1TXEN, RCC.C1_AHB1ENR.ETH1RXEN

[agent.mdma]
kind = dma
reach = itcm, dtcm, axi_sram, sram1, sram2, sram4   # only DMA that reaches TCM
cached = false

[agent.bdma]
kind = dma
reach = sram4                        # the D3 prison
cached = false

[agent.ltdc]
kind = dma
reach = axi_sram, sram1, sram2
access = read                        # read-only bus master
cached = false

[agent.probe]
kind = debug
reach = *                            # AHB-AP sees everything
cached = false                       # bypasses D-cache -- matters for HIL
```

Rules:

- The cpu agent is implicit, derived from `cpu =` / `caches =`. Multi-core
  parts will need explicit `[core.*]` sections later; not designed here.
- Register paths (`handoff`, `enabled_by`) are resolved against the SVD
  modules imported by the program being compiled. Unresolvable path = error
  at build time. Raw addresses in the target file were rejected as unreadable
  and unreviewable.
- `handoff` encoding is a closed set: `byte_addr`, `word_addr` (value =
  address >> 2, because the SVD field starts at bit 2), `align N`. New
  encodings require a compiler change -- deliberately, since each one is a
  codegen rule.
- These sections are per-chip facts. They belong in the vendored target file
  and are written once per chip, ideally generated/audited from the reference
  manual's bus-matrix table.

## Layer 2: policy (regions)

A region names a slice of a `mem` block and lists the agents that share it.

```ini
[region.dma_shared]
mem = sram1
agents = eth_dma                     # cpu agent is always implicitly included
```

Checks and derivations:

- **Reach check:** the region's memory must lie within the reach of every
  listed agent. `[region.x] mem = dtcm, agents = eth_dma` is a target-file
  error -- the DTCM footgun dies here, before any source is compiled.
- **Cache discipline is derived, not declared** (usage dictates declaration):
  if a cached cpu agent shares a region with a non-snooping agent, the region
  must be non-cacheable. On the current bring-up (D-cache never enabled) this
  is vacuously satisfied; the check exists so that *enabling* D-cache later
  forces an MPU story instead of silently corrupting RX. MPU config
  generation from regions is future work, but the error fires from slice 1.
- `access = read` agents (LTDC) relax the rules: sharing with them constrains
  CPU-side ordering but cannot produce write-write races.

Open: whether regions live in the target file or a per-board file. Slice 1
puts them in the target file next to the agents they reference.

## Layer 3: software binding (source)

The module is the file, as today -- `import`/`export` already work that way.
Two additions, shown on the real example:

```bml
// eth_dma.bml
import svd.rcc;
import svd.ethernet_mac;
import svd.ethernet_dma;
import svd.ethernet_mtl;

owns Ethernet_DMA, Ethernet_MTL;                  // exclusive register access
owns Ethernet_MAC.MACCR, Ethernet_MAC.MACPFR,     // register granularity:
     Ethernet_MAC.MACA0LR, Ethernet_MAC.MACA0HR;  // MACMDIOAR belongs to phy_lan8742,
                                                  // the timestamp regs to ptp_clock

export fn eth_init_tx, fn eth_init_rx, fn eth_send_heartbeat, fn eth_poll_rx;

var TX_BUFFER: [u8; 128] @align(32) in dma_shared;
var TX_DESC:   [u32; 8]  @align(32) in dma_shared;
var RX_BUFFER: [u8; 1024] @align(32) in dma_shared;
var RX_DESC:   [u32; 8]  @align(32) in dma_shared;

var TX_DESC_INDEX: u32;     // no clause: default region, cpu-only
```

### `owns`

The only claim clause. Grants exclusive access to the listed registers;
another module touching them is an error. Granularity is register paths, with
the peripheral name as shorthand for all of it. Peripheral granularity alone
is a lie on real chips: in this example three modules legitimately share
`Ethernet_MAC` (this file: MACCR/MACA0*/MACPFR; `phy_lan8742.bml`:
MACMDIOAR/MACMDIODR; `ptp_clock.bml`: the timestamp registers).

Unowned registers stay free-for-all (RCC, GPIO), with one exception:

**Rule: handoff registers are ownership-required.** Writing a register that
appears in any agent's `handoff` list requires owning it. Without this rule
the exclusivity guarantee evaporates for exactly the registers that matter.

**Derived relation:** module M *drives* agent A iff M owns at least one of
A's handoff registers. "Drives" appears in diagnostics and tooling (LSP hover
on the `owns` line: "drives agent eth_dma, 4 handoff registers"), never as
syntax. An earlier draft had a `drives` clause; it was collapsed because the
target file's handoff map makes it derivable -- usage dictates declaration.
Consequence accepted: two modules may own disjoint handoff subsets of one
agent (TX-path / RX-path split); obligations are per-register, so this is
sound.

### `in <region>`

Placement as a checked claim. Replaces `@dma` for placement (see "`@dma`
fate"). The generated linker script places the symbol in the region's memory;
the symbol's region membership becomes a fact for handoff checking. `in` on a
region whose agents cannot all reach it is impossible by construction (the
region itself would have been rejected).

Slice 1 requires explicit `in` for anything whose address reaches a handoff.
Full inference (compiler derives placement constraints from address flows and
auto-places, `in` only pins) is the principled endpoint per
usage-dictates-declaration, but it changes link-time layout when code changes
and needs careful diagnostics; staged for later.

**Region memory is uninitialized at startup.** The `.region.*` output section
links as NOBITS (verified: the ELF has no PROGBITS for it) and is in neither
the `.data` copy nor the `.bss` clear that `reset_handler` runs -- so a static
placed `in <region>` is not zeroed and not loaded at boot. An initializer
would be silently dropped, so it is rejected (E601); the static must be written
at runtime before the agent uses it, which is how every agent-shared buffer is
set up anyway (descriptors and buffers are filled before the DMA engine is
enabled). Loadable/zeroed regions (e.g. a mailbox with an initial value) would
need region sections added to the startup copy/clear loops -- deferred until a
use case appears. A region placement also rejects a co-present `@section(...)`
(E602): both set the output section.

Syntax note: `in dma_shared` after the annotations. If the parser grows
ambiguities, fall back to `@region(dma_shared)`; `in` is preferred for
readability.

## Handoffs

**Definition:** a handoff is the place where a number stops being data and
becomes an address -- a register whose written value an agent will
dereference on its own initiative.

At every write to a handoff register, in the owning module, the compiler does
three things:

1. **Encoding.** The register knows its encoding, the programmer writes an
   address. `eth_dma.bml:174` changes from
   `Ethernet_DMA.DMACTxDLAR.TDESLA = desc0 >> 2;` to
   `Ethernet_DMA.DMACTxDLAR.TDESLA = tx_desc_addr(0);` -- the compiler
   inserts the `>> 2` for `word_addr`. The shift bug class is gone.
   (Lowering change: needs a QEMU exec fixture, not just IR-substring tests.)
2. **Static reach check.** If the value's provenance is statically known and
   the target is outside every region the agent can reach, `bml check`
   rejects it.
3. **Verification obligation.** Otherwise `bml verify` discharges it: the
   compiler emits `assume(range)` at the address-of site of the source symbol
   and `assert(in-region && aligned)` before the handoff write. See next
   section for why the assume goes there.

### In-memory handoffs (design)

Register handoffs do not cover the addresses an agent reads out of memory it
walks. In `eth_dma.bml`, the descriptors are `[u32; 8]` (two descriptors x four
words) and word 0 of each holds a buffer **byte address** the ETH DMA
dereferences:

```bml
RX_DESC[0] = rx_buffer_addr(0);   // RX descriptor 0, word 0 = buffer pointer
RX_DESC[4] = rx_buffer_addr(1);   // RX descriptor 1, word 0
TX_DESC[desc_word] = tx_buffer_addr();
```

Same shape on MDMA linked lists and RP2350 DMA control blocks -- needed on
every chip examined, so it is not optional. Today these are raw `u32` writes
the compiler cannot tell from control-word writes; the goal is to mark word 0
as an address slot and apply the handoff obligations to it.

**Surface: address-typed struct fields.** Replace the raw `[u32; 8]` with a
typed descriptor whose buffer-pointer field carries a region:

```bml
struct RxDesc @repr(packed) {
    buf1: addr in dma_shared,   // a byte address constrained to dma_shared
    _r1: u32,
    _r2: u32,
    flags: u32,
}
var RX_DESC: [RxDesc; 2] @align(32) in dma_shared;

// the in-memory handoff write:
RX_DESC[0].buf1 = rx_buffer_addr(0);
```

`addr in R` is a 4-byte field (layout-identical to `u32`, `@repr(packed)`-safe)
that means "a byte address that must lie in region `R`." It is *not* a typed
pointer: no pointee type, no deref; reading it yields a `u32`. A `word_addr in
R` variant (value = address >> 2) covers descriptor slots that store a word
address, mirroring the register handoff encodings; ETH buffer pointers are
byte addresses, so `addr` (no shift) is the common case.

**The write to an `addr in R` field carries the register-handoff actions**, and
reuses the slice-3/4 machinery:

1. *Encoding* -- `word_addr in R` inserts `>> 2` at the field store (the same
   `encode_word_addr_handoff` path); `addr in R` is verbatim.
2. *Static reach check* -- if the written value's provenance is a static placed
   in a region not contained in `R`, reject in `check`.
3. *Verify obligation* -- emit `assert(value in R.range)` at the field store.
   The provenance `assume` is already emitted by slice 4 at `&BUFFER as u32`
   (the buffer's region range), so this reuses `region_addr_ranges` and
   `emit_range_assert` unchanged. The same index juggling slice 4 catches on
   register handoffs (`base + i*512` with unbounded `i`) is caught here.

So in-memory handoffs are, mechanically, register handoffs whose "register" is
a struct field and whose target range is the field's own `in R` (rather than an
agent's reach). They are in one way *simpler*: the constraint region is
explicit on the field, no agent-reach lookup.

**Detection / lowering.** The struct-field store path in `ir.rs` already GEPs to
the field and stores; when the field type is `addr in R` / `word_addr in R`,
add the encode (build + verify) and the assert (verify). The checker needs the
new field type (`Type::Addr { region, word }` or similar) threaded through
`types.rs`/resolver/checker like other field types, with `sizeof == 4` and
packed layout.

**Open questions.**

- *Transitive reach.* `addr in R` constrains the value to `R`, but does not yet
  check that the agent which walks this descriptor can reach `R`. The link is
  the register handoff that delivers the descriptor base (`DMACRxDLAR =
  &RX_DESC`): the agent owning that handoff walks `RX_DESC`, so every `addr in
  R` field inside `RX_DESC` should satisfy `R.mem in agent.reach`. A
  target+type-level check can add this once descriptors are tied to agents
  through the delivering handoff; v1 constrains the value to `R` and leaves the
  agent-reaches-`R` check as a refinement.
- *`addr` as a general type.* v1 scopes it to struct fields (the descriptor
  case). Whether locals/params/returns may be `addr in R` (an address proven
  in-region flowing around) is deferrable; the helpers (`rx_buffer_addr`)
  currently return `u32` and the provenance flows through that fine.
- *Reading.* Reading an `addr in R` field yields `u32`. Whether a read should
  re-establish the `in R` fact (so a value loaded back from a descriptor is
  known in-region) is open; not needed for the write-obligation use case.
- *Move/aliasing.* This is also the natural home for whatever replaces `@dma`'s
  Move-typing once placement moves to `in <region>` (see slice 5) -- the
  descriptor struct, not a storage-class wrapper, would carry it.

**Why this is the next slice.** It unblocks the `eth_dma.bml` descriptor-struct
refactor (direct typed indexing, no `*u32` index-read workaround), it is the
prerequisite for retiring `@dma`, and it closes the last place an unchecked
address reaches an agent.

## Verification strategy (empirically validated)

Probed 2026-06-08 against the local IKOS fork (interval-congruence domain) on
instrumented copies of the real example; 6/6 boundary asserts at the
DMACTxDLAR/DMACRxDLAR writes proved in 0.56s wall. The rules below are
measured behavior, not assumptions:

- **Assume at the address-of site, not the call site.** Two `ptrtoint`s of
  the same global do not unify across functions; a range assumed on the
  caller's `&TX_DESC` does not constrain the callee's recomputation. Facts
  attached where the address is taken propagate through calls, returns, and
  arithmetic.
- **No backward congruence narrowing.** Even `assume(x % 32 == 0);
  assert(x % 32 == 0);` fails (separate urem SSA values). Alignment is
  unprovable symbolically. Exact-address assumes (`base == 0x30004000`) prove
  everything including modulo -- and the compiler generates the linker
  script, so it *has* exact addresses. Compiler-owned layout is therefore the
  alignment story; patching the IKOS fork with backward urem refinement is
  the fallback.
- **Severity:** definite contradiction = error (reject), unproven = warning
  (require annotation or restructure). Maps directly onto check/verify:
  statically decidable violations die in `check`; the rest become obligations
  with this severity split.

## What this fixes, on the bench

The five problems from the top, revisited:

1. DTCM placement of `TX_DESC`: target-file/region error, before compiling.
2. `>> 2`: compiler-inserted from the `word_addr` encoding.
3. dmb-only sync: cache-discipline derivation makes enabling D-cache without
   an MPU story an error instead of a latent RX corruption.
4. `@dma`: replaced by `in <region>` (below).
5. Address juggling: handoff obligations put IKOS exactly where the juggling
   happens; the descriptor-struct refactor (in-memory handoffs) removes most
   of the arithmetic.

## `@dma` fate

`@dma` shrinks to nothing: placement moves to `in <region>`, cache discipline
moves to the region derivation, and it never delivered volatility anyway.
Plan: port the example off it, then remove the storage class and correct
`doc/language.md` (which currently promises "no elision/caching" that codegen
does not implement). The index-read asymmetry workaround (`rx_desc_get32`
going through `*u32`) disappears with it.

## HIL hook (pointer, not designed here)

The probe is `kind = debug`: an AHB-AP bus master that bypasses the D-cache.
Two consequences land in this design: testbed-visible regions must be
non-cacheable (same derivation as DMA sharing), and the typed layout manifest
(`bml build --manifest`: name/address/type incl. `@be`) is how the host-side
harness addresses board state. The harness itself (mailbox transport,
`bml test --hil`) is a separate track.

## Open questions

- **In-memory handoff design**: `addr in <region>` field types; interaction
  with move semantics and views; what `&RX_BUFFER + desc_index * LEN`
  provenance looks like to IKOS through a struct store.
- **`owns` verbosity**: register-granularity lists get long. Possible
  `owns Ethernet_MAC except ...` or SVD-cluster granularity if real files
  hurt. Do nothing until they do.
- **Shared-peripheral hole**: RCC/GPIO stay unclaimed free-for-all.
  Field-level `owns` (RCC.C1_AHB1ENR.ETH1MACEN) would close it; deferred
  until the hole bites.
- **Placement inference**: when to flip `in` from required to pin
  (usage-dictates-declaration endpoint).
- **Regions in target vs board file**; multi-core `[core.*]`; TrustZone
  attribute pairs -- all deferred until a second target forces them.
- **`enabled_by` checking**: clock-gate-before-touch is a real bug class
  (the `[startup]` SRAM ungating exists for exactly this reason); whether to
  check handoff writes against `enabled_by` state is unresolved.

## Implementation plan

Code-grounded against the current tree. Each slice lands and is testable on
its own; ordering is by dependency, not by size.

### Anchors in the existing code

- **Placement already exists.** `@section "name"` lowers to LLVM
  `section "..."` at `ir.rs:356-379`; the linker script places sections.
  `in <region>` reuses this path -- a section-name convention plus
  MEMORY/SECTIONS entries in `target.rs::generate_linker_script` (`:242`). No
  new codegen concept.
- **The checker does not see `Target` today.**
  `Checker::check(&program, &symbols, &mut diags)` (`main.rs:391`, `:537`,
  `:751`) takes no target. Region/ownership checks need both, so they live in
  a new pass `bml-core/src/region.rs` taking `&Target`, threaded in at those
  three call sites.
- **Module-header clauses parse like `import`/`export`** -- a new arm in
  `parse_item` (`parser.rs:200`) and a new `Item` variant.
- **Register paths resolve against `symbols.peripherals`** -- the same
  address/bit-offset data codegen uses for field writes
  (`ir.rs:3325-3374`).
- **Handoff write hook = the peripheral-field store** at `ir.rs:3325`;
  encoding insertion and the assert go there. The assume goes at the
  address-of site (`ir.rs:3128-3180`).

### Slice 0 -- Target physics (no language change)

`target.rs` only. Extend the section-dispatch loop (`:78-127`, template:
`[startup]`/`[interrupts]`) with `[mem.*]`, `[agent.*]`, `[region.*]`. New
structs `MemBlock{base,size}`, `Agent{kind,reach,cached,access,handoffs,
enabled_by}`, `Region{mem,agents}`. Closed-set handoff-encoding parse
(`byte_addr`/`word_addr`/`align N`). Self-consistency at parse time (fail
loudly): region `mem` within reach of every listed agent; mem blocks
non-overlapping; register paths kept as strings (resolved later -- `target.rs`
cannot see the SVD). Tests: `target.rs` units, `parses_startup_section` style.
Value: the DTCM footgun fires at target load, before any source.

### Slice 1 -- `in <region>` placement

`ast.rs` (extend `StorageAnnotation` / `StaticDef`), `parser.rs`
(`parse_static_def`), `ir.rs` (emit `section ".region.<n>"`), `target.rs`
(`generate_linker_script`: one MEMORY entry per mem block, one output section
per region `> MEMBLOCK`), new `region.rs` (the `in` name must be a real
region). Demo: `TX_DESC ... in dma_shared` lands in sram1; a dtcm-backed
region is rejected (slice 0). Replaces `@dma` placement. Risk: section
ordering/alignment needs a **QEMU exec fixture**, not IR-substring alone.

### Slice 2 -- `owns` + handoff-ownership rule

Whole-program visibility **confirmed**: `imports.rs::resolve_imports` flattens
every item from non-aliased imported modules into one `Program` via
`push_unique_def` (span-keyed dedup for diamonds). The peripheral-owning
modules are imported without an alias, so all `owns` clauses reach one pass.
Aliased imports keep their items in the `AliasMap` instead -- a known gap, not
hit by the realistic import style.

Done (2a): `owns` keyword + `Item::Owns`/`OwnsPath` (peripheral or
`Periph.REG`; field-level `Periph.REG.FIELD` rejected with E603 -- field
granularity is the deferred RCC-sharing story). `region.rs` resolves paths
against the peripheral table (E603) and flags a register owned by two
different files (E604, cross-module exclusivity). Field/peripheral overlap is
handled (owning a whole peripheral conflicts with owning one of its
registers). Tests: owns_ok / owns_conflict (two modules, one peripheral) /
owns_unknown / owns_field.

Done (2b): the handoff-ownership-required rule. `region.rs` builds the handoff
register set from the target (agent handoff strings -> (peripheral, register)),
the ownership maps from `owns` (whole-peripheral and per-register), and walks
every function body exhaustively (`walk_stmt`/`walk_expr`, including statements
embedded in block/if/match expressions, no catch-all) collecting peripheral
register/field writes. A write to a handoff register from a file that does not
own it is E605. Tests: handoff_unowned (E605) / handoff_owned (register) /
handoff_owned_peripheral (whole peripheral) / handoff_nonhandoff_write (an
ordinary register needs no ownership). The walker is reused by slices 3-4,
which also act at handoff write sites. The derived `drives` relation (M drives
A iff M owns one of A's handoff registers) is computable from these same maps;
surfacing it in LSP hover is deferred to the LSP track.

### Slice 3 -- Handoff encoding insertion (done)

The IR emitter carries the set of `word_addr` handoff field paths
(`set_word_addr_handoffs`, built from the target in `bml build`). At the
peripheral-field store (`ir.rs:3332`) it inserts `lshr i32 val, 2` for a
`word_addr` handoff field, so source writes the *byte* address and the field's
own bit-2 position re-aligns it: `(addr >> 2) << 2`. The hand-written `>> 2`
(and its double-shift bug class) is gone.

Only the field-write site is encoded (the real, unambiguous case: the SVD
models the field at bit 2). A register-level `word_addr` handoff would have
different semantics (whole register holds `addr >> 2`, no re-align), so it is
left for a concrete need.

`region.rs` adds the double-shift guard (E606): a source-level `>> 2` feeding a
`word_addr` handoff field is rejected, since the compiler already encodes -- it
shares the slice-2b write walk (now carrying the field name and an
`rhs_is_shr2` flag).

The encode is applied to the *widened* (i32) value, so a narrow RHS still emits
valid IR. Only plain assignment is encoded/guarded; a compound assign to a
handoff field (`PTR |= addr`) is neither -- OR-ing into a descriptor base
address is nonsensical, so this is left as an intentional gap rather than
given dubious semantics.

Proof: `exec_handoff_encode` round-trips the byte address through a RAM-backed
fake peripheral under QEMU at -O0/-O2 (the register reads back as the byte
address only if the `>> 2` was inserted). Plus `test_handoff_double_shift_rejected`
(E606). Verify-path encoding (so IKOS sees the same IR) is threaded in slice 4.

### Slice 4 -- Verify obligations (done)

Verify mode now auto-emits the probe's hand-written instrumentation. The verify
emitter is threaded (in `verify::verify`, which has program + target) with three
maps: `word_addr_handoffs` (closing the slice-3 deferral so verify IR matches
build), `region_addr_ranges` (placed static -> its region's mem block range),
and `handoff_reach_bounds` (handoff field path -> the owning agent's reach
bounding range).

- **Assume at the address-of site.** At the `ptr -> int` cast, if the operand is
  `&X` / `&mut X` for a region-placed static, emit `assume(region.lo <= addr <
  region.hi)` on the ptrtoint result. This is where the probe proved it has to
  go (ptrtoints of one global do not unify across functions), so the fact
  propagates through `desc_addr()` calls/returns/arithmetic to the obligation.
- **Assert at the handoff write.** On the widened byte address (before the
  word_addr encode), emit `assert(reach.lo <= value < reach.hi)` via
  `__ikos_assert`. The assert range is the **bounding box** of the agent's reach
  mem blocks -- sound for addresses below or above all reachable memory (catches
  the DTCM footgun), an over-approximation for a gap between disjoint blocks.
  `reach = *` / empty reach imposes no bound.

Both are `verify_mode`-only; the maps are empty for targets without
regions/handoffs, so non-region programs get byte-identical verify IR (existing
verify tests unaffected).

Proof (real IKOS, local llvm18 fork): `test_verify_handoff_provenance_ok` -- a
descriptor placed in the agent's reachable region, its address flowing through
a helper into the handoff, discharges clean (exit 0). `test_verify_handoff_unreachable_addr`
-- a DTCM address (`0x20000000`, below the sram1-only reach) handed to the
handoff is a definite `error[assert]` (V200), exit 1. The DTCM footgun is now
caught at the *value* level, complementing the placement-level checks of slices
0-1.

Known limits: the assume only attaches to the direct `&X as u32` form (a
pointer stored then later cast would not carry it); the bounding-box assert
does not catch an address in a gap between disjoint reach blocks; alignment is
not asserted (IKOS has no backward congruence narrowing -- the probe's finding,
to be revisited with compiler-owned exact addresses). IKOS emits an info-level
`V170` on each assume's unreachable branch (expected: the address fact is an
assumption IKOS adopts, not one it can independently derive); info does not
fail the default `--fail-on error`.

### Slice 5 -- Retire `@dma` (partial; full retirement re-scoped)

Done: corrected `doc/language.md`'s false claim. The memory-model table said
`@dma`/`@external` give "no elision/caching across accesses" (implying
volatile); verified against the IR that a `@dma` static is a plain global and
its index write lowers to a non-volatile `store i32` -- the optimizer may
elide/reorder/cache it. The table now states `@dma` is plain RAM with
Move-typing only, and ordering/visibility toward an agent is the programmer's
job (barriers + non-cacheable placement), which the regions/agents model makes
checkable.

**Full retirement is re-scoped, not a quick deletion.** Investigating
`@dma` showed it is `Type::Dma(Box<Type>)` woven through resolver/checker/
borrow/types/ir, and it carries *Move-typing* (aliasing safety) and the
index-read restriction -- semantics the regions/agents model (placement +
ownership + handoff provenance) does **not** replace. Deleting `@dma` now would
regress aliasing safety. And a real example port is blocked on in-memory
handoffs (the descriptor buffer pointers `RX_DESC[0] = rx_buffer_addr(0)`),
which are not designed yet. Plus the example is live on hardware (TX works), so
its port wants on-board validation, and the H723 target is not exec-testable
under the QEMU harness.

Blockers before retirement: (1) design in-memory handoffs; (2) decide what
carries `@dma`'s Move-typing / index-read safety once placement moves to
`in <region>` (perhaps the region, perhaps a separate marker); (3) port the
example additively (`@dma ... in dma_shared`, ETH agent + handoffs in the
target -- behavior-preserving since `(desc>>2)<<2 == (desc)>>2<<2` after
auto-encode) with hardware validation; then delete `Type::Dma`.

### Deferred

In-memory handoffs (`addr in <region>` fields) -- separate design doc first,
absorbs the descriptor-struct refactor. HIL manifest track -- separate.

### Ordering rationale

0 and 1 are independent of source semantics and useful immediately (placement
+ reach errors on the live board). 2 gates 3 and 4. 3 (codegen, proved by
exec) and 4 (verify, proved by IKOS) are separable -- a bug in one does not
block the other. 5 is cleanup once 1-4 subsume `@dma`. Biggest open risk: the
cross-module visibility confirmation in slice 2.
