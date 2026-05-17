# Language Specification

## 1. Variables and storage classes

| Declaration  | Scope   | Mutability                  | Storage          |
|-------------|---------|-----------------------------|------------------|
| `var`       | Fn body | Mutable                     | Stack            |
| `val`       | Fn body | Immutable                   | Stack            |
| `const`     | Module  | Immutable                   | Flash (.rodata)  |
| `static`    | Module  | Mutable (access-controlled) | RAM (.bss/.data) |
| `peripheral`| Module  | Access-controlled           | MMIO bus         |

### Built-in types

| Type  | Width | LLVM   | Notes |
|-------|-------|--------|-------|
| `i8`  | 8-bit  | `i8`   | Signed byte |
| `i16` | 16-bit | `i16`  | Signed halfword |
| `i32` | 32-bit | `i32`  | Signed word |
| `i64` | 64-bit | `i64`  | Signed doubleword |
| `u8`  | 8-bit  | `i8`   | Unsigned byte |
| `u16` | 16-bit | `i16`  | Unsigned halfword |
| `u32` | 32-bit | `i32`  | Unsigned word (default integer literal) |
| `u64` | 64-bit | `i64`  | Unsigned doubleword |
| `f16` | 16-bit | `half` | Half-precision float (ARM VFP only) |
| `f32` | 32-bit | `float`| Single-precision float (default float literal) |
| `f64` | 64-bit | `double`| Double-precision float |
| `b1` | 1-bit  | `i1`   | Boolean (condition results, bit fields) |
| `b8`  | 8-bit  | `i8`   | Byte-sized boolean (MMIO flags, C ABI) |
| `*T`  | 32-bit | `ptr`  | Const pointer to `T` -- read only |
| `*mut T`| 32-bit | `ptr`  | Mutable pointer to `T` -- read+write |
| `*void` | 32-bit | `ptr`  | Opaque const pointer (C interop, no deref) |
| `*mut void`| 32-bit | `ptr`  | Opaque mutable pointer (C interop, no deref) |

Pointers are immutable by default (`*T` = `*const T`). Use `*mut T`
to allow writing through the pointer. `*const T` is not a valid syntax
-- the constness is implicit in `*T`.

`*void` and `*mut void` are opaque pointer types for C interop only.
They cannot be dereferenced or indexed -- use `as` casts to a concrete
pointer type before accessing data.

Integer literals default to `u32`. Suffixed forms override: `42u8`, `255u16`,
`-1i32`, `0i64`. Integer types are NOT cross-compatible -- use `as` to cast.
Unsuffixed integer literals may be used in a typed context (e.g. `var x: u8 = 0`)
when the value fits the target type's range.
Float literals default to `f32`. Suffixed forms override: `h` (f16), `f` (f32), or
`d` (f64). Example: `3.14d`, `2.5f`, `1.0h`. Unsuffixed float literals may be used
in a typed context (e.g. `var x: f64 = 3.14`) when the value fits the target
type's range.

There are no implicit type conversions. All type crossing must use the `as` operator:
```bml
var x: i32 = 42 as i32;     // u32 literal → i32
var y: f64 = x as f64;      // i32 → f64
var z: u8 = 300 as u8;      // narrowing with warning W301
```
Exception: `*mut T` implicitly coerces to `*T` (mutable → const). The reverse
requires an explicit `as` cast.

`var`/`val` may only appear inside function bodies.
`const`/`static`/`peripheral` may only appear at module level.
All `static` declarations must be explicitly initialized (to zero or a value).

## 2. Memory model

Four distinct address spaces, each with different compiler semantics:

| Declaration                      | Semantics                                  |
|----------------------------------|--------------------------------------------|
| `peripheral X at 0x... { ... }`  | MMIO -- volatile per-access, no reordering  |
| `@dma` / `@external`            | RAM -- no elision/caching across accesses   |
| `@shared(ceiling = N)`          | RAM -- auto critical section on access      |
| `@exclusive(owner)`             | RAM -- single-context ownership             |
| (no annotation)                 | RAM -- full optimization (thread-only)      |

