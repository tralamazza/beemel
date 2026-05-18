pub mod arm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Armv6m,
    Armv7m,
    Armv7em,
}

impl Arch {
    #[must_use]
    pub fn datalayout(&self) -> &'static str {
        "e-m:e-p:32:32-i64:64-v128:64:128-a:0:32-n32-S64"
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
