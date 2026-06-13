# bml-lsp

Language server for the BML language. Implements the Language Server Protocol using `bml-core`.

## Features

| Capability | Description |
|-----------|-------------|
| Diagnostics | Full analysis pipeline on open/change/save; publishes errors and warnings. Region/agent checks (`reclaim`/`view`, E605/E611/E615/E326/...) run when a target is resolved for the file (see Targets) |
| Hover | Type information for functions, statics, peripherals, registers, fields, structs, enums, and locals |
| Go-to-Definition | Navigate to definitions across modules |
| Completion | Keywords, globals, locals, import aliases, peripheral registers/fields |

## Usage

Configure your editor to launch `bml-lsp` for `.bml` files. The server communicates over stdio using the LSP protocol.

```bash
cargo run
```

## Targets

The region/agent diagnostics (`reclaim` vs `view`, ownership/handoff/reclaim
guards, cross-core sharing) only have meaning against a `.target` file -- regions
and agents are declared there, not in the source. Without one, an array placed
`in <region>` looks like plain memory and `reclaim(x)` is wrongly rejected. The
server resolves a target for each open file in two ways:

1. `initializationOptions` (preferred). Either a workspace-wide target or a
   map of path prefixes to targets (longest matching prefix wins). Relative
   paths resolve against the workspace root.

   ```jsonc
   {
     "target": "boards/default.target",        // applies to all files
     "targets": {                              // per-directory overrides
       "examples/rp2350-pico2w": "examples/rp2350-pico2w/pico2w.target"
     }
   }
   ```

   In Zed, this goes under `lsp.bml-lsp.initialization_options` in settings.

2. Discovery (fallback, no config). The server walks up from the file's
   directory to the first one containing a `.target`, and picks the *root*
   target -- the one no sibling `include`s (so a board file that includes a
   chip file is chosen, not the chip file alone). If that directory's targets
   are ambiguous, none is chosen and the region checks stay off.

The target is re-read on every analysis, so editing it (or a base it `include`s)
takes effect on the next change to an open `.bml`. A target that fails to load
is logged to stderr and the file is analyzed target-less.