The compiler never exposes a `volatile` keyword. These semantics are
inferred from the declaration type -- the compiler knows where data lives
and applies the correct access pattern automatically.

## 3. Move / Copy semantics

- **Copy**: Primitives (`i8`..`i64`, `u8`..`u64`, `f16`..`f64`, `b1`, `b8`),
  arrays/structs composed entirely of Copy types. Assignment duplicates
  the value; old binding remains valid.
- **Move**: Any type wrapping `@exclusive`, `@shared`, or `@dma`.
  Assignment transfers ownership; old binding is invalidated.
- The compiler infers Copy vs Move from type structure. No user annotation
  needed.

This is a hybrid model -- simpler than Rust's general borrow checker but
sufficient for the flat, mostly-static ownership patterns of embedded code.

## 4. Interrupt context system

Functions without an @ annotation are `Any` (callable from anywhere). Four annotation forms:

```bml
fn main()     @context(thread)                         { ... }
fn uart_isr() @isr("USART1", priority=2)               { ... }
fn tick()     @isr(priority=1)                         { ... }  // unlabeled, placed in declaration order
fn dma()      @isr("DMA1", priority=2, tailchain=true) { ... }  // tail-chain-optimized ISR
fn helper()   @naked                                   { ... }  // no prologue/epilogue
fn hot()      @naked @section(".ram_code")             { ... }  // combined annotations
```

The `@isr(label, priority=N)` annotation serves double duty: it declares the function
as an interrupt handler AND assigns it a named slot in the vector table. The target
file's `[interrupts]` section maps labels to vector table offsets:

```
[interrupts]
SysTick = 15
USART1 = 37
```

### Annotation reference

| Annotation | Applies to | Description |
|------------|-----------|-------------|
| `@context(thread)` | Functions | Function runs in thread context; can't be called from ISRs |
| `@isr("L", priority=N)` | Functions | Interrupt handler at slot `L`, priority `N`; gets `"interrupt"` LLVM attribute |
| `@isr("L", priority=N, tailchain=true)` | Functions | ISR with tail-chain friendly codegen: no `"interrupt"` attribute, no DSB, no prologue. Leaf ISRs get `bx lr`; non-leaf get `push {lr}` / `pop {pc}`. Body must not save/restore LR manually. |
| `@naked` | Functions | No LLVM `"interrupt"` attribute, no default return. Emits `unreachable` fallback. Full manual control of prologue/epilogue via inline asm. |
| `@section("name")` | Functions, statics | Places the item in the named linker section (e.g. `.ram_code`) |
| `@exclusive(fn)` | Statics | Single-context ownership, only `fn` may access |
| `@shared(ceiling=N)` | Statics | Auto critical section via `cpsid i` / `cpsie i` |
| `@dma` | Statics | DMA-accessible RAM |
| `@external` | Statics | External/C-accessible RAM |

Annotations may be combined in any order. For example:
```bml
fn isr() @isr(priority=1) @naked @section(".ram_code") { ... }
```

### Context levels

In ARM Cortex-M, lower priority number = higher actual priority.
The compiler uses the ARM convention directly:

| Annotation                          | Level | Meaning                  |
|-------------------------------------|-------|--------------------------|
| `@context(thread)`                  | 255   | Lowest priority          |
| `@isr(priority=0)`                  | 0     | Highest priority         |
| `@isr(priority=N)`                  | N     | NVIC priority N          |
| `@isr(priority=N, tailchain=true)`  | N     | NVIC priority N, tail-chain friendly |
| *(no annotation)*                   | --     | Callable from anywhere   |

### Rules enforced at compile time

| Rule | Error code |
|------|-----------|
| `@exclusive(owner)` -- only the owning function may access | E401 |
| `@shared(ceiling=N)` -- current priority must be ≥ N (ARM: lower number = higher priority; higher number = lower priority) | E402 |
| ISR cannot call `@context(thread)` functions | E403 |
| Thread cannot call `@isr(...)` functions | E403 |
| Unannotated `static` -- implicitly thread-only | E404 |

