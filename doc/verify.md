# Verification with IKOS

`bml verify` runs [IKOS](https://github.com/tralamazza/ikos/tree/feat/llvm18)
(NASA's LLVM-based abstract-interpretation static analyzer; this is the LLVM 18
port `bml verify` targets) on a BML program to detect runtime errors such as
buffer overflows, null pointer dereferences, division by zero, and integer
overflow.

## What Gets Checked

| Check        | Analysis                                               | Severity |
|--------------|--------------------------------------------------------|----------|
| boa          | Buffer/array out of bounds (error)                     | Error    |
| boa          | Buffer/array out of bounds (warning)                   | Warning  |
| nullity      | Null pointer dereference                               | Error    |
| dbz          | Division by zero                                       | Error    |
| sio / uio    | Signed/unsigned integer overflow                       | Warning  |
| shc          | Shift count exceeds bit width                          | Error    |
| poa          | Pointer arithmetic overflow / out-of-bounds pointer    | Error    |
| upa          | Unaligned pointer access                               | Error    |
| dca          | Dead code (unreachable after assert/assume failure)    | Warning  |
| dfa          | Dangling function pointer call                         | Error    |
| fca          | Function called with wrong argument count or type      | Error    |
| prover       | User-provided `assert` statements                      | Error    |

All of the above run by default; the full set is
`boa,nullity,sio,uio,dbz,shc,poa,upa,dca,dfa,fca,prover`. `uva`
(uninitialized variable) is opt-in; see the note below for why.

`upa` requires a domain that tracks congruences. The default domain is
`interval-congruence` for that reason; pairing `upa` with the plain
`interval` domain will report every array index with a runtime index as
a false positive.

Pass `--checks <list>` to run any subset.

### Why `uva` is opt-in

Unlike `upa`, this isn't an abstract-domain precision issue — switching
domains can't fix it. The two noise sources are fundamental modeling
artifacts:

1. **Entry-point function parameters.** IKOS analyzes each `@thread` /
   `@isr` function with no caller, so it has no information about the
   initial value of any parameter. The first read of every parameter
   trips V160.
2. **Havoc'd `@shared` reads.** The preempt shim emits
   `__ikos_forget_mem(ptr, size)` immediately before a `@shared` load to
   model "an ISR may have changed this". `__ikos_forget_mem` marks the
   storage as uninitialized by design, so the subsequent load trips
   V160. The V200 (assert) finding at the same line is the real
   diagnostic; the V160 is its modeling shadow.

Neither source corresponds to a real bug, and they fire on most
non-trivial fixtures. Meanwhile BML's frontend already prevents the
classical uninit-local pattern (`var x = expr;` requires an initializer
at declaration; the borrow checker forbids reading moved-out values; no
heap). The narrow real-bug cases that remain are uninit reads through
`@dma` buffers, `@external` storage, or raw `*T` pointers built from
arbitrary addresses.

If your code does any of those, enable `uva` explicitly:

```bash
bml verify --checks boa,nullity,sio,uio,dbz,shc,poa,upa,uva,dca,dfa,fca,prover file.bml
```

The two noise classes could be filtered out on the BML side — drop V160
findings whose operand resolves to a function parameter, and drop V160
findings on loads that immediately follow a `__ikos_forget_mem`. The
parameter filter is cheap (~30 lines, reachable via the symbol table);
the post-forget filter needs IKOS-side cooperation or a different shim
emission strategy. Neither has been done yet because the surface area
of bugs `uva` would catch in real BML code looks small.

### Diagnostic codes

Findings surface as BML V-series diagnostics (V100–V999). The full list is in
[verification-codes.md](./verification-codes.md).

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

### Per-line suppression

To silence a finding on a specific line, place a directive comment on the
same line or the line immediately above:

```bml
var c: u32 = a / b; // bml-verify: ignore V120
```

Multiple codes can be listed, comma-separated:

```bml
var x: u32 = buf[i]; // bml-verify: ignore V100, V101
```

`all` is a wildcard that suppresses every finding on the affected line:

```bml
// bml-verify: ignore all
*p = 42;
```

Use suppressions sparingly. Each one disables a real analyzer result, and
the next reader has to decide whether the suppression is still justified.

### Diagnostic detail

Each finding includes the operand name(s) IKOS associates with the violation,
so a division-by-zero on `a / b` is reported as `division-by-zero violation
(operand: b)`. Full inferred ranges (e.g. `b ∈ [0, 0]`) are written to
`ikos-analyzer`'s stderr with `--display-inv=fail`; BML does not currently
parse or surface that output because it uses unstable LLVM register addresses
rather than source-level variable names.

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
| interval-congruence   | —        | Fast     | Coarse + alignment | Default; congruence keeps `upa` from false-positiving on modular indexing |
| apron-octagon         | octagon  | Moderate | Moderate  | Relationship tracking, fewer FPs      |
| apron-interval        | apron    | Slow     | Fine      | Complex arithmetic, maximal precision |

Default: `interval-congruence`.

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

# Machine-readable output (default: text)
bml verify --format json program.bml

# Control the exit code: fail only on warnings or worse (default: error).
# Levels: error, warning, info, never
bml verify --fail-on warning program.bml

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

Verification findings are reported with V-series error codes (V100–V999); see
[verification-codes.md](./verification-codes.md) for the full list.

## Requirements

- An LLVM 18 IKOS build with opaque-pointer support is required. See
  `doc/ikos-setup.md`.
