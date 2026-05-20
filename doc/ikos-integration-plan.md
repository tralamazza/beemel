# IKOS Integration Plan

Plan for integrating [IKOS](https://github.com/NASA-SW-VnV/ikos) (NASA's
LLVM-based abstract-interpretation static analyzer) into the BML compiler as an
opt-in `bml verify` subcommand.

Scope: parts (a) and (b) from the design discussion.

- **(a)** Wire BML's existing LLVM IR output into IKOS. Auto-generate hardware
  address ranges from peripheral declarations and entry points from `@isr` /
  thread-context functions. Lower new `assume` / `assert` statements to the
  appropriate IKOS intrinsics. Parse IKOS reports back into BML diagnostics.
- **(b)** Concurrency shim. Insert `__ikos_forget_mem` calls before reads of
  `@shared` statics that can be written by a preempting ISR, so the analysis is
  sound under interrupt preemption without requiring IKOS to model concurrency
  natively.

Out of scope for this plan:

- Refinement integer ranges (`u32 in 0..=N`).
- A custom IKOS check verifying the `@shared` ceiling protocol across the call
  graph.
- A direct AR backend bypassing LLVM.

## Phase 0: External preconditions

Before any BML code changes:

1. Pin an IKOS version. Document install: `apt install ikos` on Ubuntu, or
   build from `NASA-SW-VnV/ikos` against the same LLVM toolchain BML's
   `opt`/`llc` use. LLVM version compatibility is tight.
2. Verify three things by running IKOS by hand on a toy `.ll` produced by
   `bml build`:
   - Whether `ikos` accepts `.ll` directly or requires `llvm-as` to convert to
     `.bc`.
   - The exact format expected by `--hardware-addresses-file` (header file
     hints at one range per line; confirm against the `add_range_from_file`
     implementation).
   - The JSON report schema. Run `--format=json --report-file=out.json` and
     capture the shape.
3. If LLVM versions clash with the existing `opt`/`llc`, document a separate
   `IKOS_LLVM` toolchain path. Not a blocker for the design.

**Outcome:** a short `doc/ikos-setup.md` and a known-good manual invocation
that subsequent phases can shell out to.

## Phase 1: CLI surface

New subcommand in `bml/src/main.rs`:

```
bml verify [--target <file.target>] [--domain <name>] [--checks <list>]
           [--no-shim] [--ikos-bin <path>] [--report json|text]
           [--save-temps] <file.bml>
```

- `--domain` defaults to `interval` for speed, `octagon` for thoroughness.
- `--checks` defaults to a curated subset:
  `boa,nullity,sio,uio,dbz,shc,poa,upa,uva,dca,dfa,fca,prover`.
  Skip `pcmp` and `sound` initially (noise sources).
- `--no-shim` disables Phase 8 (concurrency shim) for debugging.
- Mirrors `bml build`'s arg-parsing style.

Effort: small. Add the dispatch case and a `verify_file()` skeleton.

## Phase 2: Language surface for `assume` / `assert`

Keep it minimal. Add as **statements**, not annotations.

**Files touched:**

| File                          | Change                                                                                                  |
|-------------------------------|---------------------------------------------------------------------------------------------------------|
| `bml-core/src/lexer.rs`       | Add `assume`, `assert` to the keyword table                                                             |
| `bml-core/src/ast.rs`         | New `Stmt::Assume(AssumeStmt)` and `Stmt::Assert(AssertStmt)` variants, each carrying `expr` and `span` |
| `bml-core/src/parser.rs`      | Recognize `assume(expr);` and `assert(expr);` in `parse_stmt`                                           |
| `bml-core/src/checker.rs`     | Both require `expr: b1`. Emit a dedicated error code on type mismatch                                   |
| `bml-core/src/borrow.rs`      | Treat as read-only access. No moves                                                                     |
| `bml-core/src/ir.rs`          | Lowering (Phase 3)                                                                                      |

Grammar addendum in `doc/language.md`:

```
stmt        = ... | assume_stmt | assert_stmt
assume_stmt = "assume" "(" expr ")" ";"
assert_stmt = "assert" "(" expr ")" ";"
```