Thread context accessing `@shared(...)` is always allowed -- the compiler
will auto-insert a `cpsid i` / `cpsie i` critical section during codegen.

Functions without an `@` annotation have context `Any` (callable from
any priority level). When an `Any`-context function accesses a `@shared`
static, the compiler conservatively emits a critical section -- the
function could be called from thread context where preemption is possible.

### Ceiling protocol

The priority ceiling is the *highest priority* (lowest ARM number) among all
contexts that access a resource. Tasks at equal or lower priority (higher
number) can access. Tasks at higher priority are rejected.

```
@shared(ceiling = 2):
  ISR(0): REJECTED (0 < 2) -- needs @shared(ceiling = 0)
  ISR(1): REJECTED (1 < 2)
  ISR(2): ALLOWED  (2 ≥ 2) -- direct access
  ISR(3): ALLOWED  (3 ≥ 2) -- auto critical section
  thread: ALLOWED -- auto critical section
```

## 5. Pointer semantics

### Type model

`*T` is the immutable-by-default pointer type. `*mut T` is required for writes.

```bml
var p: *u8 = &my_buf;        // const pointer (read only)
var q: *mut u8 = &mut my_buf; // mutable pointer (read+write)
```

`*const T` is NOT valid syntax -- `*T` already means const pointer.
There is no `&T` in type position -- `&` is purely an expression operator.

### Creating pointers

| Expression | Produces | Notes |
|-----------|----------|-------|
| `&x` | `*T` | Address of local, static, peripheral, or array element |
| `&mut x` | `*mut T` | Mutable address; rejected on `val` bindings |
| `null` | `*T` / `*mut T` | Null pointer, compatible with any pointer type |
| `expr as *T` | `*T` | Integer→pointer cast (LLVM `inttoptr`) |
| `expr as *mut T` | `*mut T` | Integer→mutable pointer cast |
| `p as u32` | `u32` | Pointer→integer cast (LLVM `ptrtoint`) |

### sizeof operator

`sizeof(type)` returns the size of a type in bytes as a `u32` constant:

```bml
var a: u32 = sizeof(u32);   // 4
var b: u32 = sizeof(u8);    // 1
var c: u32 = sizeof(Point); // 8 (for struct with two u32 fields)
var d: u32 = sizeof(*u32);  // 4 (pointer size on ARM Cortex-M)
```

`sizeof` is a compile-time constant expression. It evaluates to the
byte count returned by `element_size()` -- packed layout for structs,
natural sizes for primitives and pointers.

### Dereferencing

| Expression | Type | Allowed on |
|-----------|------|-----------|
| `*p` | `T` (read) | `*T` and `*mut T` |
| `*p = v` | -- (write) | `*mut T` only |
| `p[i]` | `T` (read) | `*T` and `*mut T` |
| `p[i] = v` | -- (write) | `*mut T` only |

Writing through a `*T` (const pointer) is a compile error (E314).

### Pointer comparison

```bml
if p == null { ... }
if p != null { ... }
if p == q { ... }     // both must be same pointer type
```

`null` is compatible with any pointer type in comparisons.

### Pointer arithmetic

`p + n` where `p: *T` or `*mut T` and `n` is any integer type produces
a pointer offset by `n * sizeof(T)` elements (LLVM `getelementptr`).
This is the **only mixed-type binary operation** -- pointer+integer is
fundamental to array iteration.

Subtraction works the same way:
```bml
var p: *u8 = &buf[0];
var q = p + 5;       // q points to buf[5]
var r = q - 2;       // r points to buf[3]
```

Pointer difference (`p - q` where both are `*T`) returns element count:
```bml
var diff = q - p;    // 5 (number of u8 elements)
```

Byte-level arithmetic requires explicit casts:
```bml
var addr = p as u32 + 4;   // move forward 4 bytes
var p2 = addr as *u8;      // back to pointer
```

### Implicit coercion

`*mut T` implicitly coerces to `*T` (mutable → const). This is the only
implicit coercion in the language -- it is sound because promising not to
modify data you can modify is always safe.

