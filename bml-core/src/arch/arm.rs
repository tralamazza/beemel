use std::collections::HashMap;

use crate::ast::{self, BitSpec, Program};
use crate::ir::IrEmitter;
use crate::resolver::SymbolTable;

#[must_use]
pub fn bit_mask_shift(bits: &BitSpec) -> (u32, u32) {
    match bits {
        BitSpec::Single(n) => {
            let n = *n as u32;
            (1 << n, n)
        }
        BitSpec::Range(lo, hi) => {
            let lo = *lo as u32;
            let hi = *hi as u32;
            let width = hi - lo + 1;
            let mask = ((1u64 << width) - 1) as u32;
            (mask << lo, lo)
        }
    }
}

const PERI_BITBAND_REGION_BASE: u64 = 0x4000_0000;
const PERI_BITBAND_REGION_END: u64 = 0x400F_FFFF;
const PERI_BITBAND_ALIAS_BASE: u64 = 0x4200_0000;
const SRAM_BITBAND_REGION_BASE: u64 = 0x2000_0000;
const SRAM_BITBAND_REGION_END: u64 = 0x200F_FFFF;
const SRAM_BITBAND_ALIAS_BASE: u64 = 0x2200_0000;

/// The 32-word bit-band alias range of a 4-byte register at `addr`, when the
/// register sits in a bit-band region: `(alias_start, alias_end_inclusive)`.
/// Used by the verify hwaddrs whitelist -- the emitter writes single-bit
/// fields through these aliases, and IKOS must know they are MMIO, not wild
/// pointers.
#[must_use]
pub fn bitband_alias_range(addr: u64) -> Option<(u64, u64)> {
    let base = if (PERI_BITBAND_REGION_BASE..=PERI_BITBAND_REGION_END).contains(&addr) {
        PERI_BITBAND_ALIAS_BASE + (addr - PERI_BITBAND_REGION_BASE) * 32
    } else if (SRAM_BITBAND_REGION_BASE..=SRAM_BITBAND_REGION_END).contains(&addr) {
        SRAM_BITBAND_ALIAS_BASE + (addr - SRAM_BITBAND_REGION_BASE) * 32
    } else {
        return None;
    };
    // 4 register bytes x 8 bits x 4 alias bytes per bit.
    Some((base, base + 4 * 8 * 4 - 1))
}

#[must_use]
pub fn bitband_alias(addr: u64, bits: &BitSpec) -> Option<u64> {
    let bit = match bits {
        BitSpec::Single(n) => *n as u64,
        BitSpec::Range(..) => return None,
    };
    if (PERI_BITBAND_REGION_BASE..=PERI_BITBAND_REGION_END).contains(&addr) {
        let byte_offset = addr - PERI_BITBAND_REGION_BASE;
        Some(PERI_BITBAND_ALIAS_BASE + byte_offset * 32 + bit * 4)
    } else if (SRAM_BITBAND_REGION_BASE..=SRAM_BITBAND_REGION_END).contains(&addr) {
        let byte_offset = addr - SRAM_BITBAND_REGION_BASE;
        Some(SRAM_BITBAND_ALIAS_BASE + byte_offset * 32 + bit * 4)
    } else {
        None
    }
}

/// How the matching `emit_critical_leave` undoes an `emit_critical_enter`.
pub enum CsToken {
    /// Global mask (`cpsid i`); leave re-enables with `cpsie i`.
    Primask,
    /// BASEPRI ceiling mask; holds the SSA register with the caller's
    /// BASEPRI, restored on leave.
    Basepri(String),
}

