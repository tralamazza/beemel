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
with an exhaustive write walker), slice 3 (handoff address writes -- originally a
`word_addr` encoding; superseded, see the 2026-06-09 note below), and slice
4 (verify obligations: provenance assume at `&X as u32`, reachability assert at
the handoff write, discharged by IKOS -- DTCM footgun caught at the value
level). Slice 5 partial: `doc/language.md` corrected; full `@dma` retirement
re-scoped. In-memory handoffs implemented (v1, struct-field `addr in R` with
the verify obligation; fixed the `arr[i].field` store miscompile en route).
Transitive reach done (E608: a descriptor field's region must be reachable by
the agent the descriptor is delivered to). Derived-Move done: an array placed in
a region a DMA/external agent mutates is wrapped in `Type::AgentShared` at
resolution (`region.rs::apply_derived_move`), so the index-read protection (E326)
is reproduced from placement with no new checks and no hand-written `@dma`.
Example port done and hardware-validated: `eth_dma.bml`/`stm32h723zg.target`
moved to `in dma_shared` + declared ETH agent/handoffs (auto-encoded `>> 2`),
behavior-preserving (IR diff is section + identical re-encode), TX still works on
the NUCLEO-H723ZG. The `@dma` annotation is then retired from the example
(dropping it is byte-identical IR -- derived-Move covers it); the carrier type
stays (renamed `Type::Dma` -> `Type::AgentShared`, unified with the identical
`Type::External`; `@dma`/`@external` remain distinct keywords mapping to it). The
descriptor-struct refactor is also done and hardware-validated (TX_DESC/RX_DESC
are `[TxDesc;2]`/`[RxDesc;2]` with `addr in dma_shared` buffer pointers; TX+RX
confirmed on the board) -- the ETH driver is fully regions-native. Remaining are
only the lower-priority deferred items (`addr` as a general type, read
re-establishing the in-region fact -- each generalization without a current
consumer).

**Update 2026-06-09 -- handoffs are register-level, no encoding.** The
`byte_addr`/`word_addr` handoff encoding and its double-shift guard (E606) are
removed. A handoff now names a *register* (`Peripheral.REGISTER`, not a field)
and source writes the full byte address to it verbatim. The descriptor-list and
tail-pointer registers reserve their low 2 bits read-only, so the hardware
ignores them and no shift is needed. The earlier `word_addr` `>> 2` only
cancelled the `<< 2` the SVD field's bit-2 offset applies -- writing the register
instead of the `[31:2]` field makes both vanish, collapsing the handoff write to
a single `store`. This deletes slice 3 entirely (the encoding axis,
`encode_word_addr_handoff`, `set_word_addr_handoffs`, E606) and changes the
handoff lowering from a read-modify-write to a plain store (not byte-identical
IR; `exec_handoff_full_addr` re-proves it under QEMU). Sections below that
describe `word_addr` are historical.

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

With `[mem.*]` blocks the linker derives every extent from the blocks, so the
flat `flash_*`/`ram_*` keys are unneeded: the code/flash block is inferred from
`vector_table_offset` (it is in flash by definition), and `data_block = <name>`
names the working-RAM block for `.data`/`.bss`/`.stack`. The flat keys remain
only for legacy targets with no mem blocks (`generate_linker_script` branches on
`mem_blocks.is_empty()`). A block's role -- code sink, data sink -- is a single
1-of-N selection, so `data_block` is one key naming one block, not a per-block
flag that could be duplicated; `flash_block()`/`ram_block()` fall back to
`flash_base`/`ram_base` for older targets.

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
handoff = Ethernet_DMA.DMACTxDLAR align 4
handoff = Ethernet_DMA.DMACRxDLAR align 4
handoff = Ethernet_DMA.DMACTxDTPR
handoff = Ethernet_DMA.DMACRxDTPR
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
- `handoff` names a register (`Peripheral.REGISTER`) with an optional `align N`
  (minimum alignment of the handed-off address). The full byte address is
  written verbatim -- these are dedicated address registers whose reserved low
  bits the hardware ignores, so there is no encoding/shift.
- These sections are per-chip facts. They belong in a vendored base target,
  written once per chip (ideally generated/audited from the reference manual's
  bus-matrix table). A project target `include`s that base and adds its own
  policy on top -- see "Target composition" below.
- `bus = start..end, ...` (optional, per agent) transcribes the reference
  manual's bus-master-to-bus-slave table (RM0468 Table 2) as half-open address
  windows -- the union over the agent's master ports. When declared, every
  block in `reach` must fit inside a window or the target fails to load.
  `reach` is a *claim* (project intent); `bus` is a *transcription* (manual
  facts); the cross-check means a bad placement needs both to be wrong.
  Restating the key replaces the list (overridable like `reach`). The reach
  containment check treats the windows as a union over ports: it catches what
  NO port can address. (The original "MDMA cannot reach TCM" diagnosis was
  wrong -- it was a port bit, RM0468 2.1.2 states the AHBS path explicitly.)
- Software port selection (E612): when a port is chosen by software (the H7
  MDMA routes each side through AXI or AHBS per `MDMA_CxTBR.SBUS/DBUS`), tag
  the windows (`bus = axi: ..., ahbs: ...`; a tag is sticky over following
  items) and declare the select on the handoff:
  `handoff = MDMA.MDMA_C0DAR port_by MDMA.MDMA_C0TBR.DBUS ahbs`. The check
  (region.rs::check_handoff_port, off the same write walk) resolves where the
  handed-off address lives (literal `&STATIC` -> region -> mem block -> the
  covering windows) and requires the bit to match: a block behind tag-only
  windows requires the field set somewhere (presence semantics, like E609); a
  block behind no tagged window rejects a definite set (misroute). Mixed or
  unknown coverage is skipped, conservative. Target load validates that a
  `port_by` tag names at least one tagged window. This is "usage dictates
  declaration" for ports: the requirement is derived from where the address
  lands, not declared per use.

## Target composition (`include`)

