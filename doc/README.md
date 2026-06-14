# doc/ map

| Doc | What it is |
|---|---|
| [tutorials/](tutorials/) | Hands-on, runnable learning path -- start here if you're new to BML |
| [language.md](language.md) | The language specification (types, contexts, views, structs, the agent contract table, error codes) |
| [regions-agents.md](regions-agents.md) | The regions/agents memory-safety model: target physics, ownership windows, multi-core, verify obligations, trust register, hardware status |
| [verify.md](verify.md) | `bml verify` usage: checks, soundness, suppressions, domains |
| [verification-codes.md](verification-codes.md) | V-series finding codes |
| [ikos-setup.md](ikos-setup.md) | Building the LLVM 18 IKOS fork |
| [c-interop.md](c-interop.md) | `extern fn`, ABI rules, linking C objects |
| [stm32-cmsis.md](stm32-cmsis.md) | Generating targets/peripherals from vendor CMSIS/SVD |
| [design-decisions.md](design-decisions.md) | Rationale for the major language/compiler choices |
| [hacking.md](hacking.md) | Extending the compiler: pass order, conventions, test kinds |
| [future.md](future.md) | Wishlist without committed designs |

Historical plan documents (memory views, IKOS integration, the
regions/agents slice chronology) were synthesized into the docs above;
the play-by-play lives in git history.
