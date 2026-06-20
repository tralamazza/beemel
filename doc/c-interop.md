# C Interop

Calling C functions and linking against C object files is essential for
using vendor HALs (CMSIS, libopencm3, STM32Cube, etc.) and mixing
existing C code with bml. For the concrete STM32 `SystemInit()` recipe,
see [stm32-cmsis.md](./stm32-cmsis.md).

## Design principles

1. **No header parsing** -- declare C functions in bml syntax with `extern`
2. **C ABI compatible** -- ARM EABI (AAPCS) calling convention via LLVM
3. **Explicit type mapping** -- C types map to bml types per table below
4. **Link normally** -- `lld` links bml `.o` with C `.o`/`.a` files

## Language: `extern fn`

```bml
// No context annotation -- callable from any context (no E403 check)
extern fn getchar() -> u8;
extern fn memcpy(dst: *mut u8, src: *u8, n: u32) -> *mut u8;

// Thread-only -- ISR calling this gets E403
extern fn HAL_Delay(ms: u32) @context(thread);

// ISR handler -- thread calling this gets E403
extern fn HAL_UART_IRQHandler() @isr(priority = 2);

// No annotation -- callable from any context
extern fn HAL_Init() -> u32;
```

Rules:
- `extern fn` has no body; semicolon-terminated
- `@context(...)` is optional:
  - **Absent**: callable from any context, no E403 check
  - **Present**: borrow checker enforces normal context rules (E403)
- Useful for annotating blocking C functions (`@context(thread)`) or C ISR handlers (`@isr(priority = ...)`)
- Cannot have `@shared` / `@exclusive` / `@dma` annotations
- Parameter and return types use bml types

## Type mapping

| C type              | bml type | LLVM    | Notes                    |
|---------------------|----------|---------|--------------------------|
| `char`              | `i8`     | `i8`    | Signed on ARM            |
| `unsigned char`     | `u8`     | `i8`    |                          |
| `short`             | `i16`    | `i16`   |                          |
| `unsigned short`    | `u16`    | `i16`   |                          |
| `int`               | `i32`    | `i32`   | 32-bit on Cortex-M       |
| `unsigned int`      | `u32`    | `i32`   |                          |
| `long`              | `i32`    | `i32`   | 32-bit on ARM            |
| `unsigned long`     | `u32`    | `i32`   |                          |
| `long long`         | `i64`    | `i64`   |                          |
| `unsigned long long`| `u64`    | `i64`   |                          |
| `float`             | `f32`    | `float` |                          |
| `double`            | `f64`    | `double`|                          |
| `void`              | (none)   | `void`  | No return type           |
| `_Bool` (C99)       | `b8`     | `i8`    | C `_Bool` is one byte; `b1` is rejected at the boundary (E356) |
| `uint8_t`           | `u8`     | `i8`    | stdint.h                 |
| `int32_t`           | `i32`    | `i32`   | stdint.h                 |
| `size_t`            | `u32`    | `i32`   | 32-bit on Cortex-M       |
| `void*`             | `*mut void` | `ptr` | Mutable opaque pointer |
| `const void*`       | `*void`     | `ptr` | Const opaque pointer   |
| `uint32_t*`         | `*mut u32`  | `ptr` | Mutable pointer (C pointers are mutable by default) |
| `const uint32_t*`   | `*u32`      | `ptr` | Const pointer          |

## C ABI (ARM EABI / AAPCS)

ARM Cortex-M uses the AAPCS calling convention:

- First 4 arguments in registers r0–r3
- Additional arguments on the stack
- Return value in r0 (or r0:r1 for 64-bit types)
- Stack is 8-byte aligned at function entry
- Sub-word integers (`i8`/`i16`/`u8`/`u16`/`b8`) are passed in 32-bit
  registers; the **caller** must zero/sign-extend the argument and the
  **callee** extends the return -- AAPCS makes neither side re-mask

This is a property of beemel's calling convention (AAPCS), not of
"extern-ness", so bml emits the `signext`/`zeroext` parameter and return
attributes **uniformly** -- on every function signature (`define` and
`extern` `declare`) and every call site (direct, indirect through a
function pointer, and monomorphized `peripheral_type` drivers). LLVM then
lowers the caller-side extension (`uxtb`/`uxth`/`sxtb`/`sxth`). Gating on
the boundary fails for function pointers: at an indirect call -- or at a
`define` that C may call -- you cannot tell whether the other end is C or
bml. Applying it everywhere makes bml's ABI identical to AAPCS, so every
crossing agrees.

This matters most for a *dynamic* (non-constant) narrow argument: without
the attribute a value with dirty upper bits would reach a GCC/clang HAL
(CMSIS, libopencm3) that trusts the caller and does not re-mask -- e.g. a
stray high bit in a `u16` pin mask corrupting `GPIO->BSRR`. Constants are
materialized fully extended regardless, so the hazard is dynamic args only.

Rules: unsigned narrows and `b8` get `zeroext`; signed (`i8`/`i16`) get
`signext`; the attribute is derived from the *lowered* integer, so a
`repr u8` enum gets `zeroext` too; `i32`/`u32` and wider stay bare; `b1`
(rejected at the boundary by E356) is never extended. Internal bml-to-bml
calls also carry the attribute -- harmless and self-consistent, just an
occasional redundant mask the optimizer elides.

The `extern fn` declaration emits LLVM `declare` instead of `define`,
and the caller emits `call` as normal.

## (Future) `c.bml` -- standard C prelude

A built-in module (or distributable `.bml` file) with common C function
declarations:

```bml
// c.bml -- Standard C library declarations
extern fn memcpy(dst: *mut u8, src: *u8, n: u32) -> *mut u8;
extern fn memset(ptr: *mut u8, val: u8, n: u32) -> *mut u8;
extern fn memcmp(a: *u8, b: *u8, n: u32) -> i32;
extern fn strlen(s: *u8) -> u32;
// Blocking functions annotated so ISRs can't call them
extern fn printf(fmt: *u8) -> i32 @context(thread);
```

Usage: `import c;`, then call qualified -- `c.memcpy(...)` (imported `extern fn`s
are reached through the module name, like any other imported function).

## Deferred

- **Varargs**: `extern fn printf(fmt: *u8, ...);` -- complex, rarely needed in embedded
- **Header-to-bml converter**: tool that parses C headers → `extern fn` declarations
- **Calling convention attributes**: `__attribute__((naked))`, `__attribute__((interrupt))` -- rare edge cases
