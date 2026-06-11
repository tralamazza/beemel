# The regions & agents model

Whole-system memory correctness on MCUs as a checked property. The compiler
learns which hardware actors ("agents") touch memory, what each can reach,
and where software hands addresses to them ("handoffs"). Placement,
reachability, sizing, and synchronization bugs become compile errors or
discharged verification obligations instead of silent board lockups.

This is the synthesized, current-state description. The model was built
empirically -- every check below was driven by a concrete failure observed
or provoked on hardware (NUCLEO-H723ZG, Pico 2 W, micro:bit v1) -- and the
chronological slice history lives in git (`git log --grep "feat:"`).

## Founding failure modes

All real, from the first ETH driver on the H723 bench:

1. **Unreachable placement.** Moving `TX_DESC` to DTCM compiled, linked, and
   produced a board that never transmits -- nothing knew the bus topology.
2. **Hand-written address encoding.** `... = desc0 >> 2` because an SVD
   field starts at bit 2; forgetting or doubling the shift was undetectable.
   (Resolved by construction: handoffs are register-level writes of the full
   byte address -- the encoding axis no longer exists.)
3. **Unchecked cache discipline.** `dmb`-only sync was sound only because
   D-cache happened to be off; enabling it would break RX silently.
4. **`@dma` over-promised.** Documented as volatile, implemented as a plain
   global -- a placement hint pretending to be a contract.
5. **Address juggling.** Descriptor index arithmetic was exactly what IKOS
   flagged, with no structure to prove it against.

The common shape: memory correctness on an MCU is a property of the *system*
(CPU + DMA engines + caches + bus matrix), but a compiler normally models
only the CPU.

## The agent model

**An agent is anything that touches memory on its own initiative.**
Three-question test, all must hold:

1. Does it initiate memory accesses itself (not just decode them)?
2. Does it act concurrently, on its own clock, once set up?
3. Does it have its own view of memory -- reach, caching, permissions?

Every pair of agents is a potential disagreement about memory: visibility
(cache vs no cache), ordering, staleness, permission. The compiler referees
these at compile time where possible and emits verification obligations
where not.

| kind       | example                  | binding to software   | who answers for its accesses            |
|------------|--------------------------|-----------------------|------------------------------------------|
| `cpu`      | cm7, RP2350 core1        | all modules (implicit); secondary cores via `entry =` | the compiler -- it emits every load/store |
| `dma`      | ETH DMA, MDMA, RP2350 DMA, nRF EasyDMA engines | module owning handoffs | that module, at handoff write sites |
| `debug`    | SWD probe (AHB-AP)       | none (built-in)       | host-side harness. Deliberately inert to the concurrency checks: a halted-CPU prober is not a runtime mutator |
| `external` | other-vendor firmware    | none                  | nobody -- channels only                  |

A `cpu` agent's accesses are fully visible in the IR, so its rules are
per-access checks. A `dma` agent's accesses are invisible -- the compiler
only sees the address flowing into a register. Handoffs recover, for opaque
agents, the visibility the compiler gets for free on `cpu` agents.

**Non-agents:** contexts (thread/ISR) are scheduling identities *within* a
cpu agent -- same reach, same cache; the `@context` machinery is orthogonal.
The RP2350 PIO executes code but has zero memory reach (fails question 3).
TrustZone security states are attribute pairs on a cpu agent, not kinds.

Three vendors validated the shape: ST's central crossbar controllers (H723:
ETH, MDMA with software-selected ports), Raspberry Pi's per-channel
controller (RP2350), and Nordic's per-peripheral fixed-block engines (nRF51
ECB -- every capable peripheral is its own bus master).

## Design principles