Defer integer refinement ranges to a later iteration. Not needed for (a)+(b).

## Phase 3: IR lowering for `assume` / `assert`

In `IrEmitter::emit`, before any function body is emitted, declare:

```
declare void @__ikos_assert(i32) nounwind
```

Note: `__ikos_assume` is a header macro, not an extern. No declaration needed
for it.

Per statement:

- `assume(cond)`:
  ```
  %c = <emit cond>           ; i1
  br i1 %c, label %ok, label %unreach
  unreach:
    unreachable
  ok:
    <continues>
  ```
  This is exactly what the IKOS macro expands to. The analyzer recognizes the
  pattern via its standard unreachable handling. No intrinsic call.

- `assert(cond)`:
  ```
  %c = <emit cond>           ; i1
  %z = zext i1 %c to i32
  call void @__ikos_assert(i32 %z), !dbg !N
  ```
  The `!dbg` metadata is critical for mapping the eventual diagnostic back to
  the BML span. `bml verify` should force-enable debug info regardless of any
  user flag.

Edge case: `assert` inside a `@naked` function. The function has no prologue
but the assert call is just a regular call. Document as untested initially.

## Phase 4: Hardware addresses generation

New module `bml-core/src/verify/hwaddrs.rs`:

```rust
pub fn write_hwaddrs_file(symbols: &SymbolTable, path: &Path) -> io::Result<()>
```

Logic: for each `PeripheralSymbol`, for each `RegSymbol`, emit one range. Two
granularity choices:

- **Per-register** (precise): one range per register,
  `[base+offset, base+offset+width)` where width comes from field type widths,
  default 4 bytes.
- **Per-peripheral** (loose): one range spanning the full peripheral block.
  Less precise but tolerant of registers BML doesn't explicitly declare.

Start with per-register. If a user encounters a peripheral access that escapes
BML's declarations (e.g. via `as *u32` from a raw address constant), they can
pass `--extra-hwaddrs <file>` to merge in custom ranges.

File format: confirm in Phase 0. Likely one `0xADDR-0xADDR` hex range per line.

`PeripheralSymbol` already has `base_addr: u64` and `regs: HashMap<String,
RegSymbol>` with `offset: u64`. Computing the file is roughly a 20-line
function.

## Phase 5: Entry points

In `verify_file()`, walk `symbols.functions`:

```rust
let entries: Vec<&str> = symbols.functions.iter()
    .filter(|(_, f)| f.isr_label.is_some() || matches!(f.context, Context::Thread))
    .map(|(name, _)| name.as_str())
    .collect();
```

Pass as `--entry-points=name1,name2,...`. If empty (a library-style file with
no `main` and no ISR), fall back to all `Context::Any` functions and warn.

## Phase 6: Orchestration

New module `bml-core/src/verify/mod.rs`:

```rust
pub struct VerifyConfig {
    pub ikos_bin: PathBuf,
    pub domain: String,
    pub checks: Vec<String>,
    pub shim_enabled: bool,
    pub report_format: ReportFormat,
    pub extra_hwaddrs: Vec<PathBuf>,
}

pub fn verify(
    program: &Program,
    symbols: &SymbolTable,
    source_map: &SourceMap,
    target: &Target,
    config: &VerifyConfig,
    work_dir: &Path,
) -> Result<Vec<Finding>, VerifyError>
```

Internal flow:

1. Run `IrEmitter` with `debug=true` (independent of user `--debug`),
   `verify_mode=true` (enables the Phase 8 shim).
2. Write `<stem>.verify.ll`.
3. Write `<stem>.verify.hwaddrs` from Phase 4.
4. Optionally `llvm-as <stem>.verify.ll -o <stem>.verify.bc` if IKOS requires
   bitcode.
5. Build `ikos-analyzer` command: input, `--entry-points=...`, `-d=<domain>`,
   `-a=<checks>`, `--hardware-addresses-file=...`, `--format=json`,
   `--report-file=<stem>.verify.json`.
6. Spawn process, capture stderr for tool errors.
7. Read JSON report, parse into `Vec<Finding>`.
8. Return findings.

