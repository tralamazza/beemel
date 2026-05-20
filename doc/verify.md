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

### Entry Points

IKOS analyzes functions marked as entry points. BML collects them automatically:

- All functions with `@context(thread)`
- All functions with `@isr(...)` (labeled or unlabeled)

If no annotated functions exist, all functions are used as a fallback.

## What Doesn't Get Checked

- Integer refinement ranges (`u32 in 0..=N`) -- deferred.
- Custom `@shared` ceiling protocol verification -- deferred.
- Data races or concurrency safety (no active preemption shim).
- Liveness or termination.
- Overflow in `for` loop induction variables.

## Soundness

For a single execution trace (no interrupt preemption), IKOS is sound:
every reported violation corresponds to a real possible execution path.

Under interrupt preemption, no shim is currently active. `@shared` statics
are analyzed as single-threaded — a preempting ISR that modifies a `@shared`
static between an access check and the access itself is not modelled. This is
unsound under preemption. A concurrency shim (via `__ikos_forget_mem`) was
prototyped and removed due to an IKOS 3.5 crash on pointer-typed extern
declarations.

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

# Keep intermediate files (.ll, .bc, .hwaddrs, .json)
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

- IKOS 3.5 must be installed. See `doc/ikos-setup.md`.
- `llvm-as` from LLVM 14 is **required** (used to convert IR to bitcode). See `doc/ikos-setup.md`.
