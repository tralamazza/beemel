# 05 - Interrupts and Contexts

An interrupt handler runs at a higher priority than your main loop, can preempt
it between any two instructions, and often shares state with it. In C that's a
minefield: forget to mask interrupts around a shared access and you get a torn
read; call a blocking thread routine from an ISR and you deadlock; touch a
register from the wrong context and nothing warns you.

BML makes the *execution context* a property of every function and every piece of
shared storage, then checks -- at compile time -- who may call whom and who may
touch what. This tutorial covers `@isr`/`@context`, how a handler is wired into
the vector table, the call-graph rules, and the storage annotations
(`@exclusive`, `@shared`) plus the `claim` window that make sharing sound.

## Declaring an interrupt handler

An ISR is a normal function with an `@isr` annotation:

```bml
fn usart1_isr() @isr("USART1", priority = 2) { /* ... */ }
```

`@isr("USART1", priority = 2)` does two things: it marks the function as an
interrupt handler at NVIC priority 2, and it claims the vector-table slot named
`USART1`. The target file's `[interrupts]` section maps that label to the chip's
IRQ number:

```
[interrupts]
USART1 = 37
TIM2 = 28
```

The compiler places the handler at the right vector-table slot for you. Build the
above and the generated `@vector_table` puts `usart1_isr` at index **53** -- the
16 core entries (initial SP, reset, the system exceptions) plus IRQ 37. You never
write the vector table or an `extern "C"` symbol with a magic name.

An unlabeled `@isr(priority = N)` is allowed too: it fills the first free
external-interrupt slot (IRQ 0, then IRQ 1, ...) in declaration order. In practice
you **label** real handlers so each lands at its chip's actual IRQ; reach for the
unlabeled form when you only need an ISR-priority *context* to exist -- to derive a
`@shared` ceiling, exercise the context rules, or write a demo whose handler never
actually fires (as in this tutorial's example below), where the slot is
irrelevant.

Priority follows the ARM convention: **lower number = higher priority**, so
`priority = 0` preempts `priority = 2`.

## Contexts

Every function has a context that says what priority it runs at:

| Annotation | Context | Meaning |
|------------|---------|---------|
| `@context(thread)` | thread (255) | the main loop; lowest priority |
| `@isr(priority = N)` | ISR at N | runs at NVIC priority N |
| *(none)* | `Any` | callable from any context |

`Any` is the default. An unannotated helper can be called from thread code and
from ISRs; the compiler propagates the *caller's* context into it, so a helper
reached from an ISR is checked as if it ran at ISR priority (an `Any` hop can't
launder an access -- see the rules below).

## The call-graph rules

The core rule: **thread code and ISRs may not call each other.**

```bml
fn helper() @context(thread) { /* uses thread-only resources */ }

fn tick() @isr(priority = 1) {
    helper();          // error[E403]: cannot call `helper`
}                      //   (requires @context(thread)) from ISR `tick`
```

```bml
fn main() @context(thread) {
    tick();            // error[E403]: cannot call `tick`
}                      //   (ISR at priority 1) from thread context `main`
```

Both directions are `E403`. An ISR calling a thread routine is almost always a
bug (the routine may block or use a resource only the thread owns); a thread
calling an ISR directly bypasses the NVIC. Handlers are invoked by hardware, not
by you.

> **From C:** there's no convention to remember (`void USART1_IRQHandler(void)`
> matched by name) and no way to accidentally call a handler or mix contexts --
> the call graph is checked.
>
> **From Rust:** contexts are the embedded analog of `Send`/`Sync` boundaries,
> but specialized to interrupt priority and enforced without a borrow checker.

## Sharing state safely

A module-level `var` is, by default, **thread-only**. Touch it from an ISR and
the compiler stops you:

```bml
var g: u32;                       // unannotated -> thread-only
fn tick() @isr(priority = 1) {
    g = 1;                        // error[E404]: `g` is thread-only;
}                                 //   cannot access from ISR `tick`
```

To share data you say *how* it's shared. Two annotations cover the common cases.

**`@exclusive(fn)`** -- single-owner storage. Only `fn` may access it; anyone else
is `E401`. Use it for a buffer one ISR (or one task) owns outright -- no masking
needed, because nothing else touches it.

```bml
var rx_buf: [u8; 64] @exclusive(usart1_isr);   // only usart1_isr may touch it
```

**`@shared`** -- accessed from more than one context. The compiler automatically
wraps each access in a critical section, so you never hand-mask interrupts:

```bml
var counter: u32 @shared;

fn tick() @isr(priority = 2) {
    var c: u32 = counter;        // read snapshots into a plain local
    counter = c + 1;             // write back
}

fn main() @context(thread) {
    counter = 41;
    var c: u32 = counter;
    counter = c + 1;             // thread access gets an auto critical section
}
```