/// Mask out exactly the contenders. With a real ISR ceiling on ARMv7(E)-M
/// this raises BASEPRI to the ceiling's hardware priority: ISRs above the
/// ceiling cannot touch the data and keep running, where a PRIMASK mask
/// would add latency to interrupts that have no business being blocked.
/// `BASEPRI_MAX` (not BASEPRI) so entering from an already-masked context can
/// only tighten, never loosen -- an Any-context fn called from a
/// higher-priority ISR stays safe.
///
/// Falls back to the global `cpsid i` mask when:
/// - the arch is ARMv6-M (BASEPRI does not exist there),
/// - the ceiling is 0 (its hardware encoding is BASEPRI=0, which means
///   "masking disabled" -- only the global mask excludes priority-0
///   contenders),
/// - the ceiling is not a real ISR priority (the 255 thread-level sentinel,
///   or no ceiling known at all).
///
/// Known limitation: Cortex-M7 r0p1 (erratum 837070) needs MSR BASEPRI
/// wrapped in cpsid/cpsie; no shipped target uses that revision.
pub fn emit_critical_enter(e: &mut IrEmitter, ceiling: Option<u8>) -> CsToken {
    let is_real_isr_prio = |c: u8| c > 0 && u16::from(c) < (1u16 << u16::from(e.priority_bits));
    let hw = match ceiling {
        Some(c) if !matches!(e.arch, crate::arch::Arch::Armv6m) && is_real_isr_prio(c) => {
            u32::from(c) << (8 - u32::from(e.priority_bits))
        }
        _ => {
            let reg = e.new_reg();
            e.line(&format!(
                "{reg} = call i32 asm sideeffect \"cpsid i\", \"={{r12}},~{{memory}}\"()"
            ));
            return CsToken::Primask;
        }
    };
    let saved = e.new_reg();
    e.line(&format!(
        "{saved} = call i32 asm sideeffect \"mrs $0, basepri\", \"=r\"()"
    ));
    e.line(&format!(
        "call void asm sideeffect \"msr basepri_max, $0\", \"r,~{{memory}}\"(i32 {hw})"
    ));
    CsToken::Basepri(saved)
}

pub fn emit_critical_leave(e: &mut IrEmitter, token: CsToken) {
    match token {
        CsToken::Basepri(saved) => {
            e.line(&format!(
                "call void asm sideeffect \"msr basepri, $0\", \"r,~{{memory}}\"(i32 {saved})"
            ));
        }
        CsToken::Primask => {
            let reg = e.new_reg();
            e.line(&format!(
                "{reg} = call i32 asm sideeffect \"cpsie i\", \"={{r12}},~{{memory}}\"()"
            ));
        }
    }
}

pub fn emit_tailchain_prologue(e: &mut IrEmitter, has_calls: bool) {
    if has_calls {
        e.line("call void asm sideeffect \"push {lr}\", \"\"()");
    }
}

pub fn emit_tailchain_epilogue(e: &mut IrEmitter, has_calls: bool) {
    if has_calls {
        e.line("call void asm sideeffect \"pop {pc}\", \"\"()");
    } else {
        e.line("call void asm sideeffect \"bx lr\", \"\"()");
    }
    e.line("unreachable");
}

/// Program the NVIC IPR of every `@isr` IRQ from its declared priority.
/// Emitted in the reset handler (core0) AND in the prologue of every
/// declared core entry: the NVIC is BANKED per core, and a secondary core
/// never runs the reset handler, so its IPRs would otherwise stay
/// unprogrammed -- the ceiling model's priorities would be fiction there.
/// Writing the full set on every core is exact: the registers are banked,
/// and an IRQ a core never enables ignores its priority.
pub fn emit_ipr_stores(e: &mut IrEmitter, isr_priorities: &[(u16, u8)]) {
    let ptr_ty = e.arch.ptr_type();
    let shift = 8 - u32::from(e.priority_bits);
    if matches!(e.arch, crate::arch::Arch::Armv6m) {
        // ARMv6-M: the IPR registers are word-access-only (a byte store is
        // unpredictable). At reset every IPR is zero and this is the only
        // writer, so compose the four byte lanes of each touched IPR word
        // and store it whole -- exact, no read-modify-write needed.
        let mut words: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
        for (irq, priority) in isr_priorities {
            let lane = (u32::from(*irq) % 4) * 8;
            let val = (u32::from(*priority) << shift) & 0xFF;
            *words.entry(u32::from(*irq) / 4).or_insert(0) |= val << lane;
        }
        for (word, val) in words {
            let addr = 0xE000_E400u32 + 4 * word;
            e.line(&format!(
                "store volatile i32 {val}, ptr inttoptr ({ptr_ty} {addr} to ptr)"
            ));
        }
    } else {
        for (irq, priority) in isr_priorities {
            let addr = 0xE000_E400u32 + u32::from(*irq);
            let val = (u32::from(*priority) << shift) & 0xFF;
            e.line(&format!(
                "store volatile i8 {val}, ptr inttoptr ({ptr_ty} {addr} to ptr)"
            ));
        }
    }
}