The reverse (`*T` → `*mut T`) requires an explicit `as` cast:
```bml
extern fn strlen(s: *u8) -> u32;

var p: *mut u8 = &mut buf[0];
strlen(p);                   // OK -- *mut u8 → *u8 implicit
```

### C interop

```bml
extern fn memcpy(dst: *mut u8, src: *u8, n: u32) -> *mut u8;
extern fn memset(ptr: *mut u8, val: u8, n: u32) -> *mut u8;
extern fn malloc(size: u32) -> *mut u8;
extern fn free(ptr: *mut u8);

fn main() @context(thread) {
    var buf: *mut u8 = malloc(256);
    memset(buf, 0, 256);
    free(buf);
}
```

### Function pointers

Function pointer types use the `fn` keyword in type position:
`fn(params) -> ret`. Omit `-> ret` for `void` return.

```bml
var fp: fn(i32) -> i32 = null;
fp = &add_one;            // take address of any function without @context restriction
var result = fp(41);      // indirect call

// Function pointer as parameter
fn apply(op: fn(i32) -> i32, x: i32) -> i32 {
    return op(x);
}

// C callback pattern: pass fn pointer to extern C library
extern fn register_callback(cb: fn(i32) -> void);
register_callback(&my_handler);
```

Function pointers are restricted to functions without @annotation (Any context) only.
Taking `&` of a `@context(thread)` or `@isr(...)` function emits
E408. Function pointers are pointer-like: they can be `null`, compared with
`== null` / `!= null`, stored in structs, and passed as arguments.

### Safety rules (compile-time)

| Rule | Error |
|------|-------|
| Write through `*T` (const pointer) | E314 |
| Dereference non-pointer type | E315 |

### Design notes

- **No lifetimes.** Pointers carry no lifetime information. Stack escape
  is not currently detected (planned). The existing storage annotations
  (`@exclusive`, `@shared`) protect statics.
- **No runtime null checks.** Dereferencing null triggers a HardFault on
  Cortex-M -- same as C. Use `if p != null { ... }` guards explicitly.
- **No fat pointers.** `*T` is always 32 bits (a single address). For
  slice-like types, use a separate `struct` with pointer + length fields.
- **No `&T` in type position.** The `&` sigil is reserved for the addr-of
  expression operator. It does not appear in type annotations. This avoids
  confusion with C++/Rust references which carry non-null guarantees.

## 6. Struct types

Structs are user-defined composite types with named, ordered fields.
Packed layout (no alignment padding), nominal typing (same field layout
with different name = different type).

### Definition

```
struct Point {
    x: u32,
    y: u32,
}
```

Fields are comma-separated. Each field is declared `name: Type`.

### Initialization

```
var p = Point { x: 10, y: 20 };
```

All fields must be provided (no default values). Duplicate field names in
the initializer are errors (E321).

### Field access

- Read: `p.x` -- returns the field value (or via `extractvalue` in LLVM)
- Write: `p.x = 42` -- assigns to a single field (GEP + store)
- Pointer-to-struct: `(*p).x` -- dereference first, then field access
- Address of field: `&p.x` -- produces a pointer to the field (GEP)

### Semantics

A struct is Copy if **all** its fields are Copy; otherwise it is Move.
Structs containing `@exclusive` or `@shared` fields are Move-typed.

### LLVM lowering

Struct types are lowered to LLVM anonymous struct types:
```
Point → { i32, i32 }
```