Two things to notice. First, each `@shared` access is its own guarded load/store,
so you snapshot into a plain `u32` to do arithmetic (a bare `counter + 1` is a
type error -- the value is still storage-qualified until you read it out).
Second, you wrote no masking: on this v7-M target the compiler raises `BASEPRI`
around the access and restores it (on v6-M it uses `cpsid i`/`cpsie i`).

### The ceiling protocol

How high does the mask go? `@shared` derives a **priority ceiling**: the highest
priority (lowest ARM number) among all the contexts that access the static. The
top accessor -- the one *at* the ceiling -- accesses directly, because nothing
that also touches the data can preempt it; every lower-priority accessor (thread
included) takes a critical section that masks up to the ceiling. You can pin the
number explicitly with `@shared(ceiling = N)` -- and then an accessor that
*outranks* the pin is an error:

```bml
var c: u32 @shared(ceiling = 2);
fn hi() @isr(priority = 0) {
    c = 1;        // error[E402]: @shared(ceiling=2) but current priority is 0
}                 //   (lower = higher priority); raise the ceiling to 0
```

The pin is a claim about the highest-priority accessor; `E402` means the code
disagrees with it. Bare `@shared` (no pin) can't hit `E402` -- the ceiling is
derived from actual usage.

## `claim` -- a window over shared memory

Per-access critical sections make each single access atomic, but a *multi-word*
operation (summing a log, snapshotting a struct) can still be torn between
accesses -- and building a `view` over a `@shared` array is rejected outright
(`E405`), because the view's pointer accesses would bypass the per-access mask.

`claim X { ... }` solves both: one mask pair around the whole block, inside which
`X` is its plain inner type -- views and indexing allowed, and the per-access
sections suppressed (the window already covers everything):

```bml
var LOG: [u32; 4] @shared;

fn drain() -> u32 @context(thread) {
    var sum: u32 = 0;
    claim LOG {
        const v = view(LOG);                 // a view is fine inside the window
        for i: u32 in 0 upto 4 { sum = sum + v[i]; }
    }
    return sum;                              // value copied out; safe
}
```

Restrictions (`E614`): the target must be a `@shared` static, and the body may not
call functions or escape the window (`return`, or `break`/`continue` of an outer
loop) -- a callee's own critical section would reopen the mask early, and an escape
would skip the restore. Copying *values* out of the window is the whole point and
stays legal; letting a view over the claimed buffer escape is `E616`.

## `@naked`

For a handler that needs full control of its prologue/epilogue (a context
switcher, a custom trampoline), `@naked` emits no compiler-generated
prologue/epilogue or default return -- you write it all in `asm`. It's the escape
hatch; the annotations above cover ordinary handlers.

## Put it together and run it

This self-test exercises a `@shared` counter (auto critical section) and a `claim`
window, and prints `PASS`. It also *declares* an ISR -- wired into the vector
table -- so you can see the whole shape compile:

```bml
fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

var counter: u32 @shared;
var LOG: [u32; 4] @shared;

fn tick() @isr(priority = 2) {
    var c: u32 = counter;
    counter = c + 1;
}

fn main() @context(thread) {
    var pass: b1 = true;

    counter = 41;
    var c: u32 = counter;
    counter = c + 1;
    var got: u32 = counter;
    if got != 42 { pass = false; }

    var sum: u32 = 0;
    claim LOG {
        LOG[0] = 10; LOG[1] = 20; LOG[2] = 30; LOG[3] = 40;
        const v = view(LOG);
        for i: u32 in 0 upto 4 { sum = sum + v[i]; }
    }
    if sum != 100 { pass = false; }

    if pass { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

Build and run it with the toolchain from tutorial 01 (any target works -- the
exec `qemu.target` is handy):

```sh
bml build --target qemu.target irq.bml
ld.lld -T irq.ld irq.o -o irq.elf
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel irq.elf
# -> PASS
```

As in tutorial 03, the honest caveat: `tick` is wired into the vector table but
**does not fire** under QEMU here (nothing pends the interrupt). What this proves
is the thread-side lowering -- the auto critical section around `@shared`, and the
`claim` window -- producing correct values; real preemption is a hardware
exercise. Inspect `irq.ll` and you'll see the `BASEPRI` save/restore the compiler
inserted around the shared accesses, with no masking in your source.

## Next

[Tutorial 06 - Data: Structs, Enums, Modules](06-structs-enums-modules.md):
user-defined types -- structs with explicit layout and visible padding,
`@repr(C)`/`@repr(packed)`, field endianness, enums and `match`, the Move/Copy
rule, and splitting a program across modules with `import`/`export`.