pub fn emit_module_attributes(e: &mut IrEmitter) {
    // `no-builtins`: bml output is freestanding -- without it the optimizer
    // recognizes zero/copy loops and emits __aeabi_memclr/__aeabi_memcpy
    // libcalls nothing provides (first hit: a 48-byte buffer clear on
    // thumbv6m became an undefined __aeabi_memclr4 at link).
    e.line("attributes #0 = { nounwind \"no-builtins\" }");
    e.line("attributes #1 = { nounwind \"interrupt\" \"no-builtins\" }");
    e.line("attributes #2 = { nounwind optnone noinline \"no-builtins\" }");
}

pub fn emit_vector_table<S: ::std::hash::BuildHasher>(
    e: &mut IrEmitter,
    program: &Program,
    symbols: &SymbolTable,
    target_interrupts: &HashMap<String, u16, S>,
) {
    let is_armv6m = matches!(e.arch, crate::arch::Arch::Armv6m);
    let system_exceptions: &[(&str, usize)] = if is_armv6m {
        // ARMv6-M (Cortex-M0/M0+): only NMI, HardFault, SVCall, PendSV, SysTick
        // Slots 4-10, 12-13 are reserved
        &[
            ("NMI", 2),
            ("HardFault", 3),
            ("SVC", 11),
            ("PendSV", 14),
            ("SysTick", 15),
        ]
    } else {
        &[
            ("NMI", 2),
            ("HardFault", 3),
            ("MemManage", 4),
            ("BusFault", 5),
            ("UsageFault", 6),
            ("SVC", 11),
            ("DebugMon", 12),
            ("PendSV", 14),
            ("SysTick", 15),
        ]
    };
    let reserved_slots: &[usize] = if is_armv6m {
        &[4, 5, 6, 7, 8, 9, 10, 12, 13]
    } else {
        &[7, 8, 9, 10, 13]
    };

    let mut labeled: HashMap<String, (&str, u8)> = HashMap::new();
    let mut unlabeled: Vec<(String, u8)> = Vec::new();

    for item in &program.items {
        let (name, isr) = match item {
            ast::Item::FnDef(f) => (f.name.0.as_str(), &f.isr),
            ast::Item::ExternFnDef(f) => (f.name.0.as_str(), &f.isr),
            _ => continue,
        };
        if let Some(isr) = isr {
            if let Some(label) = &isr.label {
                labeled.insert(label.clone(), (name, isr.priority));
            } else {
                unlabeled.push((name.to_string(), isr.priority));
            }
        }
    }

    // `(irq, priority)` for every `@isr` that lands in an NVIC slot, collected
    // while the table is assembled below; the generated reset handler programs
    // the IPR bytes from it (system-exception slots use SHPR, not modeled yet).
    let mut isr_priorities: Vec<(u16, u8)> = Vec::new();

    let default_handler_name = if symbols.functions.contains_key("Default_Handler") {
        "Default_Handler"
    } else {
        "default_handler"
    };
    if default_handler_name == "default_handler" {
        e.counter = 0;
        e.line("define void @default_handler() #1 {");
        e.line("entry:");
        e.line("  br label %halt_loop");
        e.line("halt_loop:");
        e.line("  br label %halt_loop");
        e.line("}");
        e.line("");
    }

    let user_reset = labeled.get("Reset").map(|s| s.0.to_string()).or_else(|| {
        ["reset_handler", "Reset_Handler"]
            .iter()
            .find(|n| symbols.functions.contains_key(**n))
            .copied()
            .map(String::from)
    });

    // The generated reset handler is emitted AFTER the table entries are
    // assembled (it needs the collected `@isr` priorities); only its name is
    // needed here. With a user-written reset handler nothing is generated, so
    // the priorities stay unprogrammed -- same limitation as startup_init/MPU.
    let generate_reset = user_reset.is_none();
    let reset_fn = user_reset.unwrap_or_else(|| "reset_handler".to_string());

    let mut entries: Vec<String> = vec![String::new(); 16];
    entries[0] = "@_stack_top".to_string();
    entries[1] = format!("@{reset_fn}");

    for &(label, slot) in system_exceptions {
        entries[slot] = if let Some((fn_name, _)) = labeled.get(label) {
            format!("@{fn_name}")
        } else if symbols.functions.contains_key(label) {
            format!("@{label}")
        } else {
            format!("@{default_handler_name}")
        };
    }
    for &slot in reserved_slots {
        entries[slot] = "null".to_string();
    }

    let max_irq = target_interrupts.values().max().copied().unwrap_or(0) as usize;
    let irq_start = 16;
    let irq_count = max_irq + 1;
    let total = irq_start + irq_count;
    entries.resize(total, format!("@{default_handler_name}"));

    for (label, slot) in target_interrupts {
        let index = irq_start + *slot as usize;
        if index >= entries.len() {
            entries.resize(index + 1, format!("@{default_handler_name}"));
        }
        if let Some((fn_name, priority)) = labeled.get(label) {
            entries[index] = format!("@{fn_name}");
            isr_priorities.push((*slot, *priority));
        } else if symbols.functions.contains_key(label) {
            entries[index] = format!("@{label}");
        } else if symbols
            .functions
            .contains_key(&format!("{label}_IRQHandler"))
        {
            entries[index] = format!("@{label}_IRQHandler");
        }
    }

    let mut unlabeled_idx = irq_start;
    for (fn_name, priority) in &unlabeled {
        while unlabeled_idx < entries.len()
            && entries[unlabeled_idx] != format!("@{default_handler_name}")
        {
            unlabeled_idx += 1;
        }
        if unlabeled_idx >= entries.len() {
            entries.push(format!("@{fn_name}"));
        } else {
            entries[unlabeled_idx] = format!("@{fn_name}");
        }
        isr_priorities.push(((unlabeled_idx - irq_start) as u16, *priority));
        unlabeled_idx += 1;
    }

    if generate_reset {
        e.isr_priorities.clone_from(&isr_priorities);
        emit_startup_routine(e, symbols, &isr_priorities);
    }

    e.line(&format!(
        "@vector_table = global [{} x ptr] [",
        entries.len()
    ));
    for (i, entry) in entries.iter().enumerate() {
        let comma = if i + 1 < entries.len() { "," } else { "" };
        e.line(&format!("  ptr {entry}{comma}"));
    }
    e.line("], section \".vector_table\"");
    e.line("");

    e.line("@_stack_top = external global i32");
    e.line("");

    emit_module_attributes(e);
}