- Field read: `extractvalue { i32, i32 } %struct_val, 0`
- Field write: `getelementptr { i32, i32 }, ptr %alloca, i32 0, i32 0` + `store`
- Struct init: allocate temp, GEP + store each field, load whole struct
```

## 7. Enum types

Enums are user-defined nominal types backed by an integer representation.
Each variant maps to a discriminant value that fits within the underlying type.
Enum types are Copy (they are plain integers at runtime).

### Definition

```
enum State: u32 {
    Idle = 0,
    Running = 1,
    Error,
}
```

The underlying type is mandatory (`: u8`, `: u16`, or `: u32`). Variant
discriminants are integer literals. Omitted discriminants auto-increment
from the previous value (starting at 0 for the first variant).

### Variant access

Variants are accessed via the `@` operator:

```bml
var s = State@Idle;
s = State@Running;
if s == State@Error { }
```

`EnumName@VariantName` is a compile-time constant that evaluates to the
discriminant value.

### Casts

Enums are not implicitly compatible with integers. Use explicit `as` casts:

```bml
var raw: u32 = s as u32;
var back: State = raw as State;
```

### Semantics

- `sizeof(State)` returns the size of the underlying type (1, 2, or 4)
- Enum types are Copy
- Nominal typing: two enum types with different names are never compatible,
  even if they share the same discriminants
- Variant names are scoped to their enum; conflicts with other namespaces
  are caught as E200 (duplicate name)

### LLVM lowering

Enum values are just integers of the underlying type:
`State@Idle` → `add i32 0, 0` (for `State: u32`).

## 8. Module system

- One file = one module (`.bml` extension)
- `import foo;` -- wildcard import (imports all exported items)
- `import foo { bar, baz };` -- selective import (imports only listed items)
- `import foo as f;` -- aliased import (access via `f.bar()` qualified syntax)
- `export fn foo;` -- marks public (non-exported items are private)
  - Also supports: `export struct Foo;`, `export enum Bar;`, `export static X;`, `export const Y;`, `export peripheral Z;`
- No circular imports (compile error E500)
- No header files -- compiler reads `.bml` directly
- Module-level items are unordered within a file; forward references are fine
- Module resolution: `import foo` resolves to `foo.bml` in the same directory as the importing file
- Path-based imports: `import sub.mod` resolves to `sub/mod.bml` relative to the importing file
  - Intermediate segments become subdirectories; the last segment is the module name
  - Works with all import forms: wildcard, selective, and aliased
- Compilation model: all imported items are inlined into a flat merged program (single `.ll`/`.o` output)

**Export syntax:**
```
export fn init, send;
export struct Point;
export enum State;
export static BUF;
export const RATE;
export peripheral UART1;
```

Items not listed in any `export` statement are private to the module and cannot be imported.

**Aliased imports:**
```
import lib as L;
fn main() @context(thread) {
    L.foo();  // qualified access
}
```

Aliased imports keep their namespace -- all accesses must use the alias prefix.

## 9. Peripheral declarations

```
peripheral GPIOA at 0x40020000 {
    reg MODER offset 0x00 {
        field MODER0: u32 bit[0..1]
        field MODER1: u32 bit[2..3]
    }
    reg ODR offset 0x14 {
        field ODR0: b1 bit[0]
    }
}
```

- Peripherals are typed structs at known addresses
- `periph_name.reg_name` reads a register (volatile load)
- `periph_name.reg_name = expr` writes a register (volatile store)
- `periph_name.reg_name.field_name` reads a bit field (volatile load + bit extract)
- `periph_name.reg_name.field_name = expr` writes a bit field (read-modify-write:
  loads the full register, clears the field bits, masks and shifts the new value,
  and writes back with a volatile store)
- On Cortex-M3/M4 targets with `has_bitband = true`, single-bit fields within the
  bit-band region (peripheral `0x4000_0000`–`0x400F_FFFF`, SRAM `0x2000_0000`–`0x200F_FFFF`)
  use direct alias load/store instead of RMW -- no masking or shifting needed.
- Field types must be explicitly declared -- `field NAME: TYPE bit[N]` for a single bit
  or `field NAME: TYPE bit[L..H]` for a bit range. Single bits are typically `b1`,
  multi-bit ranges `u32`.
- `&PERIPH` yields `*PeriphType` for use in pointer contexts
- `&periph.reg` yields a pointer to the register (via `inttoptr`)
- CMSIS-SVD XML import available via the standalone [`bml-svd`](../../bml-svd/) tool

## 10. Target files

```
# stm32f401.target
arch = armv7em
priority_bits = 4
has_fpu = true
has_bitband = true
has_mpu = true
flash_base = 0x08000000
flash_size = 256K
ram_base = 0x20000000
ram_size = 64K
vector_table_offset = 0x08000000
```

- Keys: `arch`, `priority_bits`, `has_fpu`, `has_bitband`, `has_mpu`,
  `flash_base`, `flash_size`, `ram_base`, `ram_size`, `vector_table_offset`
- `has_bitband = true` enables bit-band alias access for single-bit fields
  on Cortex-M3/M4 (peripheral region `0x4000_0000`–`0x400F_FFFF`,
  SRAM region `0x2000_0000`–`0x200F_FFFF`)
- Size suffixes: `K` (×1024), `M` (×1024²)
- Hex prefixes: `0x` / `0X`
- Auto-generates linker script via `bml build`

## 11. Grammar (summary)

```
program       = { item }