`bml` takes a single `--target`. A target file may `include = <path>` other
targets (resolved relative to the including file); includes load first and the
including file applies on top, so later definitions override or extend earlier
ones. This is what keeps physics and policy in separate files: a vendored
`stm32h723.target` carries only scalars + `[mem.*]` + `[agent.*]` (the chip),
and a project target does `include = stm32h723.target` then declares its
`[region.*]`. Re-opening a named section resumes editing that entity --
`[mem.dma_pool]` + `cacheable = false` flips one field and keeps the inherited
base/size (key-level merge), a bare scalar overwrites, an accumulator line like
`handoff` appends. Everything is overridable (so a project can also patch a base
bug); each file is applied at most once (diamonds dedup, cycles terminate);
validation runs once on the merged target. Implemented in `target.rs`
(`from_file` -> `load_file` -> `apply`/`finalize`); the
`nucleo-h723zg-ptp` example is split this way (`stm32h723.target` base +
`stm32h723zg.target` project).

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
- **Cache discipline** (failure mode #3): DONE (detection **and enforcement**).
  Detection: `validate_regions` rejects a cacheable region (the `cacheable`
  mem-block attribute, default `true`) shared by a cached CPU and a non-snooping
  `dma`/`external` agent. Fix: declare the mem `cacheable = false`. **Enforcement
  (Stage 4, hardware-validated):** a `cacheable = false` mem block is *generated*
  into an MPU non-cacheable region (`Target::mpu_regions` -> `arch/arm.rs` emits
  the MPU setup at the start of `reset_handler`, before `.data`/`.bss` and any
  cache: disable MPU, per region RNR/RBAR/RASR = Normal non-cacheable shareable
  RW XN, enable MPU + DSB/ISB). `validate_regions` requires each non-cacheable
  block be MPU-encodable (power-of-two size >= 32, size-aligned base). On the
  NUCLEO the example now runs with the **D-cache enabled**: `dma_pool` is
  `cacheable = false`, the generated MPU keeps the CPU coherent with ETH/MDMA, and
  `RX_PACKET_COUNT` keeps advancing (`SCB_CCR.DC=1`, `MPU_CTRL=0x5`). So the
  trusted claim is now enforced silicon config, not just a forced declaration --
  the last detection-only founding failure mode is closed. (`reach` is now
  cross-checked against `bus` windows when the base target declares them --
  see the `bus` key above; `cacheable` remains a trusted declaration, but a
  wrong one is at least *visible* silicon config now, not silent.)
- **Alignment-as-derived-physics** (same physics, second consequence): DONE.
  RM0468 confirms the ETH DMA imposes *no* buffer-address alignment ("There is
  no limitation to the buffer address alignment", Table 579) and only word
  alignment on the descriptor list -- so the `@align(32)` on the ETH buffers is
  purely the Cortex-M7 cache line, the same fact the cache check uses.
  `Target::region_alignments` derives it: a static placed `in R` is emitted with
  at least `cache_line_size(cpu)` alignment (32 for M7) when `R`'s mem is
  cacheable and shared with a non-coherent agent; the emitter floors the global's
  `align` (`ir.rs`, `region_alignments` map), and an explicit `@align` can still
  raise it. Dropping the four `@align(32)` from `eth_dma.bml` is byte-identical
  IR. The number is now correct-by-construction for the cache state instead of a
  hand-written literal.
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
two things (the full byte address is stored verbatim -- no encoding):

1. **Static reach check.** If the value's provenance is statically known and
   the target is outside every region the agent can reach, `bml check`
   rejects it.
2. **Verification obligation.** Otherwise `bml verify` discharges it: the
   compiler emits `assume(range)` at the address-of site of the source symbol
   and `assert(in-region)` before the handoff write. See next section for why
   the assume goes there.

### In-memory handoffs (implemented, v1)

Done as scoped: `addr in R` is a struct-**field**-only type (byte address, no
encoding), with the verify obligation and the no-transitive-reach simplification
chosen. `Type::Addr(String)` / `TypeExpr::Addr(Ident)` thread through
resolver/checker/types/ir as a `u32`-layout, 4-byte, 4-aligned, Copy value;
`types_compatible` interconverts `addr in R <-> u32` (a byte address writes in,
reading yields `u32`). The region pass rejects an unknown field region (E607).
At a write to an `addr in R` field, verify mode emits `assert(value in
R.range)` (reusing `emit_range_assert`), discharged from the slice-4 provenance
assume at `&BUFFER as u32` -- no new verify machinery, just a region-name range
map.

This surfaced and fixed a real silent miscompile: a store to a field of an
indexed array element (`RX[i].buf1 = ...`, the descriptor shape) was *dropped*
(`lvalue_base_info` returned `None` for an `Index` base). `Index` now returns
the element pointer + type (mirroring the read side), so the store happens. An
exec fixture (`field_of_index_store`) pins it under QEMU.

Proof (real IKOS): a buffer address in the field's region, written through a
helper into `RX[0].buf1`, discharges clean; a DTCM address (out of region) is a
definite `error[assert]`; E607 on an unknown field region.

Transitive reach (E608) is now done: when a descriptor is delivered to an agent
(`agent_handoff = &RX`), every `addr in R` field inside it must name a region
the agent can reach, else `error[E608]`. The remaining deferred items (`addr` as
a general type, read re-establishing the in-region fact) are generalization
without a current consumer and stay open.

The original design follows.

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
pointer: no pointee type, no deref; reading it yields a `u32`. ETH buffer
pointers are byte addresses, so the value is stored verbatim (no shift).

**The write to an `addr in R` field carries the register-handoff actions**, and
reuses the slice-4 machinery:

1. *Static reach check* -- if the written value's provenance is a static placed
   in a region not contained in `R`, reject in `check`.
2. *Verify obligation* -- emit `assert(value in R.range)` at the field store.
   The provenance `assume` is already emitted by slice 4 at `&BUFFER as u32`
   (the buffer's region range), so this reuses `region_addr_ranges` and
   `emit_range_assert` unchanged. The same index juggling slice 4 catches on
   register handoffs (`base + i*512` with unbounded `i`) is caught here.

So in-memory handoffs are, mechanically, register handoffs whose "register" is
a struct field and whose target range is the field's own `in R` (rather than an
agent's reach). They are in one way *simpler*: the constraint region is
explicit on the field, no agent-reach lookup.

**Detection / lowering.** The struct-field store path in `ir.rs` already GEPs to
the field and stores; when the field type is `addr in R`, add the assert
(verify). The checker needs the new field type (`Type::Addr`) threaded through
`types.rs`/resolver/checker like other field types, with `sizeof == 4` and
packed layout.

**Open questions.**

- *Transitive reach.* DONE (E608). `addr in R` constrains the value to `R`; the
  transitive check ties the descriptor to the agent through the delivering
  handoff (`agent_handoff = &RX_DESC`) and requires `R.mem in agent.reach` for
  every `addr in R` field inside the delivered struct (descending through arrays
  and nested structs). It only fires when the delivery is a literal address-of a
  static (`&RX` / `&RX[0]`); an indirect base (a helper returning `u32`) is not
  tied to a descriptor type and is conservatively skipped. Implemented in
  `region.rs::check_descriptor_reach`.
- *`addr` as a general type.* v1 scopes it to struct fields (the descriptor
  case). Whether locals/params/returns may be `addr in R` (an address proven
  in-region flowing around) is deferrable; the helpers (`rx_buffer_addr`)
  currently return `u32` and the provenance flows through that fine.
- *Reading.* Reading an `addr in R` field yields `u32`. Whether a read should
  re-establish the `in R` fact (so a value loaded back from a descriptor is
  known in-region) is open; not needed for the write-obligation use case.
- *Move/aliasing.* DONE (see slice 5). `@dma`'s real content is the index-read
  protection (E326); `region.rs::apply_derived_move` derives the existing
  `Type::AgentShared` carrier from agent-shared placement rather than a storage-class
  wrapper or the descriptor struct. Pinned by `region_index_read.bml` /
  `cpu_region_index_read.bml`.
- *Reclaim (runtime ownership, Stage-3 dogfood).* DONE (direction "C", trusted).
  The model is spatial: `AgentShared` blocks index-reads forever, with no notion
  of the OWN/transfer handshake that makes a post-transfer CPU read safe. The
  handoff write *is* the release; `reclaim(x)` is the missing **reclaim** -- it
  yields a bounds-checked `view` over agent-shared memory, the explicit
  handshake-acknowledged escape. Dogfooding also found that a plain `view(x)`
  silently bypassed E326 over agent-shared memory; the contiguous `view` is now
  tightened to reject it (E335, points to `reclaim`). `reclaim` is
  `Expr::ViewNew { reclaim: true }`; checker only, zero IR change (lowers like
  `view`). Pinned by `view_agent_shared.bml` (E335), `reclaim_plain_array.bml`
  (E335), `view_over_dma.bml` (reclaim ok).
- *Sound reclaim (direction "B", v0).* DONE. An agent declares its
  transfer-complete signal in the target -- `completes_by = P.R.F` (mirrors
  `enabled_by`); declaring it activates the guard, leaving `reclaim` trusted
  otherwise. `region.rs::check_reclaim_guards` (E611) then requires every
  `reclaim(BUF)` of that agent's buffer to be control-dependent on observing the
  flag. v0 is sound but conservative via **span containment**: the reclaim must
  lie lexically in the then-block of an `if <flag>` (proven by span, no
  flow-sensitive walk). **Helper predicates now recognized** (broadened): a fn
  whose result is the flag read (`fn mdma_done() -> b1 { return F; }`) maps to its
  flag, so `if mdma_done() { reclaim }` -- the idiomatic form -- counts; only a fn
  that actually returns the flag qualifies (a non-predicate guard stays E611).
  Still NOT recognized (conservatively rejected): `while !flag {}` busy-waits,
  negated/compared conditions, and per-buffer association (v0 assumes one async
  agent per region). The full B (control-flow domination across those forms) is
  the follow-up. Pinned by `reclaim_guarded{,_helper}.bml`,
  `reclaim_unguarded.bml`/`reclaim_guard_nonpredicate.bml` (E611); dogfooded on
  `copy_dma.bml` (`if mdma_done()`).
- *Bus-matrix cross-check (reach verification, v0).* DONE. The deepest finding
  of the single-board dogfood was that `reach` is trusted physics the silicon
  can falsify. The `bus` key (see Layer 1) turns it into a cross-checked claim:
  windows transcribed from RM0468 Table 2 in the vendored base target, reach
  containment validated at target load. Dogfooding the transcription itself
  *falsified our own earlier conclusion*: the MDMA/DTCM TED error was a
  misconfigured `MDMA_CxTBR.DBUS` port bit, not unreachability -- the manual
  (2.1.2) and Table 2 both give MDMA an AHBS path to the TCMs. The windows are
  port-unions, so they catch what *no* port can address (ETH -> TCM dies at
  target load, verified) and deliberately accept MDMA -> DTCM (verified).
  Pinned by target.rs unit tests (`reach_outside_bus_windows_*`,
  `bus_windows_override_last_wins`).
- *Port-select check (E612).* DONE, hardware-validated -- the full-circle close
  of the DTCM saga. Tagged windows + `port_by` on the handoff (see Layer 1)
  make the compiler require `MDMA_CxTBR.DBUS` whenever the handed-off address
  is behind the AHBS-only window. The example now does the copy the dogfood
  originally failed at: COPY_SRC (D2, AXI side) -> SCRATCH (DTCM 0x20000000,
  AHBS side) with `DBUS = true`; on the NUCLEO the ramp lands in DTCM
  (C0ISR=0x1E, TEIF0=0), the guarded reclaim consumes it, RX stays coherent
  (D-cache + MPU, now two generated regions: dtcm + dma_pool). Removing the
  DBUS line is E612 at the C0DAR write -- the exact runtime TED converted to a
  compile error. Pinned by `handoff_port_{missing,ok,misroute}.bml` +
  `handoff_port.target`.
- *Toward unifying with the ceiling protocol.* The two concurrency disciplines
  (ceiling = mutual exclusion for CPU contexts; release/reclaim = ownership
  transfer for async agents) are one concept -- region ownership -- with the
  transfer mechanism derived from the sharer set (instant priority-raise for CPU
  contexts, signal-gated handshake for agents). B is the shared engine; the plan
  is to build it, show the ceiling reduces to its instant case, then fold them.
  - *Slice U1: derived ceilings.* DONE, hardware-validated. The observation:
    the declared `ceiling=N` is a number the compiler can compute -- every
    accessor's context is static, the emitted critical section is a global
    mask (`cpsid i`), so N only feeds E402 and the skip-CS optimization at the
    top accessor, both functions of the accessor set. Bare `@shared` now
    derives it (`ceiling.rs`: min context level over functions mentioning the
    static; materialized in the resolver, zero changes downstream).
    `@shared(ceiling=N)` stays as a pin -- an accessor outranking it is E402,
    which is precisely "the pin disagrees with usage". This is the CPU-side
    mirror of derived-Move: the annotation acknowledges *that* the data is
    shared; mechanism and parameters come from the sharer set. Conservative
    v0 edges (documented in ceiling.rs): name-based access scan, `Any`
    contexts contribute nothing (their accesses stay conservatively masked,
    caller contexts not propagated -- a blind spot the declared form has
    too). Dogfooded on the NUCLEO: TIM2 update IRQ (`@isr("TIM2",
    priority=2)`) counts into a bare-`@shared` TICKS consumed by the thread
    loop; the generated IR has no cpsid in the ISR (top accessor) and a
    cpsid/cpsie pair around the thread read; on the board TICKS advances at
    ~1 Hz with ETH RX, the MDMA/DTCM copy, MPU, and D-cache all intact --
    real preemption against the derived protection. Pinned by
    `shared_derived_{isr_top,low_isr_cs,thread}.bml` (IR) +
    `exec/shared_derived.bml` (QEMU).
  - *FINDING (new trusted physics): `@isr(priority=N)` was a claim.* CLOSED
    for the priority half (hardware-validated): the generated reset handler
    now programs each `@isr` IRQ's NVIC IPR byte from the annotation
    (`priority << (8 - priority_bits)`, collected during vector-table
    assembly in `arch/arm.rs`; `priority_bits` threaded from the target). On
    the NUCLEO, IPR7 reads 0x20 for IRQ 28 with no hand-written NVIC priority
    in source -- the value the ceiling model reasons over IS the silicon
    config. The *enable* (ISER) deliberately stays application code:
    enabling at reset could fire an ISR before its peripheral is initialized
    -- priority is static physics, enable is runtime policy. Limitations,
    recorded: ARMv6-M skipped (IPR is word-access-only there); system
    exceptions (SysTick/PendSV/...) use SHPR, not modeled; a user-written
    reset handler gets no generated NVIC programming (same as
    startup_init/MPU). Pinned by `isr_priority_program.bml`.
  - *Slice U2: blocking-acquire guard forms.* DONE, hardware-validated. E611
    now accepts the full acquire vocabulary, all by span containment:
    `if <flag> {}` (try-acquire, the v0 form), `while !<flag> {}` busy-wait
    (blocking acquire -- the lock-style form the fold needs; body must be
    EMPTY, since a non-empty body could hide a `break` exiting with the flag
    clear), and `if !<flag> { return; }` (early-exit acquire; no else, then
    must `has_direct_terminator`). The blocking forms establish the flag for
    the REST of the enclosing block (`rest_of_block` span; same soundness
    level as the then-block form -- code in the span could clear the flag,
    that is the full flow-sensitive B). Predicates compose (`while !done()
    {}`). Dogfooded: `copy_dma.bml` does a blocking acquire right after
    triggering (`while !mdma_done() {} reclaim`) AND keeps the try-acquire in
    `copy_poll` -- both validated on the NUCLEO in one program
    (SCRATCH_FIRST=0xA1, SCRATCH_DONE=1). Pinned by
    `reclaim_busywait{,_helper,_body}.bml`, `reclaim_earlyexit.bml`,
    `reclaim_before_wait.bml` (ordering negative).
  - *Slice U3: call-graph context propagation + composition guards.* DONE.
    Driven by the mixed-sharer probes (falsification first): (1) direct ISR
    access to a region static was already E404; (2) the SAME access through
    an unannotated helper built silently -- an `Any` hop laundered the ISR
    out of E404/E402 and out of the derived ceiling (a pre-existing soundness
    hole in the ceiling protocol itself, not just regions: ISR(1)-via-helper
    let a pinned-or-derived top accessor skip its critical section while
    still preemptible); (3) `@shared in R` silently displaced the derived
    AgentShared carrier and was only safe by accident. Fixes:
    `ceiling.rs::propagate_contexts` (call edges from the same exhaustive
    mention scan as the derivation -- `&f` counts as an edge, the safe
    direction; fixpoint union of caller contexts into `Any` fns; stored as
    `SymbolTable::fn_possible_contexts`), consumed by the ceiling derivation
    (`Any` fns now contribute their known callers) and by E404/E402 in
    borrow.rs (an `Any` body reachable from an ISR is checked as that ISR).
    `@shared` + `in <region>` is now rejected loudly (E613) until the
    composed construct exists. Blind spot, recorded: pointer CALLS are not
    connected to the call site's context. Pinned by
    `ctx_launder_{isr,ok,shared_pin}.bml`, `region_isr_launder.bml` (the
    motivating ISR-vs-thread race over agent-shared memory, now E404),
    `shared_in_region.bml` (E613), `shared_derived_propagated.bml` (the
    soundness fix visible in IR: the top accessor now takes its cpsid when a
    higher-priority ISR reaches the static through a helper).
  - *Slice U4: `claim` -- the masked ownership window.* DONE,
    hardware-validated: on the NUCLEO, LOG_SUM tracks the atomic-snapshot
    invariant 4*TICKS - 10 exactly at independent sample points (54 at
    TICKS=16, 90 at TICKS=25) with the ISR, ETH, and MDMA stack intact. (A
    one-off FORCED/IMPRECISERR HardFault seen mid-session was a debugger
    attach artifact from a flaky ST-Link USB period -- unreproducible after
    a clean reset.) The reclaim-shaped escape for CPU-shared data:
    `claim X { ... }` wraps the block in ONE cpsid/cpsie pair; inside, the
    `@shared` static is its inner type (views and index-reads allowed -- the
    checker and emitter recurse with `SymbolTable::with_claimed`, a patched
    table with the `Shared` wrapper stripped) and per-access critical
    sections are suppressed (`claim_depth`; an inner cpsie would unmask the
    window early -- also why nested claims emit no second pair). E614
    rejects: a non-`@shared` target, calls inside (a callee's own critical
    sections would cpsie mid-window), and escapes (`return`, or
    break/continue of an outer loop; loops fully inside are fine). View
    escape through a pre-declared local was TRUSTED at first -- the same
    lifetime gap `reclaim` had; both are now closed by E616 (scoped view
    lifetimes, below). The acquire symmetry is now complete: `claim` enters by
    masking (instant acquire), `reclaim` by observing the completion signal;
    E405's message points at `claim`. Dogfood: tim2_isr logs ticks into
    `TICK_LOG: [u32;4] @shared` (top accessor, no CS), the thread drains all
    four atomically in `timer_log_sum` (`claim` + view; IR shows exactly one
    pair there, zero in the ISR). Pinned by `claim_view.bml` (+IR),
    `claim_{not_shared,call,return,break}.bml` (E614),
    `exec/claim_window.bml` (QEMU).
  - *Slice U5: the composition -- `@shared in R` (E613 lifted).* DONE,
    hardware-validated on the NUCLEO: one readback shows all three composed
    windows ran -- SCRATCH_CHECK=0xA0/DONE=1 (thread try-acquire),
    SCRATCH_FIRST=0xA1 (blocking acquire), SCRATCH_ISR_SEEN=0xA2 (the ISR
    consumer) -- with TICKS advancing and the TICK_LOG snapshot invariant
    still exact (LOG_SUM = 4*TICKS - 10) while the ISR takes its own claim
    window every tick. The
    mixed-sharer case (CPU contexts AND an async agent on one buffer)
    composes by NESTING the carriers: `apply_derived_move` turns
    `Shared(Array)` into `Shared(AgentShared(Array))`. Outside a `claim`
    window the outer Shared blocks everything including `reclaim` (its base
    must be AgentShared; a dedicated E335 message points at `claim`), so the
    masked window is required BY CONSTRUCTION; inside `claim` the patched
    table strips the outer Shared and the static is the plain agent-shared
    world -- reclaim, the E611 guards, E326 all compose with zero new check
    logic. The consumption idiom is `claim X { if <flag> { reclaim(X) } }`
    (inline flag reads -- claim bodies forbid calls) or the blocking form
    with the busy-wait inside the claim. E613 is retired. Both windows from
    both contexts in the example: SCRATCH is `@shared in tcm_scratch`,
    consumed by the thread (copy_poll try-acquire, copy_setup blocking
    acquire) AND from the TIM2 ISR (copy_isr_peek via tim2_isr) -- the exact
    ISR-vs-thread race over an agent's buffer that U3's probes could only
    reject is now expressible and checked. IR: one cpsid/cpsie pair per
    consumer window, none in the ISR's top-accessor writes. The derived
    ceiling of SCRATCH comes from U3's context propagation (the ISR reaches
    it through an Any helper). Pinned by `shared_in_region.bml` (composed,
    accepted) and `shared_in_region_noclaim.bml` (reclaim without claim
    rejected, message points at claim).
  - *Multi-core slice 1: core identity + cross-core sharing (E615).* DONE
    (compiler side; hardware launch demonstrated with an open finding). A
    cpu-kind agent binds its code via `entry = <fn>` (project policy;
    pico2w.target binds core1 to core1_main). Core-reachability propagates
    from roots (main + ISRs -> implicit core0; declared entries -> their
    core, PINNED so the launch site taking `&entry` does not poison the
    entry's tree -- consequence: directly calling another core's entry is
    undetected, documented) over the same mention edges as context
    propagation (`ceiling::fn_mentions`, shared). A mutable static reachable
    from multiple cores is E615 -- including `@shared` ones (the ceiling's
    cpsid masks ONE core; claim likewise). Module consts are freely shared.
    ISRs are assumed core0 (per-core NVIC modeling deferred). E615's FIRST
    REAL CATCH was our own bring-up: the relaunch watchdog read core1's
    counter from core0; the fix demonstrates the sanctioned pattern
    (cross-core observation via the SIO FIFO MMIO channel). Interplay
    finding: E408 forbids `&fn` of @context fns, so v0 entries must be
    unannotated (Any); an E408 carve-out for declared entries is recorded.
    Pinned by cross_core_{static,partitioned,shared}.bml + cross_core.target.
    HARDWARE: core1 launch via the bootrom FIFO handshake (datasheet 5.3,
    PSM reset + {0,0,1,vtor,sp,entry|1} + echo verify + bounded-ack retry,
    all in bml) VALIDATED under power-on boot: first-try launch, core1
    counting at ~1.9M/s indefinitely, every 64Ki-iteration FIFO heartbeat
    received losslessly (CORE0_BEATS == COUNT >> 16 exactly), zero faults,
    the DMA probe and core0 untouched. RESOLVED FINDING: the earlier
    "bimodal launch" / "core1 stops at ~40ms" behaviors were artifacts of
    DEBUGGER-INITIATED resets (openocd `reset run`): under a debug reset
    core1 either came up with SRAM stores silently dying while SIO MMIO
    writes worked, or stopped executing ~80k iterations in -- ACCESSCTRL
    fully open in both modes, no fault taken (shared-vector HardFault
    recorder stayed clean), no matching erratum. Dev workflow: flash over
    SWD, then POWER-CYCLE for clean multi-core runs. Instrument caveat
    recorded: openocd cm1 `reg pc` is unreliable with this config.
  - *Multi-core slice 2: cross-core `claim` (hardware-spinlock-backed).*
    DONE, hardware-validated. The unification's thesis crosses the core
    boundary: the target declares its mutex physics (`spinlock_base` /
    `spinlock_count`, read-to-claim / write-to-release -- the RP2350 SIO
    bank at 0xD0000100 x32, transcribed from the user-provided SVD), and a
    cross-core `@shared` static becomes legal IFF every access sits inside
    a `claim X {}` window (E615 relaxes from reject to require-window; new
    span walker `claims_and_mentions`). Each such static gets a
    deterministic lock index (`region::cross_core_locks`, name order,
    overflow checked); the claim lowering adds spin-acquire (volatile load
    until nonzero) after the cpsid and release (store) before the cpsie, at
    any nesting depth -- the mask only excludes the local core. Spinning
    masked is sound: the holder is the other core, whose progress does not
    need our IRQs. No QEMU exec fixture is possible (single-core machine,
    no SIO bank); the silicon validation serves as the exec proof: both
    Pico 2 W cores increment both slots of one `@shared` pair inside their
    windows with an in-window A==B invariant check -- ZERO violations
    across ~44 million contended windows from power-on, first-try launch,
    DMA probe and core0 untouched. (Debugger note: an SWD snapshot of the
    pair legitimately shows skew -- the two word reads straddle thousands
    of windows; the in-window counters are the real invariant.) Pinned by
    cross_core_locked{,_nophys}.bml (+IR spinlock values),
    cross_core_unclaimed.bml, cross_core_locks.target.
  - *Scoped view lifetimes (E616) -- DONE.* The trust gap claim and reclaim
    shared: a window minted a view and nothing kept it from outliving its
    justification. One code, three teeth, all lexical span/order reasoning
    (no flow sensitivity, same soundness class as E611):
    1. *Claim escape* (`checker.rs::check_claim_view_escape`): inside
       `claim X {}`, a view/ring/bits over X -- or an inside binding holding
       one, lexical taint -- assigned to a binding declared outside the body
       (or through a pointer) is rejected. Value copies out are the point of
       the window and stay legal. Calls cannot smuggle (E614 forbids them).
    2. *Reclaim mention containment* (`region.rs::check_fn_reclaims`): a
       mention of a binding whose most recent whole-name binding event is a
       guarded `reclaim` must sit inside a guard span that also contains the
       reclaim. Events (reclaim-bind vs kill/rebind) are judged per name in
       source order, per function, so name reuse across windows and rebinds
       to harmless views do not trip it. Lvalue bases now count as mentions
       (and reclaims in lvalue index positions are now seen at all -- a
       latent E611 gap the new `gscan_lvalue` closed).
    3. *Release truncation*: a write to a handoff register of an agent that
       declares `completes_by` is a RELEASE -- it hands the buffer back, so
       the observed completion covers the previous transfer. Between guard
       and reclaim it re-opens E611 (tailored message); between reclaim and
       a later mention it is E616. Conservative: any handoff write with
       matching flags counts, even delivering a different buffer
       (per-buffer association is the standing follow-up).
    Falsified live on both examples: hoisting `copy_dma.bml`'s reclaim into
    a pre-declared local fires the claim escape; moving `probe.bml`'s
    CH0_WRITE_ADDR release after the busy-wait fires the truncated E611;
    using `first` after a re-arm fires E616 -- and both untouched examples
    still build (release-before-guard, the canonical arm->wait->consume
    order, is explicitly not flagged). Check-only slice, zero IR change.
    Pinned by claim_view_escape{,_taint}.bml, reclaim_view_escape.bml,
    reclaim_{after,use_after}_release.bml, reclaim_release_before_guard.bml,
    reclaim_name_reuse.bml, reclaim_release.target. Recorded blind spots:
    views carried across a loop back-edge (mention textually before the
    reclaim), addresses cast to integers (verify/provenance domain),
    same-name shadowing games inside one claim body (false-negative only).
  - *Claim-aware verify -- DONE.* The first time `claim` pays off in proofs
    rather than only in rejected programs, plus a preempt-shim soundness fix
    the probe exposed:
    1. *Soundness (verify/preempt.rs):* the shim skipped any reader that was
       ITSELF a writer of the static, so a thread's write-then-read-back
       (`X = 7; x = X; assert(x == 7)`) was wrongly proven even though an
       ISR writer can fire between the two (CS'd) accesses -- confirmed
       live: zero `forget_mem` calls in the .ll, IKOS "proves" the assert.
       Correct rule: a fn only cannot preempt ITSELF; being a writer does
       not exempt it from other higher-priority writers.
    2. *Precision (ir.rs):* the emitter keeps a `claimed_statics` stack;
       in verify mode, reads of the CLAIMED static inside its window are
       not havoc'd -- the mask stops local preemption and the spinlock
       (cross-core) excludes the other core, so in-window stability is
       exactly the consistency the window provides. Other statics read
       in-window keep their havoc. Without this, fix 1 would have turned
       every legitimate in-window read-back into a false V200.
    Net: the same read-back is V200 outside the window and PROVEN inside
    it (verify_shared_writeback.bml / verify_claim_window.bml). Arrays
    need no havoc at all, by construction: E326 forbids element reads of
    `@shared` arrays outside `claim`, and inside the window suppression is
    the correct semantics -- the checker closes upstream what the verify
    model would otherwise have to havoc. H7 example verify findings are
    byte-identical before/after (all pre-existing; the unproven
    descriptor-reach V200s in eth_dma.bml remain a standing item). Not
    modeled: agent (DMA) concurrency on reclaimed buffers -- reclaim views
    load without havoc; the lexical E611/E616 windows are the guard there.
  - *Remaining (smaller):* pointer-call
    context edges; compared guard conditions; per-buffer flag association;
    flag staleness across transfers (a release BEFORE the guard whose flag
    was never cleared -- needs W1C discipline modeling);
    ETH link-up recovery in the H723 example driver.

**Why this is the next slice.** It unblocks the `eth_dma.bml` descriptor-struct
refactor (direct typed indexing, no `*u32` index-read workaround), it is the
prerequisite for retiring `@dma`, and it closes the last place an unchecked
address reaches an agent.

## Cross-vendor falsification: RP2350 (Pico 2 W)

The model grew entirely on one chip, so the standing falsification question
was whether the vocabulary is BML or secretly ST. Exercise: transcribe the
RP2350 (datasheet sections 2.1/2.2/7/12.6, vendored at
`~/Documents/rp2350-datasheet.pdf`) into `bml/examples/rp2350-pico2w/`
(`rp2350.target` chip physics + `pico2w.target` board/project + `probe.bml`,
compile-only; board bring-up -- IMAGE_DEF boot block, SWD flow -- is its own
milestone, the user has a debug probe).

**Held with zero new keys:**

- The H723 idiom transposes wholesale: `owns DMA`, address handoffs
  (`CH0_READ_ADDR`/`CH0_WRITE_ADDR`, full byte addresses verbatim),
  `completes_by = DMA.INTR.CH0` (raw per-channel completion bit, done-high),
  region placement, guarded reclaim. The probe compiled first try; the
  deliberate mistakes fire the same errors (unguarded reclaim E611, handoff
  without owns E605).
- Bus windows express the opposite reach pole from the H7/nRF: a full
  crossbar where DMA reaches everything EXCEPT the core-local segments (SIO
  0xd0000000, M33 PPB 0xe0000000, on dedicated per-core paths). A reach claim
  over an SIO-range block dies at target load -- verified.
- The chip has no internal flash, so flash size is BOARD physics: a
  three-layer chip/board/project split, and the include chain handled it
  unchanged.
- `cpu = cortex-m33` was a one-line cflags addition (we emit v7e-m, which
  ARMv8-M Mainline executes).

**Vocabulary gaps found (the point of the exercise):**

1. **`enabled_by` assumes set-to-enable.** CLOSED: the `!` polarity marker
   (`enabled_by = !RESETS.RESET.DMA`) expresses clear-to-enable gating. E609
   requires the clearing write; E610's stomp direction flips (a stranger
   RE-ASSERTING the reset bit is the stomp; for the inverted case any
   non-clearing field write by a stranger counts -- writing an agent's reset
   bit from outside is suspect regardless of the computed value). The RP2350
   target now declares it and the probe's reset-clear is checked physics
   (verified: removing the clear is E609). Pinned by
   `enable_inverted{_ok,_missing}.bml` + `clock_stomp_inverted.bml`.
2. **MPU generation is PMSAv7-only.** CLOSED, hardware-validated:
   `MpuFlavor` follows the core (`cortex-m33` -> PMSAv8), the validation
   relaxes to 32-byte granularity (no power-of-two rule), and the emission
   branches to MAIR0 (attr 0 = 0x44, Normal non-cacheable) + per-region
   RBAR (base|SH=00|AP=01|XN) / RLAR (limit|AttrIndx=0|EN). The Pico 2 W
   target sets `has_mpu = true` and marks the non-striped sram8 bank
   (hosting dma_buf) `cacheable = false`; on the board MPU_CTRL=5,
   RBAR=0x20080003, RLAR=0x20080FE1, MAIR0=0x44 read back exactly as
   emitted with the DMA probe running inside the covered bank. (RP2350 has
   no data cache, so the attribute is configuration truth, not a coherence
   requirement -- the point was the v8 emission on real PMSAv8 silicon.)
   The PMSAv7 path is byte-identical post-refactor (H723 IR diff clean).
   Pinned by `pmsa8_mpu.{bml,target}` (IR) and the pmsa8 target.rs unit
   tests.
3. **E611 misses the wait-while-set idiom.** CLOSED: `completes_by =
   !DMA.CH0_CTRL_TRIG.BUSY` declares a busy-HIGH flag (done-when-clear), and
   the guard machinery is polarity-generic -- `cond_flag` normalizes `!` (a
   negated condition yields the negated fact, double negation strips), every
   blocking form establishes the NEGATION of its loop/exit condition, and
   guards match flags as polarity-carrying strings. So `while BUSY {}`,
   `if !BUSY { reclaim }`, and `if BUSY { return; }` all guard a `!BUSY`
   flag, while `if BUSY { reclaim }` -- reclaiming while the agent still
   writes -- is rejected (verified). Pinned by `reclaim_waitset.bml` /
   `reclaim_busy_wrongform.bml`; the RP2350 probe uses the native idiom.
4. **core1 is a declarable but inert agent.** `[agent.core1] kind = cpu`
   parses and validates; no check consumes a second cpu agent (multi-core is
   the recorded deferred track). The declaration at least makes the sharer
   visible in the physics file.

Net: the model is not ST-shaped -- the failures were specific polarity /
architecture gaps, and the two polarity gaps closed with a single `!` marker
on the existing keys (PMSAv8 MPU emission remains the open one).

**Board bring-up: DONE, hardware-validated on the Pico 2 W.** One new
chip-agnostic mechanism: `[boot_block]` in the target file -- literal words
the generated linker script emits in a `.boot_block` section directly after
the vector table. The RP2350 content is the 5-word minimum Arm IMAGE_DEF
(datasheet 5.9.5.1: 0xffffded3, 0x10210142, 0x000001ff, 0x00000000,
0xab123579), which lands at flash+0x44 -- inside the boot ROM's 4 kB scan
window -- and, having no entry-point item, makes the bootrom enter via the
Cortex-M vector table the compiler already generates (SP at +0, reset at
+4). No other boot work was needed: no clock setup (DMA runs on the boot
clocks), no SRAM ungating, no startup_init. Flashed via picotool (BOOTSEL),
validated over SWD with the Raspberry Pi Debug Probe + the raspberrypi
openocd fork (homebrew openocd 0.12 has no rp2350.cfg; built from source at
/tmp/rpi-openocd). First flash worked: DST holds the DMA-copied ramp
(b0 b1 b2 b3), COPY_OK shows both ownership windows ran (the busy-wait
blocking acquire AND the INTR-guarded poll), ALIVE advances (~30M/s). The
full pipeline -- IMAGE_DEF boot, .data/.bss init, clear-to-enable RESETS,
handoff release, polarity-guarded reclaim -- validated on second-vendor
silicon. Pinned by `boot_block_words_emitted_after_vector_table` /
`boot_block_bad_word_is_error` (target.rs).

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
2. `>> 2`: gone -- handoffs write the full byte address to the register, whose
   reserved low bits the hardware ignores (no shift to get wrong).
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
- **`enabled_by` checking**: DONE (E609, presence). An agent that is programmed
  (a handoff register written) must have its `enabled_by` clock-gate registers
  set somewhere in the program, else the writes hit a gated peripheral and are
  dropped. Whole-program presence check (`region.rs::check_agent_enables`), sound
  with no false positives; a `= false`/`0` write does not count as enabling, and
  an `enabled_by` path that resolves to no real register is itself E609. The
  *ordering* refinement (enabled *before* the handoff on every path) needs the
  inter-procedural call-graph analysis `stack::analyze` already builds -- a
  follow-up, deferred until presence proves insufficient.
- **Clock-stomp guard**: DONE (E610, `region.rs::check_agent_clock_stomp`). A
  *disabling* write (`= false`/`0`) to one of an agent's `enabled_by` clock gates
  from a module that does not own the agent is rejected -- a stranger gating an
  agent's clock off silently stops it. This came out of dogfooding the modules
  layer (a second clock-touching module): `owns` is an *exclusivity* primitive,
  but clock enables are a *shared, idempotent set-to-1* resource, so the wrong
  tool. The bug class is the disabling direction, and the model already had the
  pieces -- `enabled_by` (the agent->clock-bit map) and `is_disabling` (the `=0`
  detector) plus the per-agent owner set from `owns`. Only fires when the agent
  has a declared owner (the baseline for "stranger"); the owner may still gate
  its own clock (deinit). Scoped to agent clocks for now; a general
  peripheral->clock map (and the enabling-direction "touch a clock you don't
  use" lint) are the generalizations, deferred. See the Stage-1 dogfood notes.

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
- **Handoff write hook = the peripheral-register store**; the reachability
  assert goes there (verify mode). The assume goes at the address-of site.

### Slice 0 -- Target physics (no language change)

`target.rs` only. Extend the section-dispatch loop (`:78-127`, template:
`[startup]`/`[interrupts]`) with `[mem.*]`, `[agent.*]`, `[region.*]`. New
structs `MemBlock{base,size}`, `Agent{kind,reach,cached,access,handoffs,
enabled_by}`, `Region{mem,agents}`. Handoff parse is `Peripheral.REGISTER` plus
optional `align N` (no encoding). Self-consistency at parse time (fail
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

### Slice 3 -- Handoff encoding (removed 2026-06-09)

Originally the IR emitter shifted handoff *field* writes: for a `word_addr`
handoff, source wrote a byte address, the compiler inserted `lshr val, 2`, and
the SVD field's bit-2 position re-aligned it via `(addr >> 2) << 2`, with E606
rejecting a hand-written `>> 2`. This is gone. Handoffs are now register-level
writes of the full byte address (see the 2026-06-09 note at the top): writing
the whole register instead of the `[31:2]` field drops both the `>> 2` and the
field's `<< 2`, so the encoding axis (`encode_word_addr_handoff`,
`set_word_addr_handoffs`), E606, and the `HandoffEncoding` enum were all deleted.
The handoff write is now a single `store`.

Proof: `exec_handoff_full_addr` round-trips a 4-aligned address through a
RAM-backed fake peripheral under QEMU at -O0/-O2 (a stray `>> 2` or `<< 2` would
corrupt the value). The verify path stores the same address, so IKOS sees the
same IR (slice 4).

### Slice 4 -- Verify obligations (done)

Verify mode now auto-emits the probe's hand-written instrumentation. The verify
emitter is threaded (in `verify::verify`, which has program + target) with three
maps: `region_addr_ranges` (placed static -> its region's mem block range) and
`handoff_reach_bounds` (handoff register path -> the owning agent's reach
bounding range).

- **Assume at the address-of site.** At the `ptr -> int` cast, if the operand is
  `&X` / `&mut X` for a region-placed static, emit `assume(region.lo <= addr <
  region.hi)` on the ptrtoint result. This is where the probe proved it has to
  go (ptrtoints of one global do not unify across functions), so the fact
  propagates through `desc_addr()` calls/returns/arithmetic to the obligation.
- **Assert at the handoff write.** On the stored byte address, emit
  `assert(reach.lo <= value < reach.hi)` via
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

Blockers before retirement: (1) design in-memory handoffs -- DONE; (2) carry
`@dma`'s Move-typing / index-read safety once placement moves to `in <region>`
-- DONE (derived-Move, below); (3) port the example additively
(`@dma ... in dma_shared`, ETH agent + handoffs in the target -- behavior-
preserving since `(desc>>2)<<2 == (desc)>>2<<2` after auto-encode) with hardware
validation; then delete `Type::Dma`.

**Port (blocker 3), done and hardware-validated.** `stm32h723zg.target` now
splits the 32K D2 SRAM into a working `sram` block and a `dma_pool` block, and
declares the ETH DMA agent with the four descriptor handoffs (then `word_addr`,
since superseded); `eth_dma.bml` adds `owns Ethernet_DMA`, places the four
buffers `in dma_shared`, and drops the six hand-written `>> 2` (at the time the
compiler reinserted them). Verified
behavior-preserving before flashing: the controller IR diff is *only* the
buffers gaining `.region.dma_shared` plus the identical `(desc>>2)<<2`
re-encoding; the linked buffers land in `dma_pool` (0x30007000, D2 SRAM,
32-aligned). On the NUCLEO-H723ZG, heartbeats still transmit -- captured frames
(EtherType 0x88B5, monotonic seq) confirm TX.

**`@dma` annotation retired from the example.** Dropping `@dma` from the four
buffers (they are placed `in dma_shared`, which derived-Move covers) produces
*byte-identical IR* -- the example is now regions-native with no `@dma` adjective.

Correction to the original plan: the carrier type is NOT deleted. It is
derived-Move's internal carrier, and `@dma` stays a valid explicit annotation
for cases derived-Move does not cover (non-region statics, scalars), still
exercised by the borrow/index-read fixtures. "Retiring `@dma`" means retiring the
*annotation from region code*, which is done.

Follow-up (naming): the carrier was renamed `Type::Dma` -> `Type::AgentShared`
and unified with `Type::External`, which was an identical type (same Move
semantics, same E326 index-read block -- they differed only by name and which
keyword produced them). The agent kind is not part of type identity, so one
carrier suffices; `@dma` and `@external` stay as distinct keywords (different
intent) both resolving to `Type::AgentShared`. This also fixes a misleading
diagnostic: a region static that never wrote `@dma` now reads as
`agent-shared(...)` in E326, not `Dma(...)`. The tradeoff: a type error can no
longer show whether `@dma` or `@external` was written -- acceptable unless the
two ever need to diverge (e.g. external masters getting different cache
treatment), which would re-split the type.

**Descriptor-struct refactor done and hardware-validated.** `TX_DESC`/`RX_DESC`
are now `[TxDesc;2]`/`[RxDesc;2]` (@repr(packed), 16 bytes each, pinned by
`comptime_assert(sizeof == 16)`); DES0 is an `addr in dma_shared` buffer-pointer
field (a typed in-memory handoff -- the verify obligation + E608 now apply to the
real descriptors). Layout is byte-identical (same 32-byte ring, same field
offsets, same linked addresses), and the IR confirms the field stores land at
the same offsets with the same values; the RX write-back read path keeps the raw
`*u32` (`rx_desc_get32`). Re-flashed: TX heartbeats stream (monotonic seq), and a
debug readback shows RX working too (`RX_PACKET_COUNT` advancing,
`RX_LAST_ETHERTYPE = 0x0800` parsed). The ETH driver is now fully regions-native:
typed descriptors, checked handoffs, no `@dma`.

**Derived-Move (blocker 2), done.** Pinned the actual mechanism empirically:
`@dma`'s load-bearing property is the *index-read restriction*, enforced as
`E326` in `checker.rs::index_element_type` -- the read path accepts only
`Array`/`Ptr`/views, so a `Dma(Array(..))` falls through to "cannot index". The
write path unwraps the `Dma` first (`lvalue_base_info` does `sym.ty.inner()`),
so `BUF[i] = x` is legal while `let v = BUF[i]` is not. That asymmetry is the
protection (software must not alias memory it has handed to an agent). It is
*not* a borrow-checker Move, as first assumed.

Placement is orthogonal to type, so `[u32;N] in dma_shared` would be a bare
`Array` and the read protection would vanish. `region.rs::apply_derived_move`
re-establishes it (usage dictates declaration): after resolution, before the
checker, it wraps an *array* static placed `in R` in `Type::AgentShared` when `R`'s mem
is operated on by a concurrently-mutating agent (`AgentKind::Dma`/`External`;
CPU/Debug regions stay plain `Array`). The existing `E326` machinery then
applies unchanged -- no new checks, no hand-written `@dma`. Scoped to arrays
because E326 is an indexing restriction and agent-shared memory holds
buffers/descriptors.

Pinned green by three tests: `dma_index_read.bml` (hand-written `@dma` rejects
the rvalue read, E326), `region_index_read.bml` (the same array placed in a
DMA region is rejected identically -- the protection now comes from placement),
and `cpu_region_index_read.bml` (a CPU-only region stays freely indexable).
Reading agent-shared memory now uses the raw-`*u32`-view idiom (as `eth_dma.bml`
already does, and as `exec/region_placement.bml` was updated to).

### Deferred

In-memory handoffs (`addr in <region>` fields) -- separate design doc first,
absorbs the descriptor-struct refactor. HIL manifest track -- separate.

### Ordering rationale

0 and 1 are independent of source semantics and useful immediately (placement
+ reach errors on the live board). 2 gates 3 and 4. 3 (codegen, proved by
exec) and 4 (verify, proved by IKOS) are separable -- a bug in one does not
block the other. 5 is cleanup once 1-4 subsume `@dma`. Biggest open risk: the
cross-module visibility confirmation in slice 2.
