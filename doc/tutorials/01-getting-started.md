# 01 - Getting Started

By the end of this tutorial you will have built the BML compiler, written a
self-contained program that blinks an LED, and watched it run in QEMU -- no
hardware required. Along the way you'll meet the two commands you'll use
constantly: `check` and `build`.

## What you need

The compiler is a Rust program; turning a `.bml` file into an ELF shells out to a
standard LLVM toolchain.

| Tool | Why |
|------|-----|
| Rust (stable) | builds the `bml` compiler (`rustup`) |
| `opt`, `llc`, `ld.lld` | `bml build` runs IR through these; any recent LLVM, on `PATH` |
| `qemu-system-arm` | runs the result without hardware |

> **From C:** there is no `gcc`/`make` step. `bml build` emits LLVM IR, optimizes
> it with `opt`, lowers it with `llc`, and links with `ld.lld`. You can see every
> intermediate with `--save-temps`.

**macOS (Homebrew).** `brew install llvm qemu`. The `llvm` formula is keg-only,
so Homebrew won't put it on your `PATH` -- add it yourself:

```sh
export PATH="$(brew --prefix llvm)/bin:$PATH"
```

**Debian/Ubuntu.** `apt install llvm lld qemu-system-arm` (`ld.lld` ships in the
`lld` package).

**Fedora.** `sudo dnf install llvm lld qemu-system-arm` (same split: `opt`/`llc`
are in `llvm`, `ld.lld` in `lld`).

(That's everything this tutorial needs.)

## Build the compiler

The compiler isn't on a package registry yet, so you build it from a clone of
this repository. Two ways, depending on whether you want it permanently on your
`PATH`.

**Install it (recommended).** From the repository root:

```sh
cargo install --path bml
```

This compiles the `bml` binary and copies it to `~/.cargo/bin/bml`, which `rustup`
already puts on your `PATH`. Now `bml` works from anywhere; re-run the same
command to update after a `git pull`.

> Do **not** run a bare `cargo install bml`. The name `bml` on crates.io belongs
> to an unrelated SNES-preservation tool, not this compiler -- you'd install the
> wrong program. The `--path` (or, once published, a namespaced) form is the only
> correct one.

**Or just build it.** If you'd rather not install anything:

```sh
cargo build --release        # produces ./target/release/bml
export PATH="$PWD/target/release:$PATH"
bml --help
```

Either way the rest of the tutorial assumes `bml` is runnable by name. (Without
the `PATH` export, replace `bml` with `cargo run --release --bin bml --`
everywhere.)

## The two commands

You'll reach for these constantly, from fastest to most thorough:

```sh
bml check file.bml                      # parse + type-check only; no output files
bml build --target T.target file.bml    # compile to an object + linker script
```

- **`check`** is the fast inner loop. It runs the lexer, parser, type checker,
  and the context/borrow rules, and prints diagnostics. No files are written. Run
  it constantly.
- **`build`** does everything `check` does, then emits LLVM IR, optimizes,
  lowers, and (with a `--target`) auto-generates a linker script. Output is a
  `.o` and a `.ld` next to the source.

(A later tutorial adds a third command for static verification. It needs extra
setup, so we leave it alone here.)

## Your first program

Create a file `blinky.bml`. We are targeting the QEMU `stm32vldiscovery`
machine, whose virtual board has an LED on **PC8**. The program is deliberately
self-contained: the peripheral registers it needs are declared inline, so there
is nothing to import.

```bml
// blinky.bml -- blink PC8 on the QEMU stm32vldiscovery board.
//
// Prints a '.' over semihosting on every toggle, so you see a heartbeat
// in the terminal even without the graphical board view.

// --- Peripherals: only the registers we touch ---
// These addresses and bit positions are the real STM32F103 layout.

peripheral RCC at 0x40021000 {
    reg APB2ENR offset 0x18 {
        field IOPCEN: b1 bit[4]      // GPIO port C clock enable
    }
}

peripheral GPIOC at 0x40011000 {
    reg CRH offset 0x04 {
        field MODE8: u32 bit[0..1]   // PC8 mode (2 = output, 2 MHz)
        field CNF8:  u32 bit[2..3]   // PC8 config (0 = push-pull)
    }
    reg ODR offset 0x0C {
        field ODR8: b1 bit[8]        // PC8 output level
    }
}

// --- A crude busy-wait. The empty asm block is a barrier that stops the
//     optimizer from deleting the otherwise side-effect-free loop. ---
const DELAY: u32 = 500000;

fn delay() {
    var count: u32 = 0;
    while count < DELAY {
        asm { }
        count = count + 1;
    }
}

// --- Semihosting heartbeat (don't worry about the asm yet; tutorial 05) ---
// The ARM semihosting ABI takes the operation in r0 and a parameter in r1. An
// asm block with no operand sections leaves the enclosing function's parameters
// in r0-r3, so we just forward them: op -> r0, param -> r1.
fn semihost_syscall(op: u32, param: u32) {
    asm { bkpt 0xAB }
}

// SYS_WRITE0 (0x04) prints a null-terminated string.
fn semihost_write0(msg: *u8) {
    semihost_syscall(0x04, msg as u32);
}

// --- Entry point. `main` runs in thread context (the lowest priority). ---
fn main() @context(thread) {
    RCC.APB2ENR.IOPCEN = 1;     // turn on the GPIOC clock

    GPIOC.CRH.MODE8 = 2;        // PC8 = output, 2 MHz
    GPIOC.CRH.CNF8  = 0;        // PC8 = push-pull

    loop {
        delay();

        // Toggle the pin by reading the current level and writing its inverse.
        if GPIOC.ODR.ODR8 {
            GPIOC.ODR.ODR8 = false;
        } else {
            GPIOC.ODR.ODR8 = true;
        }

        semihost_write0(".");
    }
}
```

A few things to notice now; each gets its own tutorial later:

- **`peripheral ... at 0x...`** declares memory-mapped hardware at a fixed
  address. Reading or writing `GPIOC.ODR.ODR8` is a *volatile* register access,
  but you never wrote `volatile` -- the compiler knows it is MMIO because of the
  declaration. Writing a single bit-field (`MODE8 = 2`) lowers to a
  read-modify-write of the whole register. (Tutorial 03.)
- **`field MODE8: u32 bit[0..1]`** names a 2-bit slice of a register with an
  explicit type. There are no magic numbers or shift masks in your code.
- **`fn main() @context(thread)`** tags the entry point with an *interrupt
  context*. Thread context is the lowest priority; ISRs are higher. The compiler
  uses these tags to police who may call whom and who may touch shared state.
  (Tutorial 05.)
- **`asm { }`** is an inline-assembly block. The empty one is a compiler barrier;
  the one in `semihost_write0` makes a real semihosting call. (Tutorials 03/05.)
- **`msg as u32`** is an explicit cast. BML has *no* implicit conversions, not
  even pointer-to-integer. (Tutorial 02.)

## The target file

`bml build` needs to know the chip: its instruction set, memory map, and a few
capability flags. That lives in a `.target` file. Create `stm32f103c8.target`:

```
# STM32F103C8 (Blue Pill) -- also what QEMU's stm32vldiscovery emulates.
arch = armv7m
cpu = cortex-m3
priority_bits = 4
has_bitband = true
flash_base = 0x08000000
flash_size = 64K
ram_base = 0x20000000
ram_size = 20K
vector_table_offset = 0x08000000
```

The compiler turns this into the right `llc` flags and an auto-generated linker
script (so flash and RAM end up at the right addresses). Tutorial 04 covers every
key; for now it's enough that `arch`/`cpu` pick the instruction set and the
`*_base`/`*_size` keys describe the memory map.

## Check it, build it, run it

First, the fast check:

```sh
bml check blinky.bml
```

No output means it type-checked. Try breaking something -- delete the `as u32`
in `semihost_write0` -- and you'll get a precise error with a code (here `E310`,
arithmetic type mismatch). Every diagnostic has a code you can look up in
[language.md](../language.md) section 12. Put the `as u32` back.