- **Usage dictates declaration.** Only declare what usage cannot express:
  *physics* (the bus matrix, in the target file) and *exclusivity* (`owns`,
  a claim about other modules' absence). Everything else is derived --
  ceilings from accessor sets, Move-typing from placement, ISR cores from
  where their enable is written, port requirements from where addresses
  land. Optional clauses act as pins, never as the source of truth.
- **The agent contract is a closed schema.** Every target key answers one of
  six questions about a bus master (may it run / where can it touch / which
  buffer / how much / when done / what code) -- the admission rule and key
  table live in `language.md` ("The agent contract"). Cross-vendor
  falsification added zero new questions.
- **Layering.** Chip target files are written once and shipped with the
  compiler -- verbose is fine. Project target files stay small (regions,
  reach claims, an `entry`). `include = <chip>.target` composes them
  (key-level merge, re-opening a section resumes it, everything overridable).
- **Failure-driven vocabulary.** No key or check exists speculatively; each
  was added when silicon or a probe demonstrated the hole. The one removal
  rule: a key nothing consumes implies a guarantee nothing checks (the
  `access = read` key was removed on these grounds; its design note: the
  H7's LTDC is the one intrinsically read-only master, and when such an
  agent appears, `access = read` should return and relax derived-Move for
  its region -- a framebuffer the CPU produces wants its pixels read back).

## Layer 1: physics (target file)

```ini
arch = armv7em
cpu = cortex-m7
priority_bits = 4
data_block = sram          # which mem block hosts .data/.bss/.stack

[mem.flash]
base = 0x08000000
size = 1M

[mem.dma_pool]
base = 0x30007000
size = 4K
cacheable = false          # enforced via generated MPU region (see Layer 2)

[agent.mdma]
kind = dma
reach = dma_pool, dtcm     # project claim, cross-checked against `bus`
bus = axi: 0x08000000..0x08100000, 0x30000000..0x30008000, ahbs: 0x00000000..0x00040000, 0x20000000..0x20020000
enabled_by = RCC.C1_AHB3ENR.MDMAEN

[agent.mdma.ch0]           # one transaction channel of the controller
completes_by = MDMA.MDMA_C0ISR.CTCIF0
extent = MDMA.MDMA_C0BNDTR.BNDT
handoff = MDMA.MDMA_C0SAR port_by MDMA.MDMA_C0TBR.SBUS ahbs
handoff = MDMA.MDMA_C0DAR port_by MDMA.MDMA_C0TBR.DBUS ahbs

[region.dma_shared]
mem = dma_pool
agents = mdma              # cpu agent always implicitly included

[interrupts]
TIM2 = 28
```

Key reference (agent level):

- `reach = <blocks>` -- the project's claim of what the agent touches.
  Cross-checked at target load against `bus` windows when declared.
- `bus = [tag:] lo..hi, ...` -- transcription of the reference manual's
  bus-master-to-slave table; the union over the agent's master ports
  (catches what NO port can address). Tags mark software-selected ports.
- `enabled_by = [!]P.R.F, ...` -- clock/reset gates. `!` = clear-to-enable
  (the nRF/RP2350 RESETS style). Drives E609 (enable presence before the
  agent is programmed) and E610 (a non-owning module disabling -- or for
  `!`, re-asserting -- the gate).
- `entry = <fn>` (cpu agents) -- the secondary core's entry function;
  pinned to its core for core-reach, exempt from E408's address-of rule
  (the launch hands the address to hardware), and its prologue re-programs
  the banked NVIC IPRs (a secondary core never runs the reset handler).
- `spinlock_base = <addr>` / `spinlock_count = N` (top level) -- hardware
  spinlock physics (read = try-claim 0 = held, write = release; the RP2350
  SIO bank). Required for cross-core `@shared` (E615's relaxation).

Channel level (`[agent.NAME.CHANNEL]`; transaction keys written directly in
the agent section form an implicit default channel, so single-channel
agents stay flat):

- `handoff = P.R [align N] [port_by P.R.F TAG]` -- a register whose written
  value the agent dereferences. The full byte address is written verbatim
  (dedicated address registers ignore their reserved low bits -- no
  encoding, no shift to get wrong). `port_by` declares the software port
  select: an address behind a TAG-only bus window requires the field set
  (E612); behind no tagged window, a definite set is a misroute.
  Lowering: every store to a declared handoff register is followed by a
  derived `dsb` -- COMPLETION, not just ordering. Arming an agent is a
  posted Device write; one left in flight while the bus stays busy was an
  observed imprecise-BusFault source on silicon (H723 ETH tail pointers,
  2026-06-11; see the example's BRINGUP.md for the bisection). A manual
  `dmb` before the store still orders the descriptor publish; the
  completion barrier after it is the compiler's job now.
- `completes_by = [!]P.R.F, ...` -- the transfer-complete signal. Declaring
  it activates the sound-reclaim guard (E611). `!` = done-when-clear (the
  busy-high style: `completes_by = !DMA.CH0_CTRL_TRIG.BUSY`).
- `extent = P.R.F [xN] [when P.R.F = V]` -- the transfer-count field, N
  bytes per count unit. `when` ties the multiplier to the unit-select field
  that makes it true (E618: arming without establishing exactly V is an
  error). `extent = N` (a bare integer) is the fixed-block form for engines
  with no count register (nRF ECB walks exactly 48 bytes): the obligation
  moves to the delivery -- a buffer handed to the channel must be >= N
  bytes (E619).

Other sections: `[interrupts]` (label -> IRQ number), `[startup]` (MMIO
RMW-OR writes in the reset handler before .data/.bss -- the CMSIS
SystemInit slot), `[boot_block]` (literal words emitted after the vector
table -- the RP2350's 5-word Arm IMAGE_DEF; chip-agnostic mechanism).

Register paths resolve against the SVD modules the program imports;
unresolvable paths are build errors. Raw addresses in target files were
rejected as unreviewable.

## Layer 2: policy (regions)

A region names a mem block and lists the agents that share it.

- **Reach check:** the region's memory must lie within every listed agent's
  reach -- the DTCM footgun dies at target load, before any source.
- **Cache discipline, detected and enforced:** a cacheable block shared by a
  cached CPU and a non-snooping dma/external agent is rejected; declaring
  the block `cacheable = false` *generates* an MPU non-cacheable region in
  the reset handler (PMSAv7 RNR/RBAR/RASR on M4/M7 -- power-of-two,
  size-aligned; PMSAv8 MAIR0/RBAR/RLAR on M33 -- 32-byte granularity). The
  H723 example runs with D-cache ON and the generated MPU keeping ETH/MDMA
  coherent; the Pico read back the PMSAv8 registers exactly as emitted.
- **Alignment as derived physics:** a static placed `in R` is floored to
  `cache_line_size(cpu)` alignment when R's mem is cacheable and shared
  with a non-coherent agent (the RM confirms the ETH DMA itself imposes no
  buffer alignment -- the 32 was always the M7 cache line).

## Layer 3: software binding (source)

- **`owns P` / `owns P.R`** -- module-exclusive register access (E604 on a
  cross-module conflict). Register granularity matters: three modules
  legitimately share `Ethernet_MAC`. Handoff registers are
  ownership-required (E605) -- without that rule, exclusivity evaporates
  for exactly the registers that matter. The *drives* relation (module M
  drives agent A iff M owns one of A's handoff registers) is derived, never
  declared.
- **`in <region>`** -- placement as a checked claim. The generated linker
  script places the symbol; membership becomes a fact for handoff checking.
  Region memory is NOBITS: not zeroed, not loaded, so initializers are
  rejected (E601) and `@section` cannot co-apply (E602).
- **Derived-Move:** an array placed in a region a dma/external agent
  mutates is wrapped in `Type::AgentShared` at resolution
  (`region.rs::apply_derived_move`) -- the index-READ restriction (E326)
  comes from placement, with index-writes still legal (fill before
  release). `@dma`/`@external` remain as explicit annotations for
  non-region cases; both resolve to the same carrier.
- **`addr in R` struct fields** -- in-memory handoffs (descriptor buffer
  pointers). Layout-identical to `u32`; a write asserts the value lies in
  R (verify), E607 checks the region exists, E608 checks transitive reach
  (a descriptor delivered to an agent must not carry a field pointing into
  a region that agent cannot reach).
- **`@extent(addr_field [, xN])` struct-field attribute** -- a length field
  arms the buffer delivered through its `addr in R` sibling (the ETH TX
  control word). Declaration sanity is E617; the check itself is a verify
  obligation, same capacity-shadow encoding as `extent`.

## Ownership windows

The unification thesis, validated end to end: the ceiling protocol (CPU
mutual exclusion) and release/reclaim (async agent handshake) are one
concept -- *region ownership* -- with the transfer mechanism derived from
the sharer set.

- **Release** = the handoff write. **Reclaim** = `reclaim(BUF)`, yielding a
  bounds-checked view over agent-shared memory; the explicit
  handshake-acknowledged escape from E326. A plain `view()` over
  agent-shared memory is rejected (E335, points at reclaim).
- **E611, the guard engine.** When the buffer's channel declares
  `completes_by`, every `reclaim` must be control-dependent on observing
  the flag -- proven by span containment (lexical, no flow sensitivity).
  Accepted acquire forms: `if F { }` (try), `while !F {}` empty-body
  busy-wait (blocking; guards the rest of the block), `if !F { return; }`
  (early exit), completion predicates (`if done()`, where `done` returns
  the flag), compared forms (`F == true`, `!= 0`; `== 0`/`== false` for
  the negation -- `!= <nonzero>` is deliberately not recognized for wide
  fields), and both polarities throughout.
  Precision rules: a direct delivery (`P.R = &BUF`) associates the buffer
  with that register's *channel*, so the guard must be that channel's own
  flag (indirect deliveries keep the conservative region union); a release
  between guard and reclaim re-opens E611 (the observed completion covers
  the previous transfer); a guard re-observing a flag an earlier reclaim
  consumed, after a re-arm, needs a clearing write to the flag's own
  register in between (W1C staleness; first observations trusted,
  `!`-polarity busy flags exempt -- hardware-managed).
- **`claim X { }`** -- the masked window, the CPU-side reclaim: one
  mask pair (BASEPRI raised to the static's ceiling on v7-M so unrelated
  higher-priority ISRs keep running; cpsid/cpsie on v6-M or without a
  real ISR ceiling); inside, the `@shared` static is its inner type
  (views/index-reads legal) and per-access critical sections are
  suppressed. E614 rejects non-`@shared` targets, calls inside (a callee's
  CS would open the window early), and escapes (`return`/outer `break`).
- **Composition `@shared in R`** -- the carriers nest:
  `Shared(AgentShared(Array))`. Outside a claim everything is blocked
  including reclaim, so the masked window is required *by construction*;
  inside, the agent-shared rules compose with zero new logic. Idiom:
  `claim X { if <flag> { reclaim(X) } }`.
- **E616, scoped view lifetimes** -- a window's view must not outlive its
  justification: a view over the claimed static escaping the claim body
  (assignment to an outside binding, lexical taint through inner consts),
  a reclaimed view mentioned outside its guard span (mentions judged
  against the most recent binding of the name, so reuse/rebinds are fine),
  or used after the buffer was released back.
- **Cross-core `claim`** -- with declared spinlock physics, a cross-core
  `@shared` static is legal iff every access sits inside its claim window
  (E615 relaxes to require-window); the lowering adds spin-acquire after
  the mask and release before the restore. Spinning masked is sound: the
  holder is the other core. Silicon proof: tens of millions of contended
  windows on the Pico with zero in-window invariant violations.

## Multi-core

- A second `cpu` agent binds code via `entry = <fn>`. Core-reach propagates
  from roots (main + ISRs, declared entries pinned to their core) over the
  same mention edges as context propagation. A mutable static reachable
  from two cores is E615 unless it is cross-core `@shared` under claims.
- **Per-core NVIC:** the NVIC is banked, so an IRQ runs on whichever core
  enables it. A labeled `@isr`'s core(s) = the core-reach of the
  function(s) writing its ISER bit (recognized by address 0xE000E100..40,
  `1 << n`/OR values folded), an outer fixpoint with core-reach.
  Conservative fallbacks: no visible enable or unlabeled ISR -> core0
  (single-core programs unchanged); undecodable enable value -> all cores.
  The same program flips between E615 and legal on the enable's location
  alone.
- **Banked IPR grounding:** `@isr(priority=N)` is programmed into the NVIC
  by the generated reset handler (byte stores on ARMv7-M; composed whole-
  word stores on ARMv6-M, where byte IPR access is unpredictable) AND by
  every declared entry's prologue -- secondary cores never run the reset
  handler. ISER deliberately stays application code: priority is static
  physics, enable is runtime policy.
- The RP2350 bootrom launch handshake (PSM reset + FIFO sequence + echo
  verify + bounded-ack retry) is plain bml in the example.

## Context soundness (supporting machinery)

The ceiling protocol is only as good as its idea of who runs where:

- Call-graph context propagation: `Any` functions inherit their callers'
  contexts (fixpoint over mention edges; `&f` counts), so an Any hop
  cannot launder an ISR past E402/E404 or out of a derived ceiling.
- Pointer-call closure: address-taken Any functions (including via
  static/const initializers, which live in no function body) additionally
  inherit the contexts of every function performing an indirect call -- a
  stored function pointer travels invisibly, so the pointee is checked at
  every pointer-call site's context.
- Derived ceilings: bare `@shared` computes its ceiling from the accessor
  set; `@shared(ceiling=N)` is a pin, and an accessor outranking it is
  E402 ("the pin disagrees with usage").

## Verify integration (IKOS)

See `verify.md` for the pipeline and `ikos-setup.md` for the toolchain.
What the regions model adds, all verify-mode-only:

| Obligation | Emitted at | Discharged by |
|---|---|---|
| Provenance assume `addr in [block_lo, block_hi - sizeof(X)]` | `&X as u32` of a region-placed static | adopted fact (size-tightened so base+offset handoffs are provable) |
| Reach assert | handoff register write | the provenance assume, through calls/arithmetic |
| Region assert | `addr in R` field store | same |
| Extent: `count * N <= capacity` | extent-field write | capacity shadows: a direct `= &X as u32` delivery stores `sizeof(X)` into a per-handoff-register shadow global |
| Claim-aware havoc | `@shared` reads | reads of the *claimed* static inside its window are not havoc'd -- the mask/spinlock provides exactly that stability; the same read-back is V200 outside the window and proven inside |

Empirical IKOS facts the encodings are built on (measured, not assumed):

- The assume must sit at the **address-of site** -- two `ptrtoint`s of one
  global do not unify across functions.
- **No backward congruence narrowing**: alignment is unprovable
  symbolically; exact addresses (the compiler owns the linker script) are
  the alignment story.
- The interval domain is **non-relational**: a base/limit shadow pair is
  unprovable, but capacity-as-constant vs a count interval is trivial --
  the load-bearing choice in the extent encoding.
- Preempt shim soundness rule: a function only cannot preempt *itself*;
  being a writer of a static does not exempt its reads from other
  higher-priority writers.
- Wrap counters as `(x + 1) % N`, not `if x == N { 0 }` -- the equality
  form is unbounded after interval widening and poisons every address
  derived from the index.
- Helpers hide sizes: extent capacity detection needs the delivery to be a
  literal `&X as u32` at the handoff write.

## Error code index

| Code | Check |
|---|---|
| E326 | index-read of agent-shared memory (derived-Move; read through `reclaim`) |
| E335 | `view()` over agent-shared memory (use `reclaim`); reclaim of `@shared in R` outside `claim` |
| E600-E602 | region placement: unknown region; initializer on NOBITS region memory; `@section` + `in R` conflict |
| E603-E605 | `owns`: bad path; cross-module conflict; handoff register written without ownership |
| E607-E608 | `addr in R`: unknown region; descriptor delivered to an agent that cannot reach a field's region |
| E609-E610 | agent gating: enable presence (polarity-aware); clock stomp by a non-owner |
| E611 | reclaim guard: missing/wrong-polarity completion check, release between guard and reclaim, stale re-observation without a clearing write |
| E612 | software port select not established for where the handed-off address lives |
| E614 | `claim` misuse: non-`@shared` target, calls, escapes |
| E615 | mutable static reachable from multiple cores (relaxes to require-claim with spinlock physics) |
| E616 | a view outliving its justification window (claim escape, out-of-guard mention, use after release) |
| E617 | `@extent` declaration shape |
| E618 | extent unit cross-check (`when P.R.F = V` not established at arming) |
| E619 | fixed-block extent: delivered buffer smaller than the block |

(E606 and E613 were retired: the encoding axis no longer exists; `@shared
in R` now composes.)

## Trust & blind-spot register

What is still *trusted* (declared, not checked) or *lexically invisible*,
each recorded when found:

| Item | Class | Notes |
|---|---|---|
| `cacheable` on a mem block | trusted physics | a wrong value is at least visible silicon config (generated MPU), not silence |
| `bus` window transcription | trusted transcription | reach is checked against it; the transcription itself is read off the manual |
| Loop back-edge carries | lexical blind spot | a view/flag fact carried across one lexical guard by a loop back-edge (E611 staleness, E616) |
| Addresses cast to integers | provenance domain | `&X as u32` stashed and dereferenced later is the verify/IKOS domain, not the lexical windows |
| In-memory delivery association | precision | a descriptor's `addr in R` field delivery associates the descriptor, not the pointed-to buffer |
| Entry address reuse | trusted policy | a declared entry's address used as an ordinary callback dodges E408 |
| Computed ISER values | conservative smear | an undecodable enable assigns the ISR to all cores |
| System exceptions | unmodeled | SysTick/PendSV priorities live in SHPR, not modeled |
| `cleared_by` vocabulary | deferred | chips whose flag-clear is a separate register (H7 IFCR) get vocabulary when a double-observation idiom appears |
| Arm-then-deliver order | assumed | an extent count written before any handoff is unconstrained |
| Directly calling another core's entry | undetected | entries are pinned so the launcher's `&entry` does not poison the tree |
| Agent concurrency on reclaimed buffers (verify) | unmodeled | reclaim views load without havoc; the lexical E611/E616 windows are the guard |

## Hardware validation status

- **NUCLEO-H723ZG (M7):** ETH TX/RX with typed descriptors and D-cache ON
  under the generated MPU; MDMA DTCM copy through the software-selected
  AHBS port (the E612 story); the full claim/reclaim composition with the
  exact invariant LOG_SUM = 4*TICKS - 10; descriptor extents proven and
  confirmed by DMA write-back (OWN cleared, control = the proven length).
  Known driver gap: ETH bring-up is one-shot -- booting with the cable
  unplugged wedges the MAC (link-up recovery is the open example item).
- **Pico 2 W (2x M33):** IMAGE_DEF boot block, first-flash bring-up;
  PMSAv8 MPU readback; core1 launch handshake; cross-core claim with zero
  invariant violations over tens of millions of contended windows;
  per-core FIFO ISR with the banked-IPR prologue. Workflow: flash over
  SWD, then POWER-CYCLE -- debugger-initiated resets produce launch
  artifacts; per-core `reg pc` reads are unreliable.
- **micro:bit v1 (nRF51, M0):** AES ECB known-answer test through the full
  chain (handoff -> fixed extent -> guarded reclaim) on first flash;
  ARMv6-M word-composed IPR live; 1 Hz timer ISR exact. Caveat: DAPLink
  AUTO_RST resets the target on debugger attach -- early readbacks show
  fresh-boot state.
- In reserve: black-pill (F411 -- the bit-band hwaddrs silicon check),
  nRF52840 dongle (EasyDMA everywhere; needs SWD pads wired).

## Open items

- **Volatile lowering for agent-region access** (found on silicon
  2026-06-11): raw-pointer loads of agent-mutated memory compile to plain
  LLVM loads, so an OWN-bit spin loop was hoisted into an infinite `b .`
  -- the agent is a concurrent writer the optimizer cannot see. The same
  hazard sits under every raw-pointer read of DMA-written memory and is
  only kept latent by inlining luck. Candidate fixes: lower accesses with
  agent-region provenance as volatile, and/or lift the agent-shared
  index-read ban (its replacement, the raw-pointer detour, is exactly the
  unprotected idiom). Driver-level mitigation today: asm volatile loads in
  spin loops (eth_dma.bml tx_wait_idle).
- ETH link-up recovery in the H723 example driver.
- Deferred until a consumer appears: `addr` as a general (non-field) type;
  reads re-establishing the in-region fact; placement inference (`in` as
  pin); field-level `owns` (the RCC sharing story); `owns P except ...`;
  loadable/zeroed region sections; LSP "drives" hover; the HIL manifest
  track; TrustZone attribute pairs.