item          = fn_def | extern_fn_def | static_def | const_def
              | peripheral_def | import_stmt | export_stmt
              | struct_def
              | enum_def

fn_def        = "fn" ident "(" [params] ")" ["->" type]
                [ fn_annotation ] block

extern_fn_def = "extern" "fn" ident "(" [params] ")" ["->" type]
                [ fn_annotation ] ";"

fn_annotation = "@context" "(" "thread" ")"
              | "@isr" "(" [string ","] "priority" "=" int ["," "tailchain" "=" b1] ")"
              | "@isr" "(" [string ","] "tailchain" "=" b1 ["," "priority" "=" int] ")"
              | "@naked"
              | "@section" "(" string ")"

static_def    = "static" ident ":" type
                { "@" storage_annotation } ["=" expr] ";"

const_def     = "const" ident ":" type "=" expr ";"

peripheral_def= "peripheral" ident "at" int "{" { reg_def } "}"

reg_def       = "reg" ident "offset" int "{" { field_def } "}"

field_def     = "field" ident ":" type "bit" "[" int [ ".." int ] "]"

storage_annotation = "exclusive" "(" ident ")"
              | "shared" "(" "ceiling" "=" int ")"
              | "dma" | "external" | "section" "(" string ")"

import_stmt   = "import" ident ["{" ident {"," ident} "}"] ["as" ident] ";"

export_stmt   = "export" ("fn" | "static" | "const" | "peripheral" | "struct" | "enum")
                ident {"," ident} ";"

struct_def    = "struct" ident "{" { ident ":" type "," } "}"

enum_def      = "enum" ident ":" type "{" { ident ["=" int] "," } "}"

stmt          = var_decl | assign | expr_stmt | if_stmt | loop_stmt
              | while_stmt | return_stmt | break_stmt | continue_stmt
              | block | match_stmt | asm_stmt

match_stmt    = "match" expr "{"
                { match_arm }
                "}"
match_arm     = match_pattern {"|" match_pattern} block
match_pattern = ident "@" ident | "_"

match_expr    = "match" expr "{"
                 { match_arm_expr }
                 "}"
match_arm_expr= match_pattern {"|" match_pattern} block
              ;; arm block must have a trailing expression

block_expr    = block
              ;; block used as an expression; must have a trailing expression
              ;; (last item without semicolon)

if_expr       = "if" expr block "else" (block_expr | if_expr)
              ;; if/else as expression; else branch required;
              ;; both branches must have trailing expressions

var_decl      = ("var" | "val") ident [":" type] "=" expr ";"

assign        = lvalue "=" expr ";"

lvalue        = ident | lvalue "." ident | lvalue "[" expr "]"
              | "*" expr               (* deref write target *)

if_stmt       = "if" expr block ["else" (block | if_stmt)]

loop_stmt     = "loop" block

while_stmt    = "while" expr block

return_stmt   = "return" [expr] ";"

break_stmt    = "break" ";"

continue_stmt = "continue" ";"

asm_stmt      = "asm" "{" "body" "}"

for_stmt      = "for" ident "in" expr ".." expr block

array_init    = "[" [expr {"," expr}] "]"

