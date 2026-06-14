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
| `_Bool` (C99)       | `b1`     | `i1`    |                          |
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
- `i8`/`i16` are sign/zero-extended to 32 bits in registers
- LLVM already handles this for the `thumbv7em-none-eabi` target triple

bml-generated LLVM IR is already C ABI compatible. The `extern fn`
declaration just emits LLVM `declare` instead of `define`, and the
caller emits `call` as normal.

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

Usage: `import c;` (brings the module's `extern fn` declarations into scope).

## Deferred

- **Varargs**: `extern fn printf(fmt: *u8, ...);` -- complex, rarely needed in embedded
- **Header-to-bml converter**: tool that parses C headers → `extern fn` declarations
- **Calling convention attributes**: `__attribute__((naked))`, `__attribute__((interrupt))` -- rare edge cases