#[allow(clippy::similar_names)] // rbar/rlar are the architectural names
pub fn emit_startup_routine(
    e: &mut IrEmitter,
    symbols: &SymbolTable,
    isr_priorities: &[(u16, u8)],
) {
    e.counter = 0;
    let has_main = symbols.functions.contains_key("main");

    e.line("@_sdata = external global i8");
    e.line("@_edata = external global i8");
    e.line("@_sidata = external global i8");
    e.line("@_sbss = external global i8");
    e.line("@_ebss = external global i8");
    e.line("");

    e.line("define void @reset_handler() #2 {");
    e.indent += 1;

    let ptr_ty = e.arch.ptr_type();

    e.line("entry:");
    // Target-specific MMIO init (Target::startup_init), applied before the
    // .data/.bss copy below -- the CMSIS SystemInit ordering. Each is a
    // read-modify-write OR (`*addr |= mask`), typically to ungate a RAM clock
    // whose SRAM holds the stack/.data/.bss (e.g. STM32H7 D2 SRAM). This runs
    // with registers only, so it is safe even though RAM is not yet usable.
    let startup_init = e.startup_init.clone();
    for (addr, mask) in startup_init {
        let p = format!("inttoptr ({ptr_ty} {addr} to ptr)");
        let old = e.emit_line(&format!("load volatile i32, ptr {p}"));
        let updated = e.emit_line(&format!("or i32 {old}, {mask}"));
        e.line(&format!("store volatile i32 {updated}, ptr {p}"));
    }

    // MPU: program each `cacheable = false` mem block as a non-cacheable region
    // (Target::mpu_regions) so a CPU with caches on stays coherent with the DMA
    // agents sharing it -- turning the trusted claim into enforced config.
    // Register-only, before .data/.bss and before any cache is enabled. RASR is
    // Normal non-cacheable shareable (TEX=001, S=1, C=0, B=0), full RW (AP=011),
    // execute-never (XN=1).
    let mpu = e.mpu_regions.clone();
    if !mpu.is_empty() {
        const MPU_CTRL: u32 = 0xE000_ED94;
        const MPU_RNR: u32 = 0xE000_ED98;
        const MPU_RBAR: u32 = 0xE000_ED9C;
        // PMSAv7: RASR. PMSAv8: RLAR (same address, different layout) + MAIR.
        const MPU_RASR_RLAR: u32 = 0xE000_EDA0;
        const MPU_MAIR0: u32 = 0xE000_EDC0;
        // Disable the MPU while reconfiguring.
        e.line(&format!(
            "store volatile i32 0, ptr inttoptr ({ptr_ty} {MPU_CTRL} to ptr)"
        ));
        match e.mpu_flavor {
            crate::arch::MpuFlavor::Pmsa7 => {
                for (i, (base, size)) in mpu.iter().enumerate() {
                    let size_field = size.trailing_zeros() - 1; // SIZE = log2(size) - 1
                    let rasr: u32 =
                        1 | (size_field << 1) | (1 << 18) | (1 << 19) | (0b011 << 24) | (1 << 28);
                    let base = *base as u32;
                    e.line(&format!(
                        "store volatile i32 {i}, ptr inttoptr ({ptr_ty} {MPU_RNR} to ptr)"
                    ));
                    e.line(&format!(
                        "store volatile i32 {base}, ptr inttoptr ({ptr_ty} {MPU_RBAR} to ptr)"
                    ));
                    e.line(&format!(
                        "store volatile i32 {rasr}, ptr inttoptr ({ptr_ty} {MPU_RASR_RLAR} to ptr)"
                    ));
                }
            }
            crate::arch::MpuFlavor::Pmsa8 => {
                // Attribute indirection: MAIR0 attr index 0 = 0x44, Normal
                // memory, outer and inner non-cacheable. Every generated
                // region uses index 0 (they exist precisely to be
                // non-cacheable).
                e.line(&format!(
                    "store volatile i32 68, ptr inttoptr ({ptr_ty} {MPU_MAIR0} to ptr)"
                ));
                for (i, (base, size)) in mpu.iter().enumerate() {
                    // RBAR = BASE[31:5] | SH=00 | AP=01 (RW any privilege) |
                    // XN=1. Shareability is architecturally ignored for
                    // Normal Non-cacheable memory, so SH stays 0.
                    let rbar: u32 = (*base as u32 & 0xFFFF_FFE0) | 0b011;
                    // RLAR = LIMIT[31:5] (inclusive, last 32-byte granule) |
                    // AttrIndx=0 | EN=1.
                    let rlar: u32 = (((*base + *size - 32) as u32) & 0xFFFF_FFE0) | 1;
                    e.line(&format!(
                        "store volatile i32 {i}, ptr inttoptr ({ptr_ty} {MPU_RNR} to ptr)"
                    ));
                    e.line(&format!(
                        "store volatile i32 {rbar}, ptr inttoptr ({ptr_ty} {MPU_RBAR} to ptr)"
                    ));
                    e.line(&format!(
                        "store volatile i32 {rlar}, ptr inttoptr ({ptr_ty} {MPU_RASR_RLAR} to ptr)"
                    ));
                }
            }
        }
        // Enable the MPU (ENABLE | PRIVDEFENA), then barriers so it is active
        // before any later access to the region.
        e.line(&format!(
            "store volatile i32 5, ptr inttoptr ({ptr_ty} {MPU_CTRL} to ptr)"
        ));
        e.line("call void asm sideeffect \"dsb\", \"~{memory}\"()");
        e.line("call void asm sideeffect \"isb\", \"~{memory}\"()");
    }

    // NVIC priorities: program the IPR of every `@isr` IRQ from its declared
    // priority. `@isr(priority=N)` is physics the ceiling model reasons
    // over, so the compiler grounds it instead of trusting a hand-written
    // IPR to match. The *enable* (ISER) deliberately stays application
    // code: enabling at reset could fire an ISR before its peripheral is
    // initialized -- priority is static configuration, enable is runtime
    // policy.
    emit_ipr_stores(e, isr_priorities);

    e.line("  br label %data_copy_test");
    e.line("");

    // Named values for phi-node back-edges (values from successor blocks
    // referenced by the phi before their definition).
    let data_src_next = "%__phi.data_src_next".to_string();
    let data_dst_next = "%__phi.data_dst_next".to_string();
    let bss_next = "%__phi.bss_next".to_string();

    e.indent -= 1;
    e.line("data_copy_test:");
    e.indent += 1;
    let src = e.emit_line(&format!(
        "phi ptr [ @_sidata, %entry ], [ {data_src_next}, %data_copy_body ]"
    ));
    let dst = e.emit_line(&format!(
        "phi ptr [ @_sdata, %entry ], [ {data_dst_next}, %data_copy_body ]"
    ));
    let dst_int = e.emit_line(&format!("ptrtoint ptr {dst} to {ptr_ty}"));
    let edata_int = e.emit_line(&format!("ptrtoint ptr @_edata to {ptr_ty}"));
    let data_done = e.emit_line(&format!("icmp eq {ptr_ty} {dst_int}, {edata_int}"));
    e.line(&format!(
        "br i1 {data_done}, label %bss_zero_init, label %data_copy_body"
    ));
    e.line("");

    e.line("data_copy_body:");
    // Word-wise on purpose (the .ld ALIGN(4)s the section bounds): byte-wise
    // init is slower and, on ECC RAMs (STM32H7 RAMECC), every byte store
    // RMWs an ECC-uninitialized word and latches noise error flags that
    // poison later fault forensics. Word stores establish ECC cleanly.
    let val = e.emit_line(&format!("load volatile i32, ptr {src}"));
    e.line(&format!("store volatile i32 {val}, ptr {dst}"));
    e.line(&format!(
        "{data_src_next} = getelementptr i32, ptr {src}, i32 1"
    ));
    e.line(&format!(
        "{data_dst_next} = getelementptr i32, ptr {dst}, i32 1"
    ));
    e.line("br label %data_copy_test");
    e.line("");

    e.line("bss_zero_init:");
    e.line("  br label %bss_test");
    e.line("");

    e.indent -= 1;
    e.line("bss_test:");
    e.indent += 1;
    let bss_ptr = e.emit_line(&format!(
        "phi ptr [ @_sbss, %bss_zero_init ], [ {bss_next}, %bss_body ]"
    ));
    let bss_int = e.emit_line(&format!("ptrtoint ptr {bss_ptr} to {ptr_ty}"));
    let ebss_int = e.emit_line(&format!("ptrtoint ptr @_ebss to {ptr_ty}"));
    let bss_done = e.emit_line(&format!("icmp eq {ptr_ty} {bss_int}, {ebss_int}"));
    let after_bss = if has_main { "call_main" } else { "halt_loop" };
    e.line(&format!(
        "br i1 {bss_done}, label %{after_bss}, label %bss_body"
    ));
    e.line("");

    e.line("bss_body:");
    e.line(&format!("store volatile i32 0, ptr {bss_ptr}"));
    e.line(&format!(
        "{bss_next} = getelementptr i32, ptr {bss_ptr}, i32 1"
    ));
    e.line("br label %bss_test");
    e.line("");

    if has_main {
        e.line("call_main:");
        e.line("  call void @main()");
        e.line("  br label %halt_loop");
        e.line("");
    }

    e.line("halt_loop:");
    e.line("  br label %halt_loop");

    e.indent -= 1;
    e.line("}");
    e.line("");
}