type          = ident                   (* named type: u32, i8, ... *)
              | "*" type               (* const pointer (default) *)
              | "*" "mut" type         (* mutable pointer *)
              | "[" type ";" expr "]"  (* array type *)
              | "fn" "(" [type {"," type}] ")" ["->" type]  (* function pointer *)

;; Expression parsing uses Pratt precedence climbing.
;; The productions below are postfix/prefix operations
;; threaded through expr_prec(min_prec):

expr          = binary_expr (via Pratt parser -- see parser.rs)

cast_expr     = expr "as" type

enum_variant  = expr "@" ident

sizeof_expr   = "sizeof" "(" type ")"

struct_init   = ident "{" { ident ":" expr "," } "}"

### Literals

```
123           -- integer literal (u32)
0x1FF         -- hex integer literal
3.14d         -- f64 literal
2.5f          -- f32 literal
1.0h          -- f16 literal
3.14          -- unsuffixed float literal (f32, use in typed context)
"hello"       -- string literal
true / false  -- boolean literal
null          -- null pointer literal
```

Unsuffixed float literals default to `f32`. They may be used in a typed context
(e.g. `var x: f64 = 3.14`) when the value fits the target type's range.
Pointer literals: `null` is the only pointer literal. Its type is inferred
from context and is compatible with any `*T` or `*mut T`.

## 12. Error codes

| Code  | Meaning |
|-------|---------|
| E001  | Unterminated block comment |
| E002  | Invalid number literal |
| E003  | Unknown escape sequence in string |
| E004  | Unterminated string literal |
| E005  | Unexpected character |
| E006  | Unterminated asm block |
| E100  | Parser: expected specific token |
| E101  | Expected item |
| E102  | (removed -- `@context` is now optional, default Any) |
| E103  | Invalid context/annotation expression |
| E104  | Invalid storage annotation |
| E105  | Expected `bit` |
| E106  | Expected identifier |
| E107  | Expected integer |
| E108  | Invalid annotation (duplicate, missing, or malformed) |
| E200  | Duplicate name |
| E201  | `@exclusive` references unknown function |
| E300  | Type mismatch in var declaration |
| E301  | Type mismatch in assignment |
| E302  | If condition must be b1 |
| E303  | While condition must be b1 |
| E304  | Use of moved value |
| E305  | Undefined name or type |
| E306  | Logical not requires b1 |
| E307  | Function argument count mismatch |
| E308  | Function argument type mismatch |
| E309  | Cannot assign to immutable variable (`val`) |
| E310  | Type mismatch in arithmetic expression -- use `as` to cast |
| E311  | Comparison between different types -- use `as` to cast |
| E312  | For loop range types mismatch |
| E313  | Array element type mismatch |
| E314  | Cannot write through const pointer (`*T`) -- use `*mut T` |
| E315  | Dereference requires pointer type |
| E318  | Struct field not found |
| E319  | Duplicate name (struct field, enum variant, or match arm) |
| E320  | Missing field in struct initializer |
| E321  | Duplicate field in struct initializer |
| E322  | Peripheral register/field or enum variant not found |
| E323  | Invalid enum underlying type or discriminant out of range |
| E324  | Match scrutinee must be an enum type |
| E325  | Non-exhaustive match (missing variants) |
| E326  | Wildcard `_` cannot be combined with other patterns |
| E327  | Expression arm type mismatch (match arm, if branch, or fn pointer call) |
| E328  | Block used as expression has no value |
| E408  | Cannot take address of `@context(thread)` or `@isr` function -- only functions without @restriction can be used as function pointers |
| E400  | Use of moved value (borrow checker) |
| E401  | `@exclusive` access from wrong function |
| E402  | `@shared` ceiling violation |
| E403  | Context-incompatible function call (ISR→thread or thread→ISR) |
| E404  | Access to thread-only static from ISR |
| E500  | Circular import |
| E501  | Module not found |
| E503  | Item is not exported from module (private access) |
| W200  | (unused -- was "import statements not yet supported") |
| W301  | Integer literal may be truncated in cast |
| W600  | Recursive call chain detected -- stack depth may be under-estimated |
