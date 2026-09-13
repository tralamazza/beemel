# Verification finding codes (V-series)

`bml verify` reports findings from [IKOS](./verify.md) as V-series
diagnostics. This is the canonical list; it is referenced from
[`language.md`](./language.md) (§ Error codes) and [`verify.md`](./verify.md).

The **Check** column is the IKOS analysis that produces the finding (the names
accepted by `--checks`); a `—` means the code is a sub-kind reported under a
broader check.

| Code | Check     | Meaning                                          |
|------|-----------|--------------------------------------------------|
| V100 | boa       | Buffer/array out of bounds (error)               |
| V101 | boa       | Buffer/array out of bounds (warning)             |
| V110 | nullity   | Null pointer dereference                         |
| V111 | —         | Null pointer comparison                          |
| V112 | —         | Invalid pointer dereference                      |
| V113 | poa       | Pointer arithmetic overflow                      |
| V114 | —         | Unknown memory access                            |
| V115 | —         | Pointer comparison across unrelated objects      |
| V116 | —         | Store with no effect                             |
| V120 | dbz       | Division by zero                                 |
| V130 | sio / uio | Signed/unsigned integer overflow. ALWAYS ERROR-LEVEL: overflow on plain ops is excluded by the language contract, so "may overflow" warnings are escalated to errors after suppressions apply (prove it, declare wrap with `+%`, or suppress visibly). Signed-typed (iN) add/sub/mul/neg carry `nsw` in the verify IR (only there -- runtime IR stays flag-free), so IKOS checks them as SIGNED overflow with branch narrowing; unsigned ops are checked unsigned. The analyzer is invoked with `--no-wrap-sign-only` (fork patch): the tag picks the check, but the abstract semantics stay wrapping, so an unproven overflow never becomes an assumption for the code after it. Not reported on lines covered by a wrapping-arithmetic expression (`+%`/`-%`/`*%`): wrap there is declared intent. Spin-loop guards and gate conditions prove: branch-position `&&`/`||`/`!` lower as short-circuit branch trees (no phi, no materialized boolean), so each conjunct is a same-block decision IKOS refines through -- the eager `and i1` form blocked refinement when one operand came from a hardware load, and a phi join loses `!(a \|\| b) => both false` |
| V140 | shc       | Shift count exceeds bit width                    |
| V150 | upa       | Unaligned pointer access                         |
| V160 | uva       | Undefined value access (opt-in; see verify.md)   |
| V170 | dca       | Dead code (unreachable after assert/assume). Kind-0 "unreachable" entries are filtered from reports: bml encodes every obligation as a branch-to-unreachable, producing one per obligation by construction |
| V180 | dfa       | Dangling function pointer call                   |
| V190 | fca       | Function call argument mismatch                  |
| V191 | —         | Recursive function call                          |
| V192 | —         | Call through inline asm                          |
| V200 | prover    | User `assert` statement violated                 |
| V210 | f2i       | Float-to-integer conversion overflow: the float value cannot be represented in the target integer type. Undefined behaviour in C, so a **definite** overflow is an error; a **possible** one (bounds not tight enough to rule it out) is a warning; provably in-range is silent. On by default. |
| V220 | fpz       | Floating-point exception class: `fdiv` by a definitely-zero divisor, `0/0` / `inf-inf` / `inf*0` (invalid), `frem` by zero, `sqrt`/`log*` of a definitely-negative operand. Only a defect where the target runs with that FPSCR/MXCSR class **unmasked**, which the analyzer cannot see -- every message is conditional ("traps only on a target with the class unmasked"). **Opt-in** (`--checks ...,fpz`); kept out of `dbz` so integer semantics stay clean. |
| V999 | —         | Other IKOS finding (catch-all for unmapped kinds)|
