# 08 - Regions and Agents (DMA safety)

This is BML's signature feature. Every tutorial so far has modeled one actor: the
CPU, whose every load and store the compiler can see. But a real MCU has *other*
actors -- DMA engines, a second core -- that touch memory concurrently, on their
own clock, and **invisibly** to the compiler. A normal compiler models only the
CPU, so memory bugs involving those actors become silent board lockups. BML makes
them compile errors.

The founding failure (from the first Ethernet driver on a NUCLEO-H723ZG): moving a
DMA descriptor into the wrong RAM bank *compiled, linked, and ran* -- and the
board simply never transmitted, because nothing in the toolchain knew the DMA
engine couldn't reach that bank. The regions/agents model exists so that bug, and
its cousins, can't happen.

> This tutorial is the gentle version. The full model -- multi-core, cache
> discipline, port routing, the complete obligation list -- is
> [doc/regions-agents.md](../regions-agents.md).

## An agent is anything that touches memory on its own

A DMA engine qualifies: it initiates accesses itself, runs concurrently once
armed, and has its own view of memory (its own bus reach). The CPU is an agent
too (the implicit one). The model is three layers.

### Layer 1 -- the agent (target file, physics)

You declare the hardware actor in the `.target` file: what it is, what memory it
can physically `reach`, the register you hand it a buffer through (`handoff`), and
the flag that says it's finished (`completes_by`):

```ini
[agent.dma]
kind = dma
reach = dma_pool                # which mem block(s) it can address
handoff = DMA.ADDR              # register it dereferences
completes_by = DMA.SR.DONE      # transfer-complete flag
```

`reach` is the bus topology -- the thing that was missing in the founding failure.

### Layer 2 -- the region (target file, policy)

A region names a mem block (tutorial 04's `[mem.*]`) and the agents that share it:

```ini
[mem.dma_pool]
base = 0x20001800
size = 0x800

[region.dma_pool]
mem = dma_pool
agents = dma                    # the cpu is always implicitly included
```

The compiler **cross-checks reach at target load**: if a region's memory isn't
inside every listed agent's `reach`, the target is rejected before a single line
of source compiles. Say you'd put a region `pool` in a `dtcm` block the DMA can't
reach:

```
Error parsing target: region `pool` is in mem `dtcm`, which agent `dma` cannot reach
```

That is the founding footgun (a buffer in unreachable memory), dead at the
earliest possible moment.

### Layer 3 -- placement (source)

You place a buffer in a region with `in <region>`:

```bml
var TX: [u32; 4] in dma_pool;   // lives in the dma_pool block, shared with the DMA
```

Region memory is uninitialized at boot (like `.bss`, but not even zeroed), so a
placed buffer takes **no initializer** (`error[E601]`) -- it's written before use,
exactly like a DMA buffer. The linker script (generated from the `[mem.*]` blocks)
puts `TX` at the region's address, distinct from where ordinary statics land.

## The access discipline

Placing `TX` in a region a DMA agent touches makes it **agent-shared**, and that
changes how software may touch it. The rule: *you must not read memory you may
have handed to an agent.*

- **Index-writes are allowed** -- you fill the buffer before handing it over:
  ```bml
  TX[0] = 0xAA;        // ok: preparing the buffer
  ```
- **Index-reads are blocked:**
  ```bml
  var x = TX[0];       // error[E326]: cannot index agent-shared memory
  ```
- **A plain `view` is blocked too:**
  ```bml
  const v = view(TX);  // error[E335]: ... the agent may still own it. Use reclaim(x)
  ```

The two halves of the handshake:

- **Release** = writing the buffer's address to the agent's handoff register. This
  hands ownership to the agent and arms it:
  ```bml
  DMA.ADDR = &TX as u32;       // release: the DMA now owns TX
  ```
- **Reclaim** = `reclaim(TX)` once the agent is done. It yields the same
  bounds-checked `view` that `view()` would -- but it marks that the ownership
  handshake happened, so it's the sanctioned escape from `E326`.

And reclaim must be **guarded** by observing the completion flag. Because the
target declared `completes_by = DMA.SR.DONE`, an unguarded reclaim is rejected:

```bml
const v = reclaim(TX);          // error[E611]: not guarded by a completion check
```

Guard it by testing the flag -- then reading the result is allowed:

```bml
if DMA.SR.DONE {
    const v = reclaim(TX);      // ok: the agent signalled done
    var first = v[0];           // now reading is safe
}
```

A `while !DMA.SR.DONE { }` busy-wait or an `if !DMA.SR.DONE { return; }` early
exit work too. The compiler proves the guard by lexical containment -- the reclaim
must sit inside a span that observed the flag.

So the whole DMA round-trip is now a checked protocol:

```
fill (index-write)  ->  release (handoff write)  ->  wait (completes_by)  ->  reclaim (guarded)  ->  read
```

Mis-reach, reading mid-transfer, reclaiming without waiting -- each is a compile
error, not a 2 a.m. logic-analyzer session.

> **From C:** there is no language-level equivalent. This is the class of bug --
> "the DMA wrote where I was reading," "the descriptor was in TCM the engine can't
> see" -- that you normally find with an oscilloscope. Here the buffer's *type*
> changes when you place it in a DMA region, and the compiler tracks the
> handshake.
>
> **From Rust:** it's ownership transfer applied to *hardware*: handing a buffer
> to the DMA is a move out of CPU-readable land; `reclaim` after the completion
> flag is the move back. Closer to a typestate protocol than to `Send`/`Sync`.

## `@dma` / `@external` -- agent-shared without a declared region

When you haven't declared a full `[region.*]` (quick bring-up, or a buffer an
external agent touches), the `@dma` and `@external` annotations mark a static as
agent-shared directly:

```bml
var BUF: [u8; 64] @dma;         // agent-shared: same E326/E335/reclaim rules
```

They resolve to the same carrier as region placement -- the access discipline is
identical. (Note `@dma` is *only* a placement/aliasing marker: it does **not**
make accesses volatile. Ordering and cache visibility toward the agent are still
yours -- `asm { dmb }` for ordering, non-cacheable placement for visibility. A
declared region with `cacheable = false` automates the latter by generating an MPU
region; see the full doc.)

## Run it

QEMU's `stm32vldiscovery` has no DMA engine, so we can't drive a real transfer --
but we *can* prove the parts that don't need one: a buffer placed in a region
lands at the region's address, fills correctly, and reads back. (The full transfer
on real silicon is the [NUCLEO-H723ZG example](../../bml/examples/nucleo-h723zg-ptp).)

Target (`dma.target`) -- a DMA pool region in the 8 KB QEMU SRAM:

```ini
arch = armv7m
cpu = cortex-m3
priority_bits = 4
vector_table_offset = 0x08000000
data_block = sram

[mem.flash]
base = 0x08000000
size = 64K
[mem.sram]
base = 0x20000000
size = 6K
[mem.dma_pool]
base = 0x20001800
size = 0x800

[agent.dma]
kind = dma
reach = dma_pool

[region.dma_pool]
mem = dma_pool
agents = dma
```

Program:

```bml
fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

var TX: [u32; 4] in dma_pool;   // agent-shared, no initializer
var ORDINARY: u32 = 7;

fn main() @context(thread) {
    var pass: b1 = true;

    // Placement: TX is in the dma_pool block; ORDINARY is in working RAM below it.
    const tx_addr:  u32 = &TX as u32;
    const ord_addr: u32 = &ORDINARY as u32;
    if tx_addr < 0x20001800 { pass = false; }
    if tx_addr >= 0x20002000 { pass = false; }
    if ord_addr >= 0x20001800 { pass = false; }

    // Index-writes are allowed (filling before handoff).
    TX[0] = 0xAA;
    TX[3] = 0xBB;

    // Read back through a raw pointer -- a plain `TX[0]` read would be E326.
    const txp = &TX as *u32;
    if txp[0] != 0xAA { pass = false; }
    if txp[3] != 0xBB { pass = false; }

    if pass { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

```sh
bml build --target dma.target dma.bml
ld.lld -T dma.ld dma.o -o dma.elf
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel dma.elf
# -> PASS
```

To see the access discipline itself, paste any of the blocked forms into `main`
and run `bml check` -- `var x = TX[0];` gives E326, `view(TX)` gives E335. The
errors *are* the feature.

## Next

[Tutorial 09 - Verifying with `bml verify`](09-verifying.md): the regions model
covers *placement and ownership*; the matching *sizing and provenance* properties
-- "the armed length fits the buffer," "the descriptor address really lands in the
region" -- are proved by the IKOS static analyzer. That's the last tool in the
box.