Main flow in `bml/src/main.rs` `verify` subcommand:

```rust
match verify(...) {
    Ok(findings) => {
        for f in findings { diags.push(f.into_diagnostic(&source_map)); }
        diags.emit(&source_map);
        if diags.has_errors() { exit(1); }
    }
    Err(e) => { eprintln!("ikos failed: {e}"); exit(1); }
}
```

## Phase 7: Report parsing and diagnostic mapping

In `bml-core/src/verify/report.rs`:

```rust
#[derive(Deserialize)]
pub struct IkosReport { /* schema confirmed in Phase 0 */ }

#[derive(Debug)]
pub struct Finding {
    pub check: String,            // "boa", "nullity", ...
    pub status: Status,           // Error | Warning | Safe | Unreachable
    pub message: String,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
}

impl Finding {
    pub fn into_diagnostic(self, sm: &SourceMap) -> Diagnostic
}
```

Map IKOS check codes to BML error codes:

| IKOS check    | BML code | Severity        |
|---------------|----------|-----------------|
| `boa` (error)   | V100     | Error           |
| `boa` (warning) | V101     | Warning         |
| `nullity`     | V110     | Error           |
| `dbz`         | V120     | Error           |
| `sio` / `uio` | V130     | Warning         |
| `shc`         | V140     | Error           |
| `upa`         | V150     | Error           |
| `uva`         | V160     | Error           |
| `dca`         | V170     | Warning         |
| `dfa`         | V180     | Error           |
| `fca`         | V190     | Error           |
| `prover`      | V200     | Error           |

Status `Safe` is silent. Status `Unreachable` becomes an info diagnostic.

Deduplicate findings: IKOS reports per-callsite and can flag the same span
repeatedly when functions are analyzed under multiple entry points.

Add new V-series codes to `doc/language.md` error table.

## Phase 8: Concurrency shim

New module `bml-core/src/verify/preempt.rs`:

```rust
pub struct PreemptInfo {
    /// For each (reader_fn, static_name): set of writer_fns that can preempt this reader.
    pub preemptable: HashMap<(String, String), HashSet<String>>,
}

pub fn analyze(program: &Program, symbols: &SymbolTable) -> PreemptInfo
```

**Algorithm:**

1. Determine `effective_priority(fn)`:
   - `@isr(priority=N)` -> N
   - `@context(thread)` -> 255
   - `Any` (no annotation) -> 255 (worst case: callable from thread)

2. For each `@shared(ceiling=N)` static `S`:
   - Build `writers(S) = { fn | fn body contains S = ..., S.field = ..., or &mut S }`.
     The borrow checker already needs this information; reuse its data
     structures if possible, otherwise rebuild from an AST walk.
   - For each function R,
     `preemptable[(R, S)] = { W in writers(S) | priority(W) < priority(R) and W != R }`.

3. `@exclusive(owner)`: only `owner` writes, only `owner` reads (BML enforces
   this). No shim needed.

**IR emission integration:**

`IrEmitter` gets a new field `preempt: Option<PreemptInfo>` set when
`verify_mode=true`. Before emitting a load (or load-modify-store for register
fields) of a `@shared` static `S` inside function `R`:

```rust
if !preempt[(R, S)].is_empty() {
    emit_forget_mem(&S, sizeof S);
}
// then emit the actual load
```

where `emit_forget_mem` produces:

```
%p = bitcast <S type>* @S to i8*
call void @__ikos_forget_mem(i8* %p, i64 <sizeof S>)
```

with a module-level `declare void @__ikos_forget_mem(i8*, i64) nounwind`.

**Subtleties to document and decide:**

- **Granularity.** `__ikos_forget_mem` havocs bytes. For a `@shared` struct
  static where only one field is written by a preempting ISR, this is
  over-approximation. Two options:
  - Coarse: havoc the whole static. Simple, correct, less precise. Recommended
    for v1.
  - Fine: per-field havoc when writers only touch one field. Defer.

- **Register-field RMW for peripherals.** A `peripheral.reg.field = v` lowers
  to load-modify-store. Peripherals aren't BML statics and aren't a `@shared`
  concern; IKOS treats them as in `--hardware-addresses` so doesn't track
  values across the RMW. Skip.