Now compile:

```sh
bml build --target stm32f103c8.target blinky.bml
```

This writes three files next to the source: `blinky.o` (the object),
`blinky.ll` (the LLVM IR, handy to inspect), and `blinky.ld` (the linker
script). It even prints the exact link command. Link the object into an ELF with
LLVM's linker:

```sh
ld.lld -T blinky.ld blinky.o -o blinky.elf
```

And run it headless in QEMU:

```sh
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel blinky.elf
```

You should see a fast, steady stream of dots -- one per loop iteration:

```
....................................................................
```

Each `.` is the `semihost_write0(".")` call at the bottom of the loop, so the
dots are a heartbeat proving the CPU is running your code. (In QEMU the busy-wait
runs far faster than on real silicon, so they fly by.) Quit with `Ctrl-A` then
`X`.

One honest caveat: QEMU's `stm32vldiscovery` emulates the Cortex-M3 core and
semihosting, but **not** the GPIO peripheral. So the LED won't light in QEMU and
reading a GPIO register back returns 0 -- which means the toggle's `if` always
takes the same branch under emulation (harmless here, correct on real silicon).
The heartbeat is your proof of life in QEMU; the actual LED blink is something you
confirm on real hardware in tutorial 04.

> If `qemu-system-arm` reports an unknown machine, your QEMU build may lack the
> STM32 boards; install a full `qemu-system-arm` (the `qemu-system-arm` package,
> not just `qemu-user`).

## What just happened

You wrote a program with no `volatile`, no `#define` register masks, no linker
script of your own, and no startup assembly -- yet it boots, runs, and (on real
silicon) drives the LED pin. BML filled those in from declarations:

- The `peripheral` blocks told it which accesses are MMIO and how to pack
  bit-fields, so `GPIOC.ODR.ODR8 = true` became the correct masked volatile
  store.
- The `.target` file told it the memory map, so it generated a linker script and
  a reset vector that jumps to `main`.
- `@context(thread)` told it the priority `main` runs at, which it will use the
  moment you add an interrupt.

> **From C:** the closest equivalent is a vendor HAL plus a hand-written linker
> script and `startup_stm32.s`. Here the register *layout* is data
> (`peripheral`), the memory *map* is data (`.target`), and the compiler
> generates the glue. The payoff grows in later tutorials: the type system knows
> a register field is 2 bits wide and read-only, knows a buffer belongs to a DMA
> engine, and rejects code that violates either.
>
> **From Rust:** think `cortex-m`/PAC + `cortex-m-rt` + a `memory.x`, but the
> peripheral access layer is a language feature instead of a generated crate, and
> there is no `unsafe` to reach the registers. The safety comes from a narrower,
> embedded-specific model (contexts, storage classes, regions) rather than a
> general borrow checker -- that's the subject of tutorials 05-08.

## Next

[Tutorial 02 - Values and Control Flow](02-values-and-control-flow.md): the type
system that just rejected your missing `as`, why there are no implicit
conversions, and the full set of control-flow constructs (including `match` and
expression-position `if`/blocks).
