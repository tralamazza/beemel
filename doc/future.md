# Future (maybe)

Wishlist with no committed design. Items move out of here when a concrete
consumer appears (multi-core support and MPU generation graduated this way).

- Standard C-interop prelude
- Standard library
- RISC-V target support
- Enum parameterization / tagged unions
- Generic functions (over sizes, types)
- IT block folding (Thumb-2)
- RP2350 PIO as a compile-time resource-allocation problem (state machines,
  instruction memory, pins)
- TrustZone: security states as cpu-agent attribute pairs

Regions/agents-specific deferrals (each waiting on a consumer) live in
`regions-agents.md` ("Open items").
