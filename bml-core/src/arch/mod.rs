pub mod arm;

/// Native byte order of a target. The default is little-endian, matching the
/// only architectures currently supported (and the `e` in `datalayout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Endianness {
    #[default]
    Little,
    Big,
}

impl Endianness {
    /// Whether a field with byte order `field` must be byte-swapped to reach the
    /// target's native order. A field with no attribute, or one already in the
    /// native order, never swaps; the opposite order does.
    #[must_use]
    pub fn swaps(self, field: crate::ast::FieldEndian) -> bool {
        use crate::ast::FieldEndian;
        match field {
            FieldEndian::Native => false,
            FieldEndian::Big => self == Endianness::Little,
            FieldEndian::Little => self == Endianness::Big,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Armv6m,
    Armv7m,
    Armv7em,
}

/// MPU programming model. Follows the CORE, not the emitted ISA: a
/// Cortex-M33 (`ARMv8-M`) implements `PMSAv8` (`RBAR`/`RLAR` + `MAIR` attribute
/// indirection) even though we emit `v7e-m` code for it. `PMSAv7` regions are
/// power-of-two sized and size-aligned; `PMSAv8` regions are `[base, limit]`
/// pairs at 32-byte granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpuFlavor {
    Pmsa7,
    Pmsa8,
}

impl Arch {
    #[must_use]
    pub fn datalayout(&self) -> &'static str {
        "e-m:e-p:32:32-i64:64-v128:64:128-a:0:32-n32-S64"
    }

    #[must_use]
    pub fn endianness(&self) -> Endianness {
        // All supported ARM cores run little-endian here; kept as a method so
        // byte-order decisions (field `@be`/`@le` swaps, the E360 diagnostic)
        // derive from the target rather than a hardcoded constant.
        Endianness::Little
    }

    #[must_use]
    pub fn ptr_width(&self) -> u32 {
        32
    }

    #[must_use]
    pub fn ptr_type(&self) -> &'static str {
        "i32"
    }

    #[must_use]
    pub fn asm_param_regs(&self) -> &'static [&'static str] {
        &["{r0}", "{r1}", "{r2}", "{r3}"]
    }

    #[must_use]
    pub fn llvm_target_triple(&self) -> &'static str {
        match self {
            Arch::Armv6m => "thumbv6m-none-unknown-eabi",
            Arch::Armv7m => "thumbv7m-none-unknown-eabi",
            Arch::Armv7em => "thumbv7em-none-unknown-eabi",
        }
    }
}