- **`Any`-context functions.** A function with no annotation could be called
  from thread context (preemptable by ISRs) or from inside an ISR (preemption
  depends on the calling ISR's priority). Conservative model: treat `Any` as
  priority 255 (thread-like). Sound but loses precision when only ISR callers
  exist. Acceptable for v1.

- **Per-read vs per-basic-block havoc.** On real hardware, the static's value
  doesn't actually havoc between two adjacent reads in the same basic block
  unless a function call or memory barrier sits between them. IKOS has no
  preemption-point concept though, so to model "value can change between reads"
  it has to havoc before each read. Slight precision loss. Recommend per-read
  in v1; optimize in v2 if precision complaints surface.

## Phase 9: Tests

New test directory `bml/tests/verify/`:

- `boa_array_oob.bml` -> expect V100 at a known line.
- `null_deref.bml` -> expect V110.
- `int_overflow.bml` -> expect V130.
- `division_by_zero.bml` -> expect V120.
- `assert_holds.bml` -> expect no findings.
- `assert_fails.bml` -> expect V200 at the assert line.
- `assume_narrows.bml` -> `assume` narrows the input enough to make a
  downstream `assert` provable.
- `shared_static_read.bml` (shim test) -> reading a `@shared` static from
  thread context with a writing ISR. A prover assertion based on the static's
  value should NOT be provable. Verifies the forget_mem is firing.
- `shared_static_no_writer.bml` (negative shim test) -> no ISR writes the
  static. Prover assertion should still hold.

Extend `bml/tests/tests.rs` with a `verify_*` test family that shells out to a
configured IKOS binary. Gate on env var `BML_IKOS_BIN` so CI without IKOS
doesn't break.

Add a CI job that installs IKOS and runs the verify suite. Separate job from
the main `cargo test` so it can fail independently while the toolchain
stabilizes.

## Phase 10: Documentation

1. New `doc/verify.md`:
   - What gets checked, what doesn't.
   - The single-trace soundness statement, the cross-ISR caveat, the role of
     the forget_mem shim.
   - Domain selection guide (interval vs octagon vs apron).
   - Command examples.
2. Update `doc/language.md` section 11 grammar with `assume` / `assert`.
3. Update `doc/language.md` section 12 error table with V100-V200 codes.
4. Update `README.md` Quick-start with a `bml verify` example.

## Effort and ordering

| Phase | Effort   | Blocks                            |
|-------|----------|-----------------------------------|
| 0     | 0.5 day  | everything                        |
| 1     | 0.5 day  | none                              |
| 2     | 1 day    | 3                                 |
| 3     | 1 day    | 9                                 |
| 4     | 0.5 day  | 6                                 |
| 5     | 0.25 day | 6                                 |
| 6     | 1.5 days | 7, 9                              |
| 7     | 1 day    | 9                                 |
| 8     | 2 days   | 9 (the part testing the shim)     |
| 9     | 1.5 days | 10                                |
| 10    | 0.5 day  | nothing                           |

Roughly 10 working days end-to-end. Critical path: 0 -> 6 -> 8 -> 9.

Suggested order: 0 -> 1 -> 4 -> 5 -> 2 -> 3 -> 6 -> 7 -> tests for (a) -> 8 ->
tests for (b) -> 10. This lets you ship a useful "verify without shim" build
at the (a) milestone before tackling the harder concurrency work.

## Open questions to resolve in Phase 0

1. Exact `--hardware-addresses-file` line format.
2. Whether `ikos` accepts `.ll` or only `.bc`.
3. JSON report schema stability across IKOS versions.
4. IKOS version-pin policy.
5. Whether `__ikos_forget_mem` must be a real link-time symbol or if IKOS
   recognizes the declaration only. The intrinsic header declares it as
   `extern`. Confirm whether the analyzer intercepts it before any
   link/LTO step, in which case no body is needed.
6. Whether having both `--debug` (DWARF) and the forget-mem calls interferes
   with IKOS's source-mapping.
