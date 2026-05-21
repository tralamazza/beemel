# Verification with IKOS

`bml verify` runs [IKOS](https://github.com/NASA-SW-VnV/ikos) (NASA's
LLVM-based abstract-interpretation static analyzer) on a BML program to detect
runtime errors such as buffer overflows, null pointer dereferences, division by
zero, and integer overflow.

## What Gets Checked

| Check        | Analysis                                               | Severity |
|--------------|--------------------------------------------------------|----------|
| boa          | Buffer/array out of bounds (error)                     | Error    |
| boa          | Buffer/array out of bounds (warning)                   | Warning  |
| nullity      | Null pointer dereference                               | Error    |
| dbz          | Division by zero                                       | Error    |
| sio / uio    | Signed/unsigned integer overflow                       | Warning  |
| shc          | Shift count exceeds bit width                          | Error    |
| upa          | Unaligned pointer access                               | Error    |
| uva          | Undefined value access                                 | Error    |
| dca          | Dead code (unreachable after assert/assume failure)    | Warning  |
| dfa          | Dangling function pointer call                         | Error    |
| fca          | Function called with wrong argument count or type      | Error    |
| prover       | User-provided `assert` statements                      | Error    |

All 13 checks run by default. Pass `--checks <list>` to run a subset.

### Diagnostic codes

Findings surface as BML V-series diagnostics. The full table lives in
[`language.md` § Error codes](./language.md), but the most common ones are:

| Code  | Meaning                                            |
|-------|----------------------------------------------------|
| V100  | Buffer/array out of bounds (error)                 |
| V101  | Buffer/array out of bounds (warning)               |
| V110  | Null pointer dereference                           |
| V111  | Null pointer comparison                            |
| V112  | Invalid pointer dereference                        |
| V113  | Pointer arithmetic overflow                        |
| V114  | Unknown memory access                              |
| V115  | Pointer comparison across unrelated objects        |
| V116  | Store with no effect                               |
| V120  | Division by zero                                   |
| V130  | Signed/unsigned integer overflow                   |
| V140  | Shift count exceeds bit width                      |
| V150  | Unaligned pointer access                           |
| V160  | Undefined value access                             |
| V170  | Dead code                                          |
| V180  | Dangling function pointer call                     |
| V190  | Function call argument mismatch                    |
| V191  | Recursive function call                            |
| V192  | Call through inline asm                            |
| V200  | User `assert` statement violated                   |
| V999  | Other IKOS finding (catch-all for unmapped kinds)  |

### Entry Points

IKOS analyzes functions marked as entry points. BML collects them automatically:

- All functions with `@context(thread)`
- All functions with `@isr(...)` (labeled or unlabeled)

If no annotated functions exist, all functions are used as a fallback.

## What Doesn't Get Checked

- Integer refinement ranges (`u32 in 0..=N`) -- deferred.
- Custom `@shared` ceiling protocol verification -- deferred.
- Full data-race or concurrency protocol checking. Shared-memory reads are
  conservatively invalidated, but `@shared` ceiling protocol verification is
  deferred.
- Liveness or termination.
- Overflow in `for` loop induction variables.

## Soundness

For a single execution trace (no interrupt preemption), IKOS is sound:
every reported violation corresponds to a real possible execution path.

Under interrupt preemption, reads of `@shared` statics are invalidated with
IKOS's `__ikos_forget_mem` intrinsic before loading. This models that a higher
priority ISR may have changed shared memory since the previous read. The model
is conservative and may reduce precision for shared-state-heavy code.

The preempt shim is only emitted when a strictly higher-priority writer
actually exists for the static being read. Functions that read a `@shared`
static no other preemptable writer touches are not invalidated.

### Expected noise after the shim fires

`__ikos_forget_mem` marks the storage as uninitialized in IKOS's internal
model. As a side effect, any subsequent use of the havoc'd value will trip
the `uva` check, so a V200 (assert) finding from a preempted shared read is
usually accompanied by a V160 (uninitialized variable) at the same line.
Treat the V160 as redundant noise -- the V200 is the load-bearing diagnostic.

Function parameters in entry-point functions (`@context(thread)`, `@isr`)
also trigger V160 because IKOS has no caller from which to infer their
values. This is informational, not a bug in the program under analysis.

## Domain Selection Guide

| Domain                | Alias    | Speed    | Precision | Use Case                              |
|-----------------------|----------|----------|-----------|---------------------------------------|
| interval              | —        | Fastest  | Coarse    | Quick bounds checking, CI             |
| apron-octagon         | octagon  | Moderate | Moderate  | Relationship tracking, fewer FPs      |
| apron-interval        | apron    | Slow     | Fine      | Complex arithmetic, maximal precision |

Default: `interval`.

## Usage

```bash
# Basic verification
bml verify program.bml

# With a target file
bml verify --target my_board.target program.bml

# Use apron-octagon domain for better precision
bml verify --domain apron-octagon program.bml

# Custom check subset
bml verify --checks boa,dbz,nullity program.bml

# With explicit IKOS binary path (or set BML_IKOS_BIN env var)
bml verify --ikos-bin /opt/ikos/bin/ikos-analyzer program.bml

# With explicit report tool path, if inference is not suitable
bml verify --ikos-report-bin /opt/ikos/bin/ikos-report program.bml

# Keep intermediate files (.ll, .db, .hwaddrs, .json)
bml verify --save-temps program.bml
```

## `assume` and `assert` Statements

`assume(cond)` tells IKOS to assume `cond` is true for the rest of the
analysis. Use it to narrow values or express preconditions:

```bml
fn divide(a: u32, b: u32) -> u32 {
    assume(b != 0);
    return a / b;
}
```

`assert(cond)` instructs IKOS to verify that `cond` always holds:

```bml
fn process(arr: *u32, len: u32, i: u32) -> u32 {
    assert(i < len);
    return arr[i];
}
```

Both require their argument to be of type `b1`. They are **statements**, not
expressions. In normal `bml build` mode, `assert` is a no-op; only
`bml verify` produces the IKOS intrinsic call.

## Report Format

Findings are printed one per line:

```
{severity}[{code}] {check} violation
  → {file}:{line}:{column}
```

Example:

```
warning[V130] unsigned-int-overflow violation
  → blinky.bml:14:0
```

Fields:
- `severity`: `error`, `warning`, or `info`
- `code`: V-series error code
- `check`: IKOS check name (e.g., `unsigned-int-overflow`)
- `file`, `line`, `column`: source location from debug info

## Error Codes

Verification findings are reported with V-series error codes:

| Code  | Check       | Meaning                                |
|-------|-------------|----------------------------------------|
| V100  | boa (err)   | Buffer/array out of bounds (error)     |
| V101  | boa (warn)  | Buffer/array out of bounds (warning)   |
| V110  | nullity     | Null pointer dereference               |
| V120  | dbz         | Division by zero                       |
| V130  | sio / uio   | Signed/unsigned integer overflow       |
| V140  | shc         | Shift count exceeds bit width          |
| V150  | upa         | Unaligned pointer access               |
| V160  | uva         | Undefined value access                 |
| V170  | dca         | Dead code                              |
| V180  | dfa         | Dangling function pointer call         |
| V190  | fca         | Function call argument mismatch        |
| V200  | prover      | User assert statement violated         |

## Requirements

- An LLVM 18 IKOS build with opaque-pointer support is required. See
  `doc/ikos-setup.md`.
