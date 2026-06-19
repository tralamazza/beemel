//! Region/agent checks that need the program, the target file, and the symbol
//! table together.
//!
//! Unlike the type checker and borrow checker, this pass takes the `Target`:
//! regions and agents are declared in the target file (the hardware physics),
//! while placement (`in <region>`) and ownership (`owns`) live in source. This
//! is the seam where the two meet.
//!
//! Implemented so far (`doc/regions-agents.md`):
//! - Slice 1: placement-name check (`in <region>` names a real region), the
//!   no-initializer rule (region memory is NOBITS at startup), and the
//!   `in`/`@section` conflict.
//! - Slice 2a: `owns` path resolution against the peripheral table, and
//!   cross-module exclusivity (two modules cannot own the same register).
//! - Slice 2b: handoff write check -- a handoff register may only be written by
//!   the module that owns it (E605), off a single exhaustive write walk. The
//!   full byte address is written verbatim (no encoding), so there is no shift
//!   guard.
//! - In-memory handoffs: an `addr in R` struct field must name a real region
//!   (E607), and a descriptor delivered to an agent (`handoff = &RX`) must not
//!   contain an `addr in R` field for a region that agent cannot reach (E608) --
//!   the transitive-reach step, off the same write walk.
//! - Agent enablement: an agent that is programmed (one of its handoff registers
//!   written) must have its `enabled_by` clock-gate registers set somewhere
//!   (E609) -- clock-gate-before-touch, a whole-program presence check off the
//!   same write walk.
//! - Clock-stomp guard: a *disabling* write (`= false`/`0`) to an agent's
//!   `enabled_by` clock gate, from a module that does not own the agent, is
//!   rejected (E610). Clock enables are shared/idempotent, so only the disabling
//!   direction is guarded -- a stranger gating an agent's clock off silently
//!   stops it; the owning module may still manage its own clock.
//! - Port-select check: a handoff with `port_by F TAG` (software-selected
//!   master port, e.g. the H7 MDMA's `MDMA_CxTBR.SBUS/DBUS`) must have F match
//!   where the handed-off address lives: an address behind TAG-only bus
//!   windows requires F set, one behind no TAG window forbids it (E612) --
//!   same write walk, presence semantics like E609.
//!
//! The handoff provenance checks (slice 4) extend this module.

use crate::ast::{
    Block, Expr, Item, LValue, OwnsPath, Program, StaticDef, Stmt, StorageAnnotation, UnaryOp,
};
use crate::errors::DiagnosticBag;
use crate::resolver::SymbolTable;
use crate::source::{FileId, Span};
use crate::target::{Agent, AgentKind, Channel, DreqSpec, PortBy, Region, Target};
use crate::types::Type;
use std::collections::{HashMap, HashSet};

/// Derive `@dma`-style protection from placement (usage dictates declaration).
///
/// `@dma`'s load-bearing property is the index-read restriction: a `Dma`-wrapped
/// value cannot be indexed as an rvalue (the type checker's `index_element_type`
/// accepts only `Array`/`Ptr`/views, so `Dma(Array(..))` is rejected with E326),
/// while the store path unwraps the `Dma` first -- so `BUF[i] = x` is legal but
/// `let v = BUF[i]` is not. That stops software aliasing memory it has handed to
/// an agent.
///
/// Placement is otherwise orthogonal to type, so a `[u32;N] in R` would lose
/// that protection. Here we re-establish it without the hand-written `@dma`: an
/// array static placed `in R`, where `R`'s memory is operated on by a
/// concurrently-mutating agent (a DMA engine or external bus master, the agents
/// `@dma`/`@external` modeled), is wrapped in `Type::AgentShared`. The existing
/// E326 machinery then applies unchanged.
///
/// Runs after resolution and before the type checker, only when a target is
/// present (`bml check` has no target and skips it, like the other region
/// checks). Scoped to array types: E326 is an indexing restriction, and
/// agent-shared memory holds buffers/descriptors (`[u8;N]`/`[RxDesc;N]`).
pub fn apply_derived_move(program: &Program, target: &Target, symbols: &mut SymbolTable) {
    for item in &program.items {
        let Item::StaticDef(s) = item else {
            continue;
        };
        let Some((region_name, _)) = &s.region else {
            continue;
        };
        let Some(region) = target.regions.iter().find(|r| &r.name == region_name) else {
            continue;
        };
        if !region_concurrently_mutated(region, target) {
            continue;
        }
        let Some(sym) = symbols.statics.get_mut(&s.name.0) else {
            continue;
        };
        // Only arrays, and never double-wrap a hand-written `@dma`/`@external`
        // static (whose type is already a Move carrier, not a bare `Array`).
        if matches!(sym.ty, Type::Array(..)) {
            let inner = sym.ty.clone();
            sym.ty = Type::AgentShared(Box::new(inner));
        } else if let Type::Shared(inner, ceiling) = &sym.ty
            && matches!(**inner, Type::Array(..))
        {
            // `@shared in R` -- the composed mixed-sharer case (CPU contexts
            // AND an async agent). Nest the carriers Shared(AgentShared(..)):
            // outside a `claim` window the outer Shared blocks everything
            // including `reclaim` (its base must be AgentShared), so the
            // masked window is REQUIRED by construction; inside `claim` the
            // patched table strips the outer Shared and the static is the
            // plain agent-shared world -- reclaim, the E611 guards, and E326
            // compose unchanged. See doc/regions-agents.md (the fold).
            sym.ty = Type::Shared(
                Box::new(Type::AgentShared(Box::new((**inner).clone()))),
                *ceiling,
            );
        }
    }
}

/// Whether `region`'s memory is operated on by a concurrently-mutating agent (a
/// DMA engine or external bus master). A CPU- or debug-probe-only region is
/// normal memory and gets no derived protection.
fn region_concurrently_mutated(region: &Region, target: &Target) -> bool {
    region.agents.iter().any(|agent_name| {
        target.agents.iter().any(|a| {
            &a.name == agent_name && matches!(a.kind, AgentKind::Dma | AgentKind::External)
        })
    })
}

/// Run the region/agent checks.
pub fn check(program: &Program, symbols: &SymbolTable, target: &Target, diags: &mut DiagnosticBag) {
    for item in &program.items {
        match item {
            Item::StaticDef(s) => check_placement(s, target, diags),
            Item::Owns(o) => {
                for path in &o.paths {
                    resolve_owns_path(path, symbols, diags);
                }
            }
            // E607: an `addr in R` struct field must name a real region, else
            // the in-memory handoff obligation would be silently skipped.
            Item::StructDef(s) => {
                for field in &s.fields {
                    if let crate::ast::TypeExpr::Addr((region, span)) = &field.ty
                        && !target.regions.iter().any(|r| &r.name == region)
                    {
                        diags.error(
                            format!(
                                "field `{}.{}` is `addr in {region}`, but the target defines no \
                                 such region{}",
                                s.name.0,
                                field.name.0,
                                known_regions(target)
                            ),
                            "E607",
                            *span,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    check_ownership_exclusivity(program, symbols, diags);
    check_handoff_writes(program, symbols, target, diags);
    check_dreq(program, symbols, target, diags);
    check_reclaim_guards(program, symbols, target, diags);
    check_core_sharing(program, symbols, target, diags);
}

/// E615: cross-core sharing. A second `cpu`-kind agent with a declared
/// `entry` runs OUR code on another core; everything its entry transitively
/// mentions runs there (mention edges, same over-approximation as context
/// propagation). The per-core protections (`@shared` ceilings, `claim`)
/// mask interrupts on ONE core only -- they provide no exclusion across
/// cores -- so a mutable static reachable from two cores is rejected until a
/// cross-core mechanism (e.g. a hardware-spinlock-backed claim) exists.
/// Cross-core communication goes through MMIO channels (the SIO FIFOs) for
/// now. Module `const`s are immutable and freely shared.
///
/// The declared entry's core comes from the target and is PINNED: the
/// launcher necessarily takes the entry's address for the handshake, and
/// that mention must not put the entry (and its whole call tree) on the
/// launcher's core. Consequence, documented: directly CALLING another
/// core's entry is not detected.
///
/// ISR cores are DERIVED from usage: the NVIC is banked per core, so an IRQ
/// is delivered to whichever core enables it -- a LABELED `@isr` runs on
/// the core(s) whose code writes its ISER bit (an outer fixpoint with the
/// reach itself, since the enabler's core depends on reach). No visible
/// enable, an undecodable enable value, or an unlabeled ISR fall back
/// conservatively: core0 for the no-enable case (single-core programs are
/// unchanged), ALL cores for an ISER write whose value cannot be folded.
/// Core-reachability for every function and static: which cores' code can
/// touch them -- `(core names, fn -> core bitmask, static -> core bitmask)`,
/// `None` when no agent declares an entry (single-core). Shared by the E615
/// check and the cross-core lock assignment.
#[allow(clippy::type_complexity)]
fn core_reach<'p>(
    program: &'p Program,
    symbols: &SymbolTable,
    target: &'p Target,
) -> Option<(Vec<String>, HashMap<&'p str, u32>, HashMap<&'p str, u32>)> {
    let entries: Vec<(&Agent, &str)> = target
        .agents
        .iter()
        .filter_map(|a| a.entry.as_deref().map(|e| (a, e)))
        .collect();
    if entries.is_empty() {
        return None;
    }
    let mut core_names: Vec<String> = vec!["core0 (implicit)".to_string()];
    let mut pinned: HashMap<&str, u32> = HashMap::new();
    for (agent, entry) in &entries {
        if !symbols.functions.contains_key(*entry) {
            continue; // reported by check_core_sharing
        }
        let bit = 1u32 << core_names.len();
        core_names.push(agent.name.clone());
        pinned.insert(*entry, bit);
    }
    let fn_names: HashSet<&str> = symbols.functions.keys().map(String::as_str).collect();
    let edges = crate::ceiling::fn_mentions(program, &fn_names);
    let all_cores_mask: u32 = (1u32 << core_names.len()) - 1;

    // Labeled ISRs and their IRQ numbers (per-core assignment needs the
    // ISER bit, so unlabeled ISRs keep the core0 default).
    let mut isr_irq: HashMap<&str, u32> = HashMap::new();
    for item in &program.items {
        if let Item::FnDef(f) = item
            && let Some(isr) = &f.isr
            && let Some(label) = &isr.label
            && let Some(&irq) = target.interrupts.get(label)
        {
            isr_irq.insert(f.name.0.as_str(), u32::from(irq));
        }
    }

    // NVIC ISER writes: `(containing fn, enabled-IRQ bits or None)` --
    // recognized by the peripheral's ADDRESS (0xE000E100, banked per core),
    // word index from the register offset. `None` bits = the value did not
    // fold; conservatively enables everything.
    let mut iser_writes: Vec<(&str, Option<Vec<u32>>)> = Vec::new();
    {
        let mut writes = Vec::new();
        collect_peripheral_writes(program, symbols, &mut writes);
        for w in &writes {
            let Some(periph) = symbols.peripherals.get(&w.periph) else {
                continue;
            };
            let Some(reg) = periph.regs.get(&w.reg) else {
                continue;
            };
            let addr = periph.base_addr + reg.offset;
            if !(0xE000_E100..0xE000_E140).contains(&addr) {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)]
            let word = ((addr - 0xE000_E100) / 4) as u32;
            let Some(owner) = program.items.iter().find_map(|i| {
                if let Item::FnDef(f) = i
                    && f.body.span.file == w.span.file
                    && f.body.span.start <= w.span.start
                    && w.span.end <= f.body.span.end
                {
                    Some(f.name.0.as_str())
                } else {
                    None
                }
            }) else {
                continue;
            };
            let irqs = w.rhs_literal.map(|v| {
                (0..32u32)
                    .filter(|b| v & (1u64 << b) != 0)
                    .map(|b| 32 * word + b)
                    .collect::<Vec<u32>>()
            });
            iser_writes.push((owner, irqs));
        }
    }

    // Outer fixpoint: seed ISRs (derived from their enablers' cores, default
    // core0), run the mention-edge reach, re-derive -- enabler cores can
    // grow as reach grows, so iterate until stable.
    let mut isr_seed: HashMap<&str, u32> = HashMap::new();
    let mut cores: HashMap<&str, u32>;
    loop {
        cores = HashMap::new();
        for item in &program.items {
            if let Item::FnDef(f) = item {
                let name = f.name.0.as_str();
                if name == "main" {
                    *cores.entry(name).or_default() |= 1;
                } else if f.isr.is_some() {
                    *cores.entry(name).or_default() |= isr_seed.get(name).copied().unwrap_or(1);
                }
            }
        }
        for (entry, bit) in &pinned {
            *cores.entry(entry).or_default() |= bit;
        }
        loop {
            let mut changed = false;
            for (caller, callees) in &edges {
                let Some(&caller_cores) = cores.get(*caller) else {
                    continue;
                };
                for callee in callees {
                    if pinned.contains_key(*callee) {
                        continue;
                    }
                    let c = cores.entry(callee).or_default();
                    let merged = *c | caller_cores;
                    changed |= merged != *c;
                    *c = merged;
                }
            }
            if !changed {
                break;
            }
        }
        // Re-derive ISR seeds from where their ISER bits are written.
        let mut new_seed: HashMap<&str, u32> = HashMap::new();
        for (isr, irq) in &isr_irq {
            let mut mask = 0u32;
            for (owner, irqs) in &iser_writes {
                let hits = match irqs {
                    Some(list) => list.contains(irq),
                    None => true, // unfoldable value: could enable anything
                };
                if hits && let Some(&oc) = cores.get(*owner) {
                    mask |= if irqs.is_some() { oc } else { all_cores_mask };
                }
            }
            if mask != 0 {
                new_seed.insert(*isr, mask);
            }
        }
        if new_seed == isr_seed {
            break;
        }
        isr_seed = new_seed;
    }
    let static_names: HashSet<&str> = program
        .items
        .iter()
        .filter_map(|i| {
            if let Item::StaticDef(sd) = i {
                Some(sd.name.0.as_str())
            } else {
                None
            }
        })
        .collect();
    let accesses = crate::ceiling::fn_mentions(program, &static_names);
    let mut static_cores: HashMap<&str, u32> = HashMap::new();
    for (f, statics) in &accesses {
        let Some(&fc) = cores.get(*f) else { continue };
        for st in statics {
            *static_cores.entry(st).or_default() |= fc;
        }
    }
    Some((core_names, cores, static_cores))
}

/// Hardware-spinlock assignment for cross-core `@shared` statics: each
/// multi-core-reachable `@shared` static gets a deterministic lock index
/// (declaration-name order). Consumed by the emitter (the cross-core claim
/// window lowers to mask + spin-acquire + release) and validated by
/// `check_core_sharing` (every access must sit inside a claim window).
#[must_use]
pub fn cross_core_locks(
    program: &Program,
    symbols: &SymbolTable,
    target: &Target,
) -> HashMap<String, u32> {
    let Some((_, _, static_cores)) = core_reach(program, symbols, target) else {
        return HashMap::new();
    };
    let mut names: Vec<&str> = static_cores
        .iter()
        .filter(|(name, c)| {
            c.count_ones() > 1
                && symbols.statics.get(**name).is_some_and(|sym| {
                    sym.storage
                        .iter()
                        .any(|a| matches!(a, StorageAnnotation::Shared(_)))
                })
        })
        .map(|(name, _)| *name)
        .collect();
    names.sort_unstable();
    names
        .into_iter()
        .enumerate()
        .map(|(i, n)| (n.to_string(), i as u32))
        .collect()
}

fn check_core_sharing(
    program: &Program,
    symbols: &SymbolTable,
    target: &Target,
    diags: &mut DiagnosticBag,
) {
    let entries: Vec<(&Agent, &str)> = target
        .agents
        .iter()
        .filter_map(|a| a.entry.as_deref().map(|e| (a, e)))
        .collect();
    if entries.is_empty() {
        return; // single-core program: nothing to check
    }

    // Report-anchor span for definition-level errors (no natural write site).
    let anchor = program.items.iter().find_map(|i| {
        if let Item::FnDef(f) = i {
            Some(f.name.1)
        } else {
            None
        }
    });
    for (agent, entry) in &entries {
        if !symbols.functions.contains_key(*entry)
            && let Some(span) = anchor
        {
            diags.error(
                format!(
                    "agent `{}` declares `entry = {entry}`, but no such function is defined",
                    agent.name
                ),
                "E615",
                span,
            );
        }
    }

    let Some((core_names, _, static_cores)) = core_reach(program, symbols, target) else {
        return;
    };

    // For the `@shared` relaxation: every claim window (target name + body
    // span) and every mention of a tracked static (name + span).
    let locks = cross_core_locks(program, symbols, target);
    let tracked: HashSet<&str> = locks.keys().map(String::as_str).collect();
    let (windows, mentions) = claims_and_mentions(program, &tracked);

    for item in &program.items {
        if let Item::StaticDef(sd) = item {
            let c = static_cores.get(sd.name.0.as_str()).copied().unwrap_or(0);
            if c.count_ones() <= 1 {
                continue;
            }
            let names: Vec<&str> = core_names
                .iter()
                .enumerate()
                .filter(|(i, _)| c & (1 << i) != 0)
                .map(|(_, n)| n.as_str())
                .collect();
            let is_shared = locks.contains_key(&sd.name.0);
            if !is_shared {
                diags.error(
                    format!(
                        "`{}` is reachable from multiple cores ({}). Per-core protections mask \
                         interrupts on one core only. Partition the data per core, communicate \
                         through an MMIO channel (e.g. the SIO FIFOs), or annotate it `@shared` \
                         and access it only inside `claim {} {{ ... }}` windows (hardware-\
                         spinlock backed, when the target declares spinlock physics).",
                        sd.name.0,
                        names.join(", "),
                        sd.name.0
                    ),
                    "E615",
                    sd.name.1,
                );
                continue;
            }
            // Cross-core `@shared`: needs spinlock physics, a free lock, and
            // every access inside a `claim` window of this static.
            if target.spinlock_base.is_none() {
                diags.error(
                    format!(
                        "`{}` is `@shared` across cores ({}), but the target declares no \
                         hardware spinlocks (`spinlock_base` / `spinlock_count`) to back the \
                         cross-core claim window.",
                        sd.name.0,
                        names.join(", ")
                    ),
                    "E615",
                    sd.name.1,
                );
                continue;
            }
            let idx = locks[&sd.name.0];
            if idx >= target.spinlock_count {
                diags.error(
                    format!(
                        "`{}` needs hardware spinlock {} but the target declares only {} \
                         (`spinlock_count`).",
                        sd.name.0, idx, target.spinlock_count
                    ),
                    "E615",
                    sd.name.1,
                );
                continue;
            }
            for (mname, mspan) in &mentions {
                if mname != &sd.name.0 {
                    continue;
                }
                let inside = windows.iter().any(|(wname, wspan)| {
                    wname == &sd.name.0
                        && wspan.file == mspan.file
                        && wspan.start <= mspan.start
                        && mspan.end <= wspan.end
                });
                if !inside {
                    diags.error(
                        format!(
                            "`{}` is `@shared` across cores ({}); every access must be inside \
                             a `claim {} {{ ... }}` window -- per-access critical sections \
                             cannot exclude the other core.",
                            sd.name.0,
                            names.join(", "),
                            sd.name.0
                        ),
                        "E615",
                        *mspan,
                    );
                }
            }
        }
    }
}

// ---- slice 1: placement -----------------------------------------------------

fn check_placement(s: &StaticDef, target: &Target, diags: &mut DiagnosticBag) {
    let Some((region_name, region_span)) = &s.region else {
        return;
    };

    // E600: the placement names a region the target does not define.
    if !target.regions.iter().any(|r| &r.name == region_name) {
        let known = known_regions(target);
        diags.error(
            format!(
                "`{}` is placed `in {region_name}`, but the target defines no such region{known}",
                s.name.0
            ),
            "E600",
            *region_span,
        );
    }

    // E601: region memory is not initialized at startup. The `.region.*`
    // section links as NOBITS and is in neither the `.data` copy nor the `.bss`
    // clear, so an initializer would be silently dropped (verified: the linked
    // ELF has no PROGBITS for it). Require runtime initialization instead --
    // which is how every agent-shared buffer is set up anyway (descriptors and
    // buffers are written before the DMA engine is enabled).
    if s.init.is_some() {
        diags.error(
            format!(
                "`{}` is placed `in {region_name}` and cannot have an initializer: region \
                 memory is not initialized at startup. Drop the `= ...` and set it at runtime \
                 before the agent uses it.",
                s.name.0
            ),
            "E601",
            s.name.1,
        );
    }

    // E602: `in <region>` and an explicit `@section(...)` both set the static's
    // output section -- they would silently fight. Placement wins in codegen,
    // so reject the combination rather than ignore the `@section`.
    if s.storage
        .iter()
        .any(|a| matches!(a, StorageAnnotation::Section(_)))
    {
        diags.error(
            format!(
                "`{}` has both `in {region_name}` and `@section(...)`; a region already \
                 determines the output section. Remove the `@section`.",
                s.name.0
            ),
            "E602",
            s.name.1,
        );
    }

    // `@shared` + `in <region>` (the mixed-sharer composition) is legal: the
    // carriers nest Shared(AgentShared(..)) (see apply_derived_move), so
    // consumption requires BOTH ownership windows -- `claim` for the CPU
    // mutual exclusion and the completion-guarded `reclaim` inside it for
    // the agent handshake. (E613, which rejected the combination while it
    // was only safe by accident, is retired.)
}

/// A " (known regions: a, b)" suffix for the diagnostic, or a hint when the
/// target declares none at all.
fn known_regions(target: &Target) -> String {
    if target.regions.is_empty() {
        " (the target file declares no [region.*] sections)".to_string()
    } else {
        let names: Vec<&str> = target.regions.iter().map(|r| r.name.as_str()).collect();
        format!(" (known regions: {})", names.join(", "))
    }
}

// ---- slice 2a: ownership ----------------------------------------------------

/// What a single `owns` path covers. Field-level ownership is not yet
/// supported (rejected in the parser), so a claim is either a whole peripheral
/// or one register.
enum Claim {
    Peripheral(String),
    Register(String, String),
    /// `owns gpio[lo..hi]` -- an inclusive GPIO-pin range.
    Pins(u64, u64),
}

impl Claim {
    /// Whether two claims cover any common resource (register or pin). Register
    /// and pin claims never overlap each other -- different resource kinds.
    fn overlaps(&self, other: &Claim) -> bool {
        match (self, other) {
            (Claim::Peripheral(a), Claim::Peripheral(b)) => a == b,
            (Claim::Peripheral(p), Claim::Register(rp, _))
            | (Claim::Register(rp, _), Claim::Peripheral(p)) => p == rp,
            (Claim::Register(p1, r1), Claim::Register(p2, r2)) => p1 == p2 && r1 == r2,
            // Two inclusive ranges intersect iff each starts at or before the
            // other ends.
            (Claim::Pins(lo1, hi1), Claim::Pins(lo2, hi2)) => lo1 <= hi2 && lo2 <= hi1,
            _ => false,
        }
    }

    /// `true` for a GPIO-pin claim (drives the E650-vs-E604 error split).
    fn is_pins(&self) -> bool {
        matches!(self, Claim::Pins(_, _))
    }

    fn display(&self) -> String {
        match self {
            Claim::Peripheral(p) => p.clone(),
            Claim::Register(p, r) => format!("{p}.{r}"),
            Claim::Pins(lo, hi) => format!("gpio[{lo}..{hi}]"),
        }
    }
}

/// Resolve an `owns` path against the peripheral table. Returns the claim when
/// it resolves; emits E603 and returns `None` otherwise.
fn resolve_owns_path(
    path: &OwnsPath,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) -> Option<Claim> {
    let (peripheral, register) = match &path.target {
        crate::ast::OwnsTarget::Gpio { lo, hi } => return Some(Claim::Pins(*lo, *hi)),
        crate::ast::OwnsTarget::Reg {
            peripheral,
            register,
        } => (peripheral, register),
    };
    let pname = &peripheral.0;
    let Some(periph) = symbols.peripherals.get(pname) else {
        diags.error(
            format!("`owns {pname}`: no peripheral named `{pname}`"),
            "E603",
            path.span,
        );
        return None;
    };
    match register {
        None => Some(Claim::Peripheral(pname.clone())),
        Some((rname, _)) => {
            if periph.regs.contains_key(rname) {
                Some(Claim::Register(pname.clone(), rname.clone()))
            } else {
                diags.error(
                    format!(
                        "`owns {pname}.{rname}`: peripheral `{pname}` has no register `{rname}`"
                    ),
                    "E603",
                    path.span,
                );
                None
            }
        }
    }
}

/// E604: a register owned by two different modules. Ownership is a claim about
/// *other* modules, so the conflict is across source files (identified by the
/// span's `FileId`); two `owns` of the same register in one file are not a
/// conflict.
fn check_ownership_exclusivity(
    program: &Program,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) {
    // Collect every resolvable claim with its file and span.
    let mut claims: Vec<(Claim, Span)> = Vec::new();
    for item in &program.items {
        if let Item::Owns(o) = item {
            for path in &o.paths {
                // Resolve quietly here: unresolved paths already reported E603
                // in the main walk; skip them rather than double-reporting.
                if let Some(claim) = resolve_owns_path_quiet(path, symbols) {
                    claims.push((claim, path.span));
                }
            }
        }
    }

    // O(n^2) over a small set: flag the first cross-file overlap for each claim.
    for (i, (claim_i, span_i)) in claims.iter().enumerate() {
        for (claim_j, span_j) in claims.iter().take(i) {
            if span_i.file != span_j.file && claim_i.overlaps(claim_j) {
                if claim_i.is_pins() {
                    // E650: GPIO pins driven by two modules. Two state machines
                    // (in different modules) configured for overlapping pins
                    // contend on the pad -- the pin-level analogue of E604.
                    diags.error(
                        format!(
                            "`{}` is driven by two modules; a GPIO pin must be driven by one \
                             (overlaps `{}`). Give each module a disjoint pin range.",
                            claim_i.display(),
                            claim_j.display()
                        ),
                        "E650",
                        *span_i,
                    );
                } else {
                    diags.error(
                        format!(
                            "`{}` is owned by two modules; ownership must be exclusive (it is a \
                             claim about other modules). One module must own it.",
                            claim_i.display()
                        ),
                        "E604",
                        *span_i,
                    );
                }
                break;
            }
        }
    }
}

/// Resolve `PERIPH.REG.FIELD`'s enum variant `variant` to its discriminant, via
/// the field's enum type in the peripheral table. `None` if any part is unknown
/// or the field is not enum-typed.
fn field_enum_value(
    symbols: &SymbolTable,
    periph: &str,
    reg: &str,
    field: &str,
    variant: &str,
) -> Option<u64> {
    let f = symbols.peripherals.get(periph)?.regs.get(reg)?.fields.get(field)?;
    if let crate::types::Type::Enum(_, _, variants) = &f.ty {
        let disc = variants.iter().find(|(n, _)| n == variant)?.1;
        return u64::try_from(disc).ok();
    }
    None
}

/// E651: the DMA->FIFO bridge. A channel declares the DREQ that pairs it with its
/// endpoint (`dreq = P.R.F = VARIANT`); a program that writes a DIFFERENT value to
/// that transfer-request field would pace the channel with the wrong request and
/// over/underrun the FIFO. A channel with no `dreq` is unchecked.
fn check_dreq(program: &Program, symbols: &SymbolTable, target: &Target, diags: &mut DiagnosticBag) {
    let dreqs: Vec<&DreqSpec> = target
        .agents
        .iter()
        .flat_map(|a| a.channels.iter())
        .filter_map(|c| c.dreq.as_ref())
        .collect();
    if dreqs.is_empty() {
        return;
    }

    let mut writes = Vec::new();
    collect_peripheral_writes(program, symbols, &mut writes);

    for d in dreqs {
        let Some(expected) = field_enum_value(symbols, &d.periph, &d.reg, &d.field, &d.variant)
        else {
            // Unresolvable target declaration (bad path / not enum-typed): skip
            // rather than mis-report against a program site.
            continue;
        };
        for w in &writes {
            if w.periph == d.periph
                && w.reg == d.reg
                && w.field.as_deref() == Some(d.field.as_str())
                && let Some(v) = w.rhs_literal
                && v != expected
            {
                diags.error(
                    format!(
                        "`{}.{}.{}` is set to a value that does not match the channel's declared \
                         DREQ `{}`; the DMA would be paced by the wrong request and over/underrun \
                         the FIFO endpoint.",
                        d.periph, d.reg, d.field, d.variant
                    ),
                    "E651",
                    w.span,
                );
            }
        }
    }
}

/// Like `resolve_owns_path` but emits nothing -- used by the exclusivity pass,
/// where unresolved paths were already reported in the main walk.
fn resolve_owns_path_quiet(path: &OwnsPath, symbols: &SymbolTable) -> Option<Claim> {
    let (peripheral, register) = match &path.target {
        crate::ast::OwnsTarget::Gpio { lo, hi } => return Some(Claim::Pins(*lo, *hi)),
        crate::ast::OwnsTarget::Reg {
            peripheral,
            register,
        } => (peripheral, register),
    };
    let periph = symbols.peripherals.get(&peripheral.0)?;
    match register {
        None => Some(Claim::Peripheral(peripheral.0.clone())),
        Some((rname, _)) => periph
            .regs
            .contains_key(rname)
            .then(|| Claim::Register(peripheral.0.clone(), rname.clone())),
    }
}

// ---- slices 2b + 3: handoff write checks ------------------------------------

/// One peripheral register/field write, with the source `FileId` (via `span`).
/// `field` is `None` for a whole-register write (`P.R = x`) and `Some(F)` for a
/// field write (`P.R.F = x`). Produced by one exhaustive walk and consumed by
/// the ownership rule (E605) and the descriptor-reach check (E608). `rhs_static`
/// is the name of the static whose address is delivered (`= &RX_DESC`), used by
/// E608 to find the descriptor handed to an agent. `rhs_disabling` is true when
/// the right-hand side is a provably-disabling literal (`false`/`0`), used by the
/// enable-presence check (E609) so a `= false` write does not count as enabling
/// a clock gate.
struct PeriphWrite {
    periph: String,
    reg: String,
    field: Option<String>,
    span: Span,
    rhs_static: Option<String>,
    rhs_disabling: bool,
    /// The RHS as a compile-time literal (`= 2`, `= true`), when it is one.
    /// The unit cross-check (E618) compares it against the declared value;
    /// a non-literal RHS is "unknown" and neither satisfies nor violates.
    rhs_literal: Option<u64>,
}

/// Handoff write checks, acting at peripheral-register write sites off a single
/// walk:
/// - E605: a write to a handoff register from a file that does not own it.
/// - E608: a descriptor delivered to an agent whose `addr in R` field names a
///   region the agent cannot reach.
/// - E609: an agent programmed without its `enabled_by` clock gates set.
/// - E610: an agent's `enabled_by` clock gate disabled by a non-owning module.
fn check_handoff_writes(
    program: &Program,
    symbols: &SymbolTable,
    target: &Target,
    diags: &mut DiagnosticBag,
) {
    // (peripheral, register) -> agent name, from every agent's handoff list.
    // `handoff_ports` carries the port-select declaration for the handoffs
    // that have one (E612).
    let mut handoff_regs: HashMap<(String, String), String> = HashMap::new();
    let mut handoff_ports: HashMap<(String, String), (String, PortBy)> = HashMap::new();
    // Fixed-block channels (`extent = N`): a buffer delivered to any of the
    // channel's handoff registers must be at least N bytes (E619).
    let mut handoff_fixed: HashMap<(String, String), (String, u64)> = HashMap::new();
    for agent in &target.agents {
        for ch in &agent.channels {
            let fixed = match &ch.extent {
                Some(crate::target::ExtentSpec::Fixed(n)) => Some(*n),
                _ => None,
            };
            for h in &ch.handoffs {
                if let Some((p, r)) = handoff_register_path(&h.register) {
                    if let Some(pb) = &h.port_by {
                        handoff_ports
                            .entry((p.clone(), r.clone()))
                            .or_insert_with(|| (agent.name.clone(), pb.clone()));
                    }
                    if let Some(n) = fixed {
                        handoff_fixed
                            .entry((p.clone(), r.clone()))
                            .or_insert((agent.name.clone(), n));
                    }
                    handoff_regs
                        .entry((p, r))
                        .or_insert_with(|| agent.name.clone());
                }
            }
        }
    }
    if handoff_regs.is_empty() {
        return;
    }

    // Static name -> region name, for resolving where a handed-off address
    // lives (the port-select check keys on the region's mem block).
    let mut static_regions: HashMap<String, String> = HashMap::new();
    for item in &program.items {
        if let Item::StaticDef(s) = item
            && let Some((region_name, _)) = &s.region
        {
            static_regions.insert(s.name.0.clone(), region_name.clone());
        }
    }

    // Which files own each register, via `owns P` (whole peripheral) or
    // `owns P.R` (single register).
    let mut periph_owners: HashMap<String, HashSet<FileId>> = HashMap::new();
    let mut reg_owners: HashMap<(String, String), HashSet<FileId>> = HashMap::new();
    for item in &program.items {
        if let Item::Owns(o) = item {
            for path in &o.paths {
                match resolve_owns_path_quiet(path, symbols) {
                    Some(Claim::Peripheral(p)) => {
                        periph_owners.entry(p).or_default().insert(path.span.file);
                    }
                    Some(Claim::Register(p, r)) => {
                        reg_owners.entry((p, r)).or_default().insert(path.span.file);
                    }
                    // Pin claims don't gate handoff registers (E605); they only
                    // feed the cross-module pin-exclusivity check (E650).
                    Some(Claim::Pins(_, _)) | None => {}
                }
            }
        }
    }

    // Owner files per agent: the files that own the agent's handoff peripheral
    // or registers. The clock-stomp guard (E610) uses this to tell the module
    // managing an agent from a stranger gating its clock.
    let mut agent_owners: HashMap<String, HashSet<FileId>> = HashMap::new();
    for ((p, r), agent_name) in &handoff_regs {
        let entry = agent_owners.entry(agent_name.clone()).or_default();
        if let Some(s) = periph_owners.get(p) {
            entry.extend(s);
        }
        if let Some(s) = reg_owners.get(&(p.clone(), r.clone())) {
            entry.extend(s);
        }
    }

    let mut writes = Vec::new();
    collect_peripheral_writes(program, symbols, &mut writes);

    for w in &writes {
        let key = (w.periph.clone(), w.reg.clone());

        // E605: handoff register written without ownership.
        if let Some(agent_name) = handoff_regs.get(&key) {
            let owned = periph_owners
                .get(&w.periph)
                .is_some_and(|s| s.contains(&w.span.file))
                || reg_owners
                    .get(&key)
                    .is_some_and(|s| s.contains(&w.span.file));
            if !owned {
                diags.error(
                    format!(
                        "`{}.{}` is a handoff register of agent `{agent_name}` and may only be \
                         written by the module that owns it. Add `owns {}.{};` (or \
                         `owns {};`) to this module.",
                        w.periph, w.reg, w.periph, w.reg, w.periph
                    ),
                    "E605",
                    w.span,
                );
            }

            // E608: the write delivers a descriptor base to the agent
            // (`handoff = &RX_DESC`). The agent walks that descriptor and
            // dereferences any `addr in R` field inside it, so it must be able
            // to reach every such region. This is the transitive step past the
            // field-level E607 (field names a real region) and validate_regions
            // (the region's own mem is reachable): the field may point into a
            // *different* region the walking agent cannot reach.
            if let Some(static_name) = &w.rhs_static
                && let Some(agent) = target.agents.iter().find(|a| &a.name == agent_name)
            {
                check_descriptor_reach(static_name, agent, symbols, target, w.span, diags);

                // E619: fixed-block extent. The engine walks exactly N bytes
                // from the delivered address (no count register to arm), so
                // the buffer itself must be at least N bytes.
                if let Some((aname, n)) = handoff_fixed.get(&key)
                    && let Some(sym) = symbols.statics.get(static_name)
                {
                    // Strip storage wrappers (Shared/AgentShared) to size the
                    // value type.
                    let mut ty = &sym.ty;
                    loop {
                        let inner = ty.inner();
                        if std::ptr::eq(inner, ty) {
                            break;
                        }
                        ty = inner;
                    }
                    let size = u64::from(crate::types::element_size(ty));
                    if size > 0 && size < *n {
                        diags.error(
                            format!(
                                "`&{static_name}` ({size} bytes) is delivered to agent `{aname}`, whose channel walks a fixed {n}-byte block -- the engine would run {} bytes past the buffer.",
                                n - size
                            ),
                            "E619",
                            w.span,
                        );
                    }
                }

                // E612: software port select. The handoff declares which field
                // routes its address to which port (window tag); the address's
                // mem block says which port it is actually behind.
                if let Some((_, pb)) = handoff_ports.get(&key) {
                    check_handoff_port(
                        static_name,
                        agent,
                        pb,
                        &static_regions,
                        &writes,
                        symbols,
                        target,
                        w.span,
                        diags,
                    );
                }
            }
        }
    }

    // E609: clock-gate-before-touch. An agent that is programmed (one of its
    // handoff registers written) must have its `enabled_by` clock/enable
    // registers set somewhere, else the handoff writes hit a gated peripheral
    // and are silently dropped. Whole-program presence check (the enable may
    // live in any module); ordering is not yet checked.
    check_agent_enables(target, &handoff_regs, &writes, symbols, diags);

    // E610: clock-stomp guard. A disabling write to an agent's `enabled_by`
    // clock gate, from a module that does not own the agent, silently stops it.
    check_agent_clock_stomp(target, &agent_owners, &writes, symbols, diags);

    // E618: extent unit cross-check. An `extent_by ... xN by P.R.F = V`
    // multiplier is only true physics while the unit-select field holds V.
    check_extent_units(target, &writes, diags);
}

/// E618. For each agent declaring `extent_by = ... by P.R.F = V`: once the
/// program ARMS the agent (writes the extent count field), some write must
/// set the unit field to exactly the literal V (presence, like E609), and a
/// definite write of a DIFFERENT literal is rejected outright -- either way
/// the declared byte multiplier would be a lie. Computed (non-literal)
/// values neither satisfy nor violate; the missing-write message covers them.
fn check_extent_units(target: &Target, writes: &[PeriphWrite], diags: &mut DiagnosticBag) {
    for agent in &target.agents {
        for ch in &agent.channels {
            check_channel_unit(agent, ch, writes, diags);
        }
    }
}

fn check_channel_unit(
    agent: &Agent,
    ch: &Channel,
    writes: &[PeriphWrite],
    diags: &mut DiagnosticBag,
) {
    {
        let Some(crate::target::ExtentSpec::Counter(eb)) = &ch.extent else {
            return;
        };
        let Some((upath, uval)) = &eb.unit else {
            return;
        };
        let eparts: Vec<&str> = eb.path.split('.').collect();
        let uparts: Vec<&str> = upath.split('.').collect();
        let ([ep, er, ef], [up, ur, uf]) = (eparts.as_slice(), uparts.as_slice()) else {
            return; // shapes validated at target load
        };
        // Armed? Use the first extent-field write as the report site.
        let Some(site) = writes
            .iter()
            .find(|w| w.periph == *ep && w.reg == *er && w.field.as_deref() == Some(*ef))
        else {
            return;
        };
        let unit_writes: Vec<&PeriphWrite> = writes
            .iter()
            .filter(|w| w.periph == *up && w.reg == *ur && w.field.as_deref() == Some(*uf))
            .collect();
        for w in &unit_writes {
            if let Some(v) = w.rhs_literal
                && v != *uval
            {
                diags.error(
                    format!(
                        "`{up}.{ur}.{uf}` is set to {v}, but agent `{}` declares its transfer count scaled x{} only when this field is {uval} (`extent ... when`). The armed byte length would be mis-scaled.",
                        agent.name, eb.scale
                    ),
                    "E618",
                    w.span,
                );
            }
        }
        if !unit_writes.iter().any(|w| w.rhs_literal == Some(*uval)) {
            diags.error(
                format!(
                    "agent `{}` is armed here with a count scaled x{}, but nothing sets `{up}.{ur}.{uf} = {uval}` -- the multiplier declared by `extent ... when` is not established. Write the unit field before arming.",
                    agent.name, eb.scale
                ),
                "E618",
                site.span,
            );
        }
    }
}

/// E609 enable-presence check. See the call site in `check_handoff_writes`.
fn check_agent_enables(
    target: &Target,
    handoff_regs: &HashMap<(String, String), String>,
    writes: &[PeriphWrite],
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) {
    for agent in &target.agents {
        if agent.enabled_by.is_empty() {
            continue;
        }
        // Is the agent programmed? Use the first write to one of its handoff
        // registers as the report site ("programmed here but not enabled").
        let Some(site) = writes.iter().find(|w| {
            handoff_regs
                .get(&(w.periph.clone(), w.reg.clone()))
                .is_some_and(|a| a == &agent.name)
        }) else {
            continue;
        };
        for enable_path in &agent.enabled_by {
            let (bare_path, inverted) = split_polarity(enable_path);
            match resolve_enable(bare_path, symbols) {
                None => {
                    diags.error(
                        format!(
                            "agent `{}` has `enabled_by = {enable_path}`, but that does not name \
                             a known peripheral register/field.",
                            agent.name
                        ),
                        "E609",
                        site.span,
                    );
                }
                Some((ep, er, ef)) => {
                    // Normal polarity: the gate must be SET somewhere.
                    // Inverted (`!P.R.F`, clear-to-enable -- e.g. a reset bit
                    // held high at boot): it must be CLEARED somewhere.
                    let enabled = if inverted {
                        writes
                            .iter()
                            .any(|w| disables_field(w, &ep, &er, ef.as_deref()))
                    } else {
                        writes
                            .iter()
                            .any(|w| enable_write_matches(w, &ep, &er, ef.as_deref()))
                    };
                    if !enabled {
                        let action = if inverted { "cleared" } else { "set" };
                        diags.error(
                            format!(
                                "agent `{}` is programmed here (handoff `{}.{}` written) but its \
                                 enable `{enable_path}` is never {action}; writes to a gated \
                                 peripheral are silently dropped. {} it before programming the \
                                 agent.",
                                agent.name,
                                site.periph,
                                site.reg,
                                if inverted { "Clear" } else { "Set" }
                            ),
                            "E609",
                            site.span,
                        );
                    }
                }
            }
        }
    }
}

/// E610 clock-stomp guard. See the call site in `check_handoff_writes`. A
/// disabling write to one of an agent's `enabled_by` clock gates, from a file
/// that does not own the agent, silently stops the agent and is rejected. Only
/// fires when the agent has a declared owner (otherwise there is no baseline for
/// "stranger").
fn check_agent_clock_stomp(
    target: &Target,
    agent_owners: &HashMap<String, HashSet<FileId>>,
    writes: &[PeriphWrite],
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) {
    for agent in &target.agents {
        if agent.enabled_by.is_empty() {
            continue;
        }
        let Some(owners) = agent_owners.get(&agent.name) else {
            continue;
        };
        if owners.is_empty() {
            continue;
        }
        for enable_path in &agent.enabled_by {
            let (bare_path, inverted) = split_polarity(enable_path);
            let Some((ep, er, ef)) = resolve_enable(bare_path, symbols) else {
                continue; // a bad path is already reported by E609
            };
            for w in writes {
                // The stomp direction follows the polarity: a normal gate is
                // stomped by CLEARING it, an inverted (clear-to-enable, e.g.
                // reset bit) gate by SETTING it back. For the inverted case a
                // field write with a non-clearing rhs counts as a possible
                // set -- writing an agent's reset bit from a stranger module
                // is suspect regardless of the computed value.
                let stomps = if inverted {
                    w.periph == ep
                        && w.reg == er
                        && w.field.as_deref() == ef.as_deref()
                        && !w.rhs_disabling
                } else {
                    disables_field(w, &ep, &er, ef.as_deref())
                };
                if stomps && !owners.contains(&w.span.file) {
                    let dir = if inverted {
                        "re-asserting it"
                    } else {
                        "disabling it"
                    };
                    diags.error(
                        format!(
                            "`{enable_path}` gates agent `{}`; {dir} from a module that does \
                             not own the agent can silently stop it. Only the module that owns \
                             the agent may operate its gate.",
                            agent.name
                        ),
                        "E610",
                        w.span,
                    );
                }
            }
        }
    }
}

/// E612 port-select check. A handoff with `port_by F TAG` hands its address to
/// an agent whose master port is chosen by software: F set routes through the
/// windows tagged TAG, F clear through the rest. Where the address actually
/// lives (its region's mem block, against the agent's windows) dictates the
/// required state of F:
/// - block behind TAG-only windows -> F must be set somewhere (presence check,
///   like E609; ordering is not checked). On the default port the address is
///   unmapped and the transfer errors at runtime -- the MDMA/DTCM TED.
/// - block behind no TAG window -> a definite set of F is rejected (it would
///   misroute the access).
/// - block covered by both (ambiguous) or by no window, or an address that is
///   not a literal `&STATIC` in a region: skipped, conservative.
#[allow(clippy::too_many_arguments)]
fn check_handoff_port(
    static_name: &str,
    agent: &Agent,
    pb: &PortBy,
    static_regions: &HashMap<String, String>,
    writes: &[PeriphWrite],
    symbols: &SymbolTable,
    target: &Target,
    span: Span,
    diags: &mut DiagnosticBag,
) {
    let Some(region_name) = static_regions.get(static_name) else {
        return;
    };
    let Some(region) = target.regions.iter().find(|r| &r.name == region_name) else {
        return;
    };
    let Some(mem) = target.mem_blocks.iter().find(|m| m.name == region.mem) else {
        return;
    };
    let covering: Vec<_> = agent
        .bus
        .iter()
        .filter(|w| w.covers(mem.base, mem.end()))
        .collect();
    if covering.is_empty() {
        return; // unreachable placement is the reach/bus validation's report
    }
    let is_tag = |w: &&crate::target::BusWindow| w.port.as_deref() == Some(pb.tag.as_str());
    let on_tag = covering.iter().all(is_tag);
    let off_tag = !covering.iter().any(is_tag);
    let Some((ep, er, ef)) = resolve_enable(&pb.field, symbols) else {
        diags.error(
            format!(
                "agent `{}` handoff has `port_by {} {}`, but that does not name a known \
                 peripheral register/field.",
                agent.name, pb.field, pb.tag
            ),
            "E612",
            span,
        );
        return;
    };
    if on_tag {
        let set = writes
            .iter()
            .any(|w| enable_write_matches(w, &ep, &er, ef.as_deref()));
        if !set {
            diags.error(
                format!(
                    "`{static_name}` (region `{region_name}`, mem `{}`) is behind agent `{}`'s \
                     `{}` port, and `{}` -- which selects that port -- is never set. On the \
                     default port this address is unmapped and the transfer errors at runtime. \
                     Set `{} = true` before enabling the agent.",
                    mem.name, agent.name, pb.tag, pb.field, pb.field
                ),
                "E612",
                span,
            );
        }
    } else if off_tag {
        for w in writes {
            if w.periph == ep
                && w.reg == er
                && w.field.as_deref() == ef.as_deref()
                && !w.rhs_disabling
            {
                diags.error(
                    format!(
                        "`{}` routes agent `{}`'s handoff through its `{}` port, but \
                         `{static_name}` (mem `{}`) is not behind that port -- the access would \
                         be misrouted. Clear `{}` or hand off an address behind the `{}` port.",
                        pb.field, agent.name, pb.tag, mem.name, pb.field, pb.tag
                    ),
                    "E612",
                    w.span,
                );
            }
        }
    }
    // Covered by a mix of TAG and non-TAG windows: either port works, nothing
    // to require.
}

/// Split an optional leading `!` (inverted polarity) off an `enabled_by` /
/// `completes_by` path: `!RESETS.RESET.DMA` -> `(RESETS.RESET.DMA, true)`.
/// Inverted enable = the gate is CLEAR-to-enable (e.g. a reset bit held high
/// at boot); the E609/E610 directions flip accordingly.
fn split_polarity(path: &str) -> (&str, bool) {
    path.strip_prefix('!')
        .map_or((path, false), |rest| (rest, true))
}

/// Resolve an `enabled_by` path (`P.R` or `P.R.F`) against the peripheral table.
/// Returns `(peripheral, register, field?)` when it resolves, `None` otherwise.
fn resolve_enable(path: &str, symbols: &SymbolTable) -> Option<(String, String, Option<String>)> {
    let mut parts = path.split('.');
    let p = parts.next()?;
    let r = parts.next()?;
    let f = parts.next().map(str::to_string);
    if parts.next().is_some() {
        return None; // more than three segments: not a register/field path
    }
    let periph = symbols.peripherals.get(p)?;
    let reg = periph.regs.get(r)?;
    if let Some(fname) = &f
        && !reg.fields.contains_key(fname)
    {
        return None;
    }
    Some((p.to_string(), r.to_string(), f))
}

/// Whether write `w` sets the enable register/field `(ep, er, ef)`. A
/// provably-disabling write (`= false`/`0`) does not count. A whole-register
/// write to the enable's register is counted as possibly setting the field
/// (we do not evaluate the mask), keeping the presence check free of false
/// positives.
fn enable_write_matches(w: &PeriphWrite, ep: &str, er: &str, ef: Option<&str>) -> bool {
    if w.periph != ep || w.reg != er || w.rhs_disabling {
        return false;
    }
    match ef {
        Some(f) => w.field.is_none() || w.field.as_deref() == Some(f),
        None => true,
    }
}

/// Whether write `w` is a disabling write (`= false`/`0`) that clears the enable
/// register/field `(ep, er, ef)` -- either a direct disable of the field or a
/// whole-register clear that takes it down with everything else. The mirror of
/// `enable_write_matches`, used by the clock-stomp guard (E610).
fn disables_field(w: &PeriphWrite, ep: &str, er: &str, ef: Option<&str>) -> bool {
    if w.periph != ep || w.reg != er || !w.rhs_disabling {
        return false;
    }
    match ef {
        Some(f) => w.field.is_none() || w.field.as_deref() == Some(f),
        None => w.field.is_none(),
    }
}

// ---- B v0: sound-reclaim guard (E611) ---------------------------------------

/// Sound-reclaim guard. A `reclaim(BUF)` of a buffer an agent writes must be
/// control-dependent on observing that agent's `completes_by` flag (the
/// transfer-complete signal), else the CPU may read the buffer mid-transfer.
///
/// Sound, conservative, proven by span containment (no flow-sensitive walk).
/// Accepted acquire forms, each establishing the flag over a span the reclaim
/// must lie in:
/// - `if <flag> { reclaim }` (try-acquire): the then-block span. `<flag>` may
///   be the field read or a completion predicate (`if mdma_done()`).
/// - `while !<flag> {} ... reclaim` (blocking acquire): the rest of the
///   enclosing block after the loop. The body must be EMPTY (the canonical
///   busy-wait) -- a non-empty body could hide a `break` that exits with the
///   flag still clear, so it stays conservatively rejected.
/// - `if !<flag> { return; } reclaim` (early-exit acquire): the rest of the
///   enclosing block, when the then-block always terminates directly
///   (`has_direct_terminator`) and there is no else branch.
///
/// "Rest of the block" is sound at the same level as the then-block form:
/// code inside the span could clear the flag after it was observed; tracking
/// that is the full flow-sensitive B. Still rejected: compared conditions
/// (`flag == true`), waits with non-empty bodies, per-buffer association
/// (one async agent per region). Opt-in: only agents that declare
/// `completes_by` are guarded; without it `reclaim` stays trusted.
///
/// Scoped view lifetimes (E616). The view a guarded `reclaim` yields is only
/// trustworthy while its justification holds, so two temporal escapes are
/// rejected on top of the guard itself:
/// - a binding holding the reclaimed view (`const v = reclaim(BUF)`) mentioned
///   OUTSIDE every guard span that contains the reclaim -- the view outlived
///   its window (e.g. assigned to an outer variable in a try-acquire then used
///   after the `if`);
/// - a RELEASE between the justification and a use: a write to a handoff
///   register of an agent that declares `completes_by` hands the buffer back,
///   so the previously observed completion no longer covers it. A release
///   between guard and reclaim re-opens E611; one between reclaim and a later
///   mention of the binding is E616. Conservative: ANY handoff write of an
///   agent with matching flags counts, even if it delivers a different buffer
///   (per-buffer association is the same recorded follow-up as above).
///
/// Lexical, per function: bindings and mentions are matched by name within one
/// function, so a view carried across a loop back-edge (mention textually
/// before the reclaim) is not seen -- recorded blind spot, with addresses cast
/// to integers (the verify/provenance domain).
fn check_reclaim_guards(
    program: &Program,
    symbols: &SymbolTable,
    target: &Target,
    diags: &mut DiagnosticBag,
) {
    // static name -> the completion flags ("P.R.F") of the DMA/external agents
    // that write its region.
    let mut flags_of: HashMap<String, Vec<String>> = HashMap::new();
    for item in &program.items {
        if let Item::StaticDef(s) = item
            && let Some((rname, _)) = &s.region
            && let Some(region) = target.regions.iter().find(|r| &r.name == rname)
        {
            let mut flags = Vec::new();
            for aname in &region.agents {
                if let Some(agent) = target.agents.iter().find(|a| &a.name == aname)
                    && matches!(agent.kind, AgentKind::Dma | AgentKind::External)
                {
                    flags.extend(agent.completes_by().cloned());
                }
            }
            if !flags.is_empty() {
                flags_of.insert(s.name.0.clone(), flags);
            }
        }
    }
    if flags_of.is_empty() {
        return; // no agent declares a completion signal -> reclaim stays trusted
    }

    // Per-channel handoff maps. `chan_flags` carries EVERY channel's flag
    // set (possibly empty) for the buffer association below; `handoff_flags`
    // keeps only flagged channels for the release-truncation check.
    let mut chan_flags: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut handoff_flags: HashMap<(String, String), Vec<String>> = HashMap::new();
    for agent in &target.agents {
        for ch in &agent.channels {
            for h in &ch.handoffs {
                if let Some((p, r)) = handoff_register_path(&h.register) {
                    chan_flags
                        .entry((p.clone(), r.clone()))
                        .or_default()
                        .extend(ch.completes_by.iter().cloned());
                    if !ch.completes_by.is_empty() {
                        // Per channel: a release through ch's handoff
                        // registers only invalidates justifications resting
                        // on ch's own flags.
                        handoff_flags
                            .entry((p, r))
                            .or_default()
                            .extend(ch.completes_by.iter().cloned());
                    }
                }
            }
        }
    }
    let mut writes = Vec::new();
    collect_peripheral_writes(program, symbols, &mut writes);
    let mut releases: Vec<(Span, Vec<String>)> = Vec::new();
    for w in &writes {
        if let Some(flags) = handoff_flags.get(&(w.periph.clone(), w.reg.clone())) {
            releases.push((w.span, flags.clone()));
        }
    }

    // Clearing writes for the staleness rule: any write touching a
    // completion flag's OWN register counts as clearing it (covers W1C
    // `INTR = 1`, event-register `= 0`, and RMW `= false` styles; a chip
    // whose clear lives in a separate register -- the H7's IFCR -- will
    // need declared vocabulary when a double-observation idiom appears).
    let mut clears: Vec<(Span, String)> = Vec::new();
    {
        let mut flag_regs: Vec<(String, String, String)> = Vec::new(); // (P, R, flag)
        for agent in &target.agents {
            for f in agent.completes_by() {
                let bare = f.strip_prefix('!').unwrap_or(f);
                let parts: Vec<&str> = bare.split('.').collect();
                if let [p, r, _] = parts.as_slice() {
                    flag_regs.push(((*p).to_string(), (*r).to_string(), f.clone()));
                }
            }
        }
        for w in &writes {
            for (p, r, f) in &flag_regs {
                if &w.periph == p && &w.reg == r {
                    clears.push((w.span, f.clone()));
                }
            }
        }
    }

    // Per-buffer flag association: a direct delivery (`P.R = &BUF`) binds
    // the buffer to that register's CHANNEL, so its reclaim must be guarded
    // by that channel's own completion flags -- not any flag of any agent
    // on the region (the old union let ch1's flag justify reclaiming ch0's
    // buffer). Narrowing is sound (stricter); a buffer with no visible
    // direct delivery keeps the region union (indirect deliveries through
    // helpers stay conservative); a buffer delivered only to flag-less
    // channels has no sound guard to demand and drops back to trusted.
    let mut delivered: HashMap<String, Vec<String>> = HashMap::new();
    for w in &writes {
        if let Some(st) = &w.rhs_static
            && flags_of.contains_key(st)
            && let Some(flags) = chan_flags.get(&(w.periph.clone(), w.reg.clone()))
        {
            delivered
                .entry(st.clone())
                .or_default()
                .extend(flags.iter().cloned());
        }
    }
    for (st, mut flags) in delivered {
        flags.sort();
        flags.dedup();
        if flags.is_empty() {
            flags_of.remove(&st);
        } else {
            flags_of.insert(st, flags);
        }
    }

    // Completion predicates: a fn whose result is a *direct* flag read (empty
    // `preds` -> no predicate-through-predicate) maps to that flag, so
    // `if mdma_done() { reclaim }` counts as a guard -- same soundness as the
    // inline read, since the predicate returns the flag's current value.
    let mut preds = HashMap::new();
    let no_preds = HashMap::new();
    for item in &program.items {
        if let Item::FnDef(f) = item
            && let Some(result) = fn_result_expr(&f.body)
            && let Some(flag) = cond_flag(result, &no_preds)
        {
            preds.insert(f.name.0.clone(), flag);
        }
    }

    // Per function: guard/reclaim containment is span-local anyway, and the
    // binding/mention matching is by NAME, so scanning per fn keeps an
    // unrelated local that happens to share a binding's name in another fn
    // from being checked against it.
    for item in &program.items {
        if let Item::FnDef(f) = item {
            let mut scan = GuardScan {
                preds: preds.clone(),
                ..GuardScan::default()
            };
            gscan_block(&f.body, &flags_of, &mut scan);
            check_fn_reclaims(&scan, &releases, &clears, diags);
        }
    }
}

/// Whether a release of any flag in `flags` lies strictly between file
/// positions `lo` and `hi` (same file) -- the buffer was handed back to the
/// agent inside that interval, so a completion observed at `lo` no longer
/// covers a use at `hi`.
fn released_between(
    releases: &[(Span, Vec<String>)],
    flags: &[String],
    at: Span,
    lo: usize,
    hi: usize,
) -> bool {
    releases.iter().any(|(rspan, rflags)| {
        rspan.file == at.file
            && lo < rspan.start
            && rspan.start < hi
            && rflags.iter().any(|f| flags.contains(f))
    })
}

/// The E611 + E616 checks over one function's scan (see
/// `check_reclaim_guards` for the rules).
fn check_fn_reclaims(
    scan: &GuardScan,
    releases: &[(Span, Vec<String>)],
    clears: &[(Span, String)],
    diags: &mut DiagnosticBag,
) {
    // E611: every reclaim inside a guard span of one of its flags, with no
    // release between the guard's start and the reclaim. `justified` keeps
    // each guarded reclaim's clean (flag, guard) pairs for the staleness
    // pass below.
    let mut guarded_reclaims: Vec<Span> = Vec::new();
    let mut justified: Vec<(Span, Vec<(String, Span)>)> = Vec::new();
    for (rspan, flags) in &scan.reclaims {
        let contained: Vec<&(String, Span)> = scan
            .guards
            .iter()
            .filter(|(gflag, gspan)| {
                flags.contains(gflag)
                    && gspan.file == rspan.file
                    && gspan.start <= rspan.start
                    && rspan.end <= gspan.end
            })
            .collect();
        let clean: Vec<(String, Span)> = contained
            .iter()
            .filter(|(_, gspan)| {
                !released_between(releases, flags, *rspan, gspan.start, rspan.start)
            })
            .map(|(f, g)| (f.clone(), *g))
            .collect();
        if !clean.is_empty() {
            guarded_reclaims.push(*rspan);
            justified.push((*rspan, clean));
        } else if contained.is_empty() {
            diags.error(
                format!(
                    "`reclaim` here is not guarded by a completion check: the agent may still be \
                     writing the buffer. Guard it with `if <flag> {{ ... }}`, a `while !<flag> \
                     {{}}` busy-wait, or `if !<flag> {{ return; }}` before it, testing one of: {}.",
                    flags.join(", ")
                ),
                "E611",
                *rspan,
            );
        } else {
            diags.error(
                "`reclaim` here is guarded, but the buffer was released back to the agent \
                 (handoff register written) after the completion check -- the observed \
                 completion covers the PREVIOUS transfer. Re-check the flag after re-arming."
                    .to_string(),
                "E611",
                *rspan,
            );
        }
    }

    // E611 staleness (the W1C gap): a guard observing a flag that an EARLIER
    // guarded reclaim in this function already consumed, with a release
    // (re-arm) in between, sees the PREVIOUS transfer's completion unless
    // the flag was cleared (some write to the flag's own register) between
    // the two observations. First observations are trusted (boot-clear
    // state); `!`-polarity flags are hardware-managed and exempt; the
    // loop-back-edge variant of this bug is a recorded blind spot (lexical
    // engine). Justifications are per (flag, guard) pair: a reclaim with at
    // least one FRESH justification is fine.
    for (i, (rspan, pairs)) in justified.iter().enumerate() {
        let all_stale = !pairs.is_empty()
            && pairs.iter().all(|(f, gspan)| {
                if f.starts_with('!') {
                    return false; // auto-clearing busy flags cannot go stale
                }
                justified.iter().take(i).any(|(r1, pairs1)| {
                    r1.start < rspan.start
                        && pairs1.iter().any(|(f1, _)| f1 == f)
                        && releases.iter().any(|(rel, rflags)| {
                            rel.file == rspan.file
                                && r1.start < rel.start
                                && rel.start < gspan.start
                                && rflags.contains(f)
                        })
                        && !clears.iter().any(|(cspan, cf)| {
                            cf == f
                                && cspan.file == rspan.file
                                && r1.start < cspan.start
                                && cspan.start < gspan.start
                        })
                })
            });
        if all_stale {
            diags.error(
                "`reclaim` here re-observes a completion flag that an earlier reclaim already                  consumed, with the buffer re-armed in between and no clearing write to the                  flag's register -- the flag is still set from the PREVIOUS transfer. Clear it                  before (or right after) re-arming."
                    .to_string(),
                "E611",
                *rspan,
            );
        }
    }

    // E616: a mention of a binding whose MOST RECENT whole-name binding event
    // (before the mention, in source order) was a guarded reclaim must still
    // sit inside a guard span containing that reclaim, with no release in
    // between. A kill (rebind to something else) in between clears the
    // obligation; unguarded reclaims already got E611, so their mentions are
    // skipped to avoid cascading.
    let reclaim_names: HashSet<&str> = scan
        .bind_events
        .iter()
        .filter(|(_, _, ev)| matches!(ev, BindEvent::Reclaim(..)))
        .map(|(n, _, _)| n.as_str())
        .collect();
    for (mname, mspan) in &scan.mentions {
        if !reclaim_names.contains(mname.as_str()) {
            continue;
        }
        let last = scan
            .bind_events
            .iter()
            .filter(|(n, pos, _)| n == mname && *pos < mspan.start)
            .max_by_key(|(_, pos, _)| *pos);
        let Some((_, _, BindEvent::Reclaim(rspan, flags))) = last else {
            continue;
        };
        if mspan.start < rspan.end || !guarded_reclaims.contains(rspan) {
            continue;
        }
        let covering = scan.guards.iter().any(|(gflag, gspan)| {
            flags.contains(gflag)
                && gspan.file == rspan.file
                && gspan.start <= rspan.start
                && rspan.end <= gspan.end
                && gspan.start <= mspan.start
                && mspan.end <= gspan.end
        });
        if !covering {
            diags.error(
                format!(
                    "view `{mname}` from `reclaim` is used outside the completion-guarded \
                     window that justified it: past the guard the agent may be writing the \
                     buffer again. Keep every use of the view inside the guard."
                ),
                "E616",
                *mspan,
            );
        } else if released_between(releases, flags, *mspan, rspan.start, mspan.start) {
            diags.error(
                format!(
                    "view `{mname}` is used after the buffer was released back to the agent \
                     (handoff register written): the completion that justified the `reclaim` \
                     covers the PREVIOUS transfer, not this use."
                ),
                "E616",
                *mspan,
            );
        }
    }
}

#[derive(Default)]
struct GuardScan {
    /// `(completion-flag path, span of the then-block it guards)`.
    guards: Vec<(String, Span)>,
    /// `(reclaim span, the buffer's acceptable completion flags)`.
    reclaims: Vec<(Span, Vec<String>)>,
    /// Completion predicates: a fn name -> the flag it returns, so an
    /// `if mdma_done()` guard resolves to the underlying flag.
    preds: HashMap<String, String>,
    /// Whole-name binding events in source order: `const v = ...` and
    /// `v = ...` either bind a reclaimed view (its later mentions must stay
    /// inside the justifying guard span, E616) or KILL the association (the
    /// name now holds something else). A mention is judged against the most
    /// recent event before it, so re-using a name across windows or rebinding
    /// it to a harmless view does not trip the check.
    bind_events: Vec<(String, usize, BindEvent)>,
    /// Every identifier occurrence `(name, span)` -- reads, index bases, and
    /// lvalue bases, but NOT a whole-variable assignment target (that is a
    /// kill/rebind, not a use of the old view). Filtered by binding names.
    mentions: Vec<(String, Span)>,
}

enum BindEvent {
    /// The name was bound to `reclaim(BUF)`: `(reclaim span, BUF's flags)`.
    Reclaim(Span, Vec<String>),
    /// The name was bound to something else; earlier reclaim facts about it
    /// no longer apply.
    Kill,
}

impl GuardScan {
    /// Record a whole-name binding (`const name = value` / `name = value`)
    /// as a reclaim binding or a kill, positioned at the value's start.
    fn bind_event(&mut self, name: &str, value: &Expr, flags_of: &HashMap<String, Vec<String>>) {
        let ev = match reclaim_init(value, flags_of) {
            Some((rspan, flags)) => BindEvent::Reclaim(rspan, flags),
            None => BindEvent::Kill,
        };
        self.bind_events
            .push((name.to_string(), value.span().start, ev));
    }
}

/// The single expression a function evaluates to -- its trailing expression, or
/// the value of a lone `return e`. Used to recognize completion predicates.
fn fn_result_expr(body: &Block) -> Option<&Expr> {
    if let Some(t) = &body.trailing {
        Some(t)
    } else if let [Stmt::Return(r)] = body.stmts.as_slice() {
        r.value.as_ref()
    } else {
        None
    }
}

/// The flag an `if` condition establishes in its then-branch: a bare field read
/// (`P.R.F`, possibly parenthesized), or a no-argument call to a completion
/// predicate (`mdma_done()` whose body returns the flag). `preds` resolves the
/// latter; pass an empty map to recognize only direct reads (used to *build* the
/// predicate set without recursing through predicates).
/// Flip the polarity of a flag string: `"P.R.F"` <-> `"!P.R.F"`. Polarity
/// rides on the string -- a `completes_by = !P.R.F` declaration means "done
/// when the field is CLEAR" (e.g. the RP2350 DMA BUSY bit), and guard forms
/// establish either the positive or the negated fact.
/// The integer value of a comparison literal (`1`, `true`, `(0)`), or `None`
/// for anything else.
fn cmp_literal(e: &Expr) -> Option<u64> {
    match e {
        Expr::Group(inner) => cmp_literal(inner),
        Expr::IntLiteral(v, _, _) => Some(*v),
        Expr::BoolLiteral(b, _) => Some(u64::from(*b)),
        _ => None,
    }
}

fn negate_flag(f: &str) -> String {
    f.strip_prefix('!')
        .map_or_else(|| format!("!{f}"), str::to_string)
}

fn cond_flag(e: &Expr, preds: &HashMap<String, String>) -> Option<String> {
    match e {
        Expr::Group(inner) => cond_flag(inner, preds),
        // `!<flag>` tests the flag clear: the condition's truth is the
        // NEGATED fact. Double negation normalizes (`!!F` -> `F`).
        Expr::Unary(UnaryOp::Not, inner) => cond_flag(inner, preds).map(|f| negate_flag(&f)),
        // Compared forms (the status-word idiom): `flag == 1`/`== true` and
        // `flag != 0`/`!= false` establish the flag; `flag == 0`/`== false`
        // establishes its negation. `!= <nonzero>` is NOT accepted: for a
        // multi-bit field it does not imply clear. Either operand order.
        Expr::Binary(l, op @ (crate::ast::BinaryOp::Eq | crate::ast::BinaryOp::NotEq), r) => {
            let (flag_side, lit_side) = match cmp_literal(l) {
                Some(_) => (r.as_ref(), l.as_ref()),
                None => (l.as_ref(), r.as_ref()),
            };
            let lit = cmp_literal(lit_side)?;
            let flag = cond_flag(flag_side, preds)?;
            match (op, lit != 0) {
                (crate::ast::BinaryOp::Eq, true) | (crate::ast::BinaryOp::NotEq, false) => {
                    Some(flag)
                }
                (crate::ast::BinaryOp::Eq, false) => Some(negate_flag(&flag)),
                _ => None, // `!= nonzero` proves nothing about a wide field
            }
        }
        Expr::FieldAccess(mid, (field, _)) => {
            if let Expr::FieldAccess(inner, (reg, _)) = mid.as_ref()
                && let Expr::Ident((periph, _)) = inner.as_ref()
            {
                Some(format!("{periph}.{reg}.{field}"))
            } else {
                None
            }
        }
        Expr::Call(callee, args) if args.is_empty() => {
            if let Expr::Ident((name, _)) = callee.as_ref() {
                preds.get(name).cloned()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Exhaustive walk collecting `if <flag>` guards and `reclaim` sites. Mirrors
/// `walk_stmt`/`walk_expr` arm-for-arm so a new AST node breaks both at once.
/// The block walk additionally probes each statement for the blocking-acquire
/// forms, which guard the REST of this block (see `check_reclaim_guards`).
fn gscan_block(block: &Block, flags_of: &HashMap<String, Vec<String>>, scan: &mut GuardScan) {
    for stmt in &block.stmts {
        // `while <cond> {}` (empty body): the loop exits only when the
        // condition turned false, so the NEGATION of the condition's fact
        // holds from after the loop to the end of the block. Covers both
        // polarities: `while !DONE {}` establishes DONE, `while BUSY {}`
        // establishes !BUSY (the wait-while-set idiom of busy-high flags).
        if let Stmt::While(w) = stmt
            && let Some(flag) = cond_flag(&w.cond, &scan.preds)
            && w.body.stmts.is_empty()
            && w.body.trailing.is_none()
        {
            scan.guards
                .push((negate_flag(&flag), rest_of_block(block, w.body.span)));
        }
        // `if <cond> { return; }` (early exit): past the if, the condition
        // was observed false -- its negated fact holds for the rest of the
        // block. Requires no else and a then-block that always terminates
        // directly. Both polarities: `if !DONE { return; }` establishes
        // DONE, `if BUSY { return; }` establishes !BUSY.
        if let Stmt::If(i) = stmt
            && let Some(flag) = cond_flag(&i.cond, &scan.preds)
            && i.else_branch.is_none()
            && i.then_block.has_direct_terminator()
        {
            scan.guards
                .push((negate_flag(&flag), rest_of_block(block, i.then_block.span)));
        }
        gscan_stmt(stmt, flags_of, scan);
    }
    if let Some(t) = &block.trailing {
        gscan_expr(t, flags_of, scan);
    }
}

/// The span from just after `upto` to the end of `block` -- the region a
/// blocking-acquire form establishes its flag over.
fn rest_of_block(block: &Block, upto: Span) -> Span {
    Span::new(block.span.file, upto.end, block.span.end)
}

fn gscan_stmt(stmt: &Stmt, flags_of: &HashMap<String, Vec<String>>, scan: &mut GuardScan) {
    match stmt {
        Stmt::If(i) => {
            gscan_expr(&i.cond, flags_of, scan);
            if let Some(flag) = cond_flag(&i.cond, &scan.preds) {
                scan.guards.push((flag, i.then_block.span));
            }
            gscan_block(&i.then_block, flags_of, scan);
            if let Some(eb) = &i.else_branch {
                gscan_stmt(eb, flags_of, scan);
            }
        }
        Stmt::VarDecl(vd) => {
            scan.bind_event(&vd.name.0, &vd.init, flags_of);
            gscan_expr(&vd.init, flags_of, scan);
        }
        Stmt::Assign(a) => {
            // A whole-variable target is a kill/rebind of the name, not a use
            // of the view it held -- skip the mention but record the binding
            // event. Any other lvalue shape (index/field/deref) reads its
            // base, so it counts as a mention.
            if let LValue::Name(n) = &a.target {
                scan.bind_event(&n.0, &a.value, flags_of);
            } else {
                gscan_lvalue(&a.target, flags_of, scan);
            }
            gscan_expr(&a.value, flags_of, scan);
        }
        Stmt::CompoundAssign(ca) => {
            // `x OP= v` reads the target, so even a whole-name target is a
            // mention (unlike a plain assignment).
            gscan_lvalue(&ca.target, flags_of, scan);
            gscan_expr(&ca.value, flags_of, scan);
        }
        Stmt::Expr(e) => gscan_expr(e, flags_of, scan),
        Stmt::Loop(l) => gscan_block(&l.body, flags_of, scan),
        Stmt::Claim(c) => gscan_block(&c.body, flags_of, scan),
        Stmt::While(w) => {
            gscan_expr(&w.cond, flags_of, scan);
            gscan_block(&w.body, flags_of, scan);
        }
        Stmt::For(f) => {
            gscan_expr(&f.start, flags_of, scan);
            gscan_expr(&f.end, flags_of, scan);
            if let Some(step) = &f.step {
                gscan_expr(step, flags_of, scan);
            }
            gscan_block(&f.body, flags_of, scan);
        }
        Stmt::Match(m) => {
            gscan_expr(&m.scrutinee, flags_of, scan);
            for arm in &m.arms {
                gscan_block(&arm.body, flags_of, scan);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                gscan_expr(v, flags_of, scan);
            }
        }
        Stmt::Asm(a) => {
            for (_, target) in &a.outputs {
                gscan_expr(target, flags_of, scan);
            }
            for (_, value) in &a.inputs {
                gscan_expr(value, flags_of, scan);
            }
        }
        Stmt::Assume(a) => gscan_expr(&a.cond, flags_of, scan),
        Stmt::Assert(a) => gscan_expr(&a.cond, flags_of, scan),
        Stmt::Block(b) => gscan_block(b, flags_of, scan),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

/// The `reclaim(BUF)` initializer of a binding, through grouping parens:
/// `(reclaim span, BUF's completion flags)` when `BUF` is a tracked static.
fn reclaim_init(e: &Expr, flags_of: &HashMap<String, Vec<String>>) -> Option<(Span, Vec<String>)> {
    match e {
        Expr::Group(inner) => reclaim_init(inner, flags_of),
        Expr::ViewNew {
            base,
            reclaim: true,
            span,
            ..
        } => {
            if let Expr::Ident((name, _)) = base.as_ref() {
                flags_of.get(name).map(|flags| (*span, flags.clone()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Mention collection over assignment targets: the base name of an
/// index/field/deref write is a USE of the binding (`v[0] = 1` dereferences
/// the view `v`), and embedded index expressions are walked like any other
/// expression (a `reclaim` in an index position is still a reclaim site).
fn gscan_lvalue(lv: &LValue, flags_of: &HashMap<String, Vec<String>>, scan: &mut GuardScan) {
    match lv {
        LValue::Name(id) => scan.mentions.push((id.0.clone(), id.1)),
        LValue::Field(base, _) => gscan_lvalue(base, flags_of, scan),
        LValue::Index(base, idx) => {
            gscan_lvalue(base, flags_of, scan);
            gscan_expr(idx, flags_of, scan);
        }
        LValue::Deref(e) => gscan_expr(e, flags_of, scan),
    }
}

fn gscan_expr(expr: &Expr, flags_of: &HashMap<String, Vec<String>>, scan: &mut GuardScan) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Ident((id, id_span)) => scan.mentions.push((id.clone(), *id_span)),
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            gscan_expr(e, flags_of, scan);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            gscan_expr(l, flags_of, scan);
            gscan_expr(r, flags_of, scan);
        }
        Expr::Call(callee, args) => {
            gscan_expr(callee, flags_of, scan);
            for a in args {
                gscan_expr(a, flags_of, scan);
            }
        }
        Expr::ViewNew {
            base,
            len,
            stride,
            reclaim,
            span,
        } => {
            if *reclaim
                && let Expr::Ident((name, _)) = base.as_ref()
                && let Some(flags) = flags_of.get(name)
            {
                scan.reclaims.push((*span, flags.clone()));
            }
            gscan_expr(base, flags_of, scan);
            if let Some(l) = len {
                gscan_expr(l, flags_of, scan);
            }
            if let Some(s) = stride {
                gscan_expr(s, flags_of, scan);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            gscan_expr(base, flags_of, scan);
            if let Some(c) = capacity {
                gscan_expr(c, flags_of, scan);
            }
            gscan_expr(head, flags_of, scan);
            gscan_expr(len, flags_of, scan);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            gscan_expr(base, flags_of, scan);
            if let Some(o) = bit_offset {
                gscan_expr(o, flags_of, scan);
            }
            if let Some(l) = len_bits {
                gscan_expr(l, flags_of, scan);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                gscan_expr(e, flags_of, scan);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                gscan_expr(e, flags_of, scan);
            }
        }
        Expr::Match(m) => {
            gscan_expr(&m.scrutinee, flags_of, scan);
            for arm in &m.arms {
                gscan_block(&arm.body, flags_of, scan);
            }
        }
        Expr::Block(b) => gscan_block(&b.block, flags_of, scan),
        Expr::If(i) => {
            gscan_expr(&i.cond, flags_of, scan);
            if let Some(flag) = cond_flag(&i.cond, &scan.preds) {
                scan.guards.push((flag, i.then_block.span));
            }
            gscan_block(&i.then_block, flags_of, scan);
            gscan_expr(&i.else_branch, flags_of, scan);
        }
    }
}

/// Collect every `claim X {}` window (target name + body span) and every
/// span-carrying mention of a `tracked` static, for the cross-core
/// require-claim check. Exhaustive walk (no catch-all), mirroring the other
/// Stmt/Expr walkers; the claim's own header name is recorded as a window,
/// not a mention.
#[allow(clippy::type_complexity)]
fn claims_and_mentions(
    program: &Program,
    tracked: &HashSet<&str>,
) -> (Vec<(String, Span)>, Vec<(String, Span)>) {
    let mut windows = Vec::new();
    let mut mentions = Vec::new();
    for item in &program.items {
        if let Item::FnDef(f) = item {
            cm_block(&f.body, tracked, &mut windows, &mut mentions);
        }
    }
    (windows, mentions)
}

fn cm_block(
    block: &Block,
    tracked: &HashSet<&str>,
    windows: &mut Vec<(String, Span)>,
    mentions: &mut Vec<(String, Span)>,
) {
    for stmt in &block.stmts {
        cm_stmt(stmt, tracked, windows, mentions);
    }
    if let Some(t) = &block.trailing {
        cm_expr(t, tracked, windows, mentions);
    }
}

fn cm_stmt(
    stmt: &Stmt,
    tracked: &HashSet<&str>,
    windows: &mut Vec<(String, Span)>,
    mentions: &mut Vec<(String, Span)>,
) {
    match stmt {
        Stmt::VarDecl(vd) => cm_expr(&vd.init, tracked, windows, mentions),
        Stmt::Assign(a) => {
            cm_lvalue(&a.target, tracked, windows, mentions);
            cm_expr(&a.value, tracked, windows, mentions);
        }
        Stmt::CompoundAssign(ca) => {
            cm_lvalue(&ca.target, tracked, windows, mentions);
            cm_expr(&ca.value, tracked, windows, mentions);
        }
        Stmt::Expr(e) => cm_expr(e, tracked, windows, mentions),
        Stmt::If(i) => {
            cm_expr(&i.cond, tracked, windows, mentions);
            cm_block(&i.then_block, tracked, windows, mentions);
            if let Some(eb) = &i.else_branch {
                cm_stmt(eb, tracked, windows, mentions);
            }
        }
        Stmt::Loop(l) => cm_block(&l.body, tracked, windows, mentions),
        Stmt::While(w) => {
            cm_expr(&w.cond, tracked, windows, mentions);
            cm_block(&w.body, tracked, windows, mentions);
        }
        Stmt::For(f) => {
            cm_expr(&f.start, tracked, windows, mentions);
            cm_expr(&f.end, tracked, windows, mentions);
            if let Some(step) = &f.step {
                cm_expr(step, tracked, windows, mentions);
            }
            cm_block(&f.body, tracked, windows, mentions);
        }
        Stmt::Match(m) => {
            cm_expr(&m.scrutinee, tracked, windows, mentions);
            for arm in &m.arms {
                cm_block(&arm.body, tracked, windows, mentions);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                cm_expr(v, tracked, windows, mentions);
            }
        }
        Stmt::Asm(a) => {
            for (_, target) in &a.outputs {
                cm_expr(target, tracked, windows, mentions);
            }
            for (_, value) in &a.inputs {
                cm_expr(value, tracked, windows, mentions);
            }
        }
        Stmt::Assume(a) => cm_expr(&a.cond, tracked, windows, mentions),
        Stmt::Assert(a) => cm_expr(&a.cond, tracked, windows, mentions),
        Stmt::Block(b) => cm_block(b, tracked, windows, mentions),
        Stmt::Claim(c) => {
            windows.push((c.name.0.clone(), c.body.span));
            cm_block(&c.body, tracked, windows, mentions);
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn cm_lvalue(
    lv: &LValue,
    tracked: &HashSet<&str>,
    windows: &mut Vec<(String, Span)>,
    mentions: &mut Vec<(String, Span)>,
) {
    match lv {
        LValue::Name((name, span)) => {
            if tracked.contains(name.as_str()) {
                mentions.push((name.clone(), *span));
            }
        }
        LValue::Field(base, _) => cm_lvalue(base, tracked, windows, mentions),
        LValue::Index(base, idx) => {
            cm_lvalue(base, tracked, windows, mentions);
            cm_expr(idx, tracked, windows, mentions);
        }
        LValue::Deref(e) => cm_expr(e, tracked, windows, mentions),
    }
}

fn cm_expr(
    expr: &Expr,
    tracked: &HashSet<&str>,
    windows: &mut Vec<(String, Span)>,
    mentions: &mut Vec<(String, Span)>,
) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Ident((name, span)) => {
            if tracked.contains(name.as_str()) {
                mentions.push((name.clone(), *span));
            }
        }
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            cm_expr(e, tracked, windows, mentions);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            cm_expr(l, tracked, windows, mentions);
            cm_expr(r, tracked, windows, mentions);
        }
        Expr::Call(callee, args) => {
            cm_expr(callee, tracked, windows, mentions);
            for a in args {
                cm_expr(a, tracked, windows, mentions);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            cm_expr(base, tracked, windows, mentions);
            if let Some(l) = len {
                cm_expr(l, tracked, windows, mentions);
            }
            if let Some(st) = stride {
                cm_expr(st, tracked, windows, mentions);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            cm_expr(base, tracked, windows, mentions);
            if let Some(c) = capacity {
                cm_expr(c, tracked, windows, mentions);
            }
            cm_expr(head, tracked, windows, mentions);
            cm_expr(len, tracked, windows, mentions);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            cm_expr(base, tracked, windows, mentions);
            if let Some(o) = bit_offset {
                cm_expr(o, tracked, windows, mentions);
            }
            if let Some(l) = len_bits {
                cm_expr(l, tracked, windows, mentions);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                cm_expr(e, tracked, windows, mentions);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                cm_expr(e, tracked, windows, mentions);
            }
        }
        Expr::Match(m) => {
            cm_expr(&m.scrutinee, tracked, windows, mentions);
            for arm in &m.arms {
                cm_block(&arm.body, tracked, windows, mentions);
            }
        }
        Expr::Block(b) => cm_block(&b.block, tracked, windows, mentions),
        Expr::If(i) => {
            cm_expr(&i.cond, tracked, windows, mentions);
            cm_block(&i.then_block, tracked, windows, mentions);
            cm_expr(&i.else_branch, tracked, windows, mentions);
        }
    }
}

/// Split a handoff register string (`Peripheral.REGISTER[.FIELD]`) into its
/// peripheral and register. A handoff always names at least a register.
fn handoff_register_path(s: &str) -> Option<(String, String)> {
    let mut parts = s.split('.');
    let p = parts.next()?;
    let r = parts.next()?;
    Some((p.to_string(), r.to_string()))
}

/// Collect every peripheral register/field *write* in the program. Walks all
/// function bodies exhaustively, including statements embedded in
/// block/if/match expressions, so a handoff write cannot hide inside an
/// expression. Reused by the provenance slice, which also acts at write sites.
fn collect_peripheral_writes(program: &Program, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    for item in &program.items {
        if let Item::FnDef(f) = item {
            walk_block(&f.body, symbols, out);
        }
    }
}

fn walk_block(block: &Block, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    for stmt in &block.stmts {
        walk_stmt(stmt, symbols, out);
    }
    if let Some(trailing) = &block.trailing {
        walk_expr(trailing, symbols, out);
    }
}

fn walk_stmt(stmt: &Stmt, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    match stmt {
        Stmt::VarDecl(vd) => walk_expr(&vd.init, symbols, out),
        Stmt::Assign(a) => {
            record_write(&a.target, Some(&a.value), symbols, out);
            walk_expr(&a.value, symbols, out);
        }
        Stmt::CompoundAssign(ca) => {
            // The RHS of a compound assign is not the stored value (it is one
            // operand of `target OP= value`), so it does not carry the
            // delivered-static or disabling-literal facts; pass None.
            record_write(&ca.target, None, symbols, out);
            walk_expr(&ca.value, symbols, out);
        }
        Stmt::Expr(e) => walk_expr(e, symbols, out),
        Stmt::If(i) => {
            walk_expr(&i.cond, symbols, out);
            walk_block(&i.then_block, symbols, out);
            if let Some(eb) = &i.else_branch {
                walk_stmt(eb, symbols, out);
            }
        }
        Stmt::Loop(l) => walk_block(&l.body, symbols, out),
        Stmt::Claim(c) => walk_block(&c.body, symbols, out),
        Stmt::While(w) => {
            walk_expr(&w.cond, symbols, out);
            walk_block(&w.body, symbols, out);
        }
        Stmt::For(f) => {
            walk_expr(&f.start, symbols, out);
            walk_expr(&f.end, symbols, out);
            if let Some(step) = &f.step {
                walk_expr(step, symbols, out);
            }
            walk_block(&f.body, symbols, out);
        }
        Stmt::Match(m) => {
            walk_expr(&m.scrutinee, symbols, out);
            for arm in &m.arms {
                walk_block(&arm.body, symbols, out);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                walk_expr(v, symbols, out);
            }
        }
        Stmt::Asm(a) => {
            // Asm output/input operands are expression places; a handoff write
            // would not take this form, but walk them so an embedded block is
            // still reached.
            for (_, target) in &a.outputs {
                walk_expr(target, symbols, out);
            }
            for (_, value) in &a.inputs {
                walk_expr(value, symbols, out);
            }
        }
        Stmt::Assume(a) => walk_expr(&a.cond, symbols, out),
        Stmt::Assert(a) => walk_expr(&a.cond, symbols, out),
        Stmt::Block(b) => walk_block(b, symbols, out),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

/// Exhaustive expression walk -- only needed to reach statements embedded in
/// block/if/match expressions, but every variant is matched (no catch-all) so
/// a new expression form cannot silently drop a nested write.
fn walk_expr(expr: &Expr, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::Ident(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            walk_expr(e, symbols, out);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            walk_expr(l, symbols, out);
            walk_expr(r, symbols, out);
        }
        Expr::Call(callee, args) => {
            walk_expr(callee, symbols, out);
            for a in args {
                walk_expr(a, symbols, out);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            walk_expr(base, symbols, out);
            if let Some(l) = len {
                walk_expr(l, symbols, out);
            }
            if let Some(s) = stride {
                walk_expr(s, symbols, out);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            walk_expr(base, symbols, out);
            if let Some(c) = capacity {
                walk_expr(c, symbols, out);
            }
            walk_expr(head, symbols, out);
            walk_expr(len, symbols, out);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            walk_expr(base, symbols, out);
            if let Some(o) = bit_offset {
                walk_expr(o, symbols, out);
            }
            if let Some(l) = len_bits {
                walk_expr(l, symbols, out);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                walk_expr(e, symbols, out);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                walk_expr(e, symbols, out);
            }
        }
        Expr::Match(m) => {
            walk_expr(&m.scrutinee, symbols, out);
            for arm in &m.arms {
                walk_block(&arm.body, symbols, out);
            }
        }
        Expr::Block(b) => walk_block(&b.block, symbols, out),
        Expr::If(i) => {
            walk_expr(&i.cond, symbols, out);
            walk_block(&i.then_block, symbols, out);
            walk_expr(&i.else_branch, symbols, out);
        }
    }
}

/// If `lv` is a write to a peripheral register or one of its fields, record it.
/// `P.R = x` is `Field(Name(P), R)` (field `None`); `P.R.F = x` is
/// `Field(Field(Name(P), R), F)` (field `Some(F)`). `rhs`, when present, supplies
/// the delivered-static (`= &X`) and disabling-literal (`= false`/`0`) facts.
fn record_write(
    lv: &LValue,
    rhs: Option<&Expr>,
    symbols: &SymbolTable,
    out: &mut Vec<PeriphWrite>,
) {
    let rhs_static = rhs.and_then(addr_of_static).map(str::to_string);
    let rhs_disabling = rhs.is_some_and(is_disabling);
    let rhs_literal = rhs.and_then(|e| literal_value(e).or_else(|| enum_discriminant(e, symbols)));
    if let LValue::Field(base, field) = lv {
        match base.as_ref() {
            // P.R = ...  (field is the register name)
            LValue::Name((p, _)) if symbols.peripherals.contains_key(p) => {
                out.push(PeriphWrite {
                    periph: p.clone(),
                    reg: field.0.clone(),
                    field: None,
                    span: field.1,
                    rhs_static: rhs_static.clone(),
                    rhs_disabling,
                    rhs_literal,
                });
            }
            // P.R.F = ...  (reg is the register, field is the field)
            LValue::Field(inner, reg) => {
                if let LValue::Name((p, _)) = inner.as_ref()
                    && symbols.peripherals.contains_key(p)
                {
                    out.push(PeriphWrite {
                        periph: p.clone(),
                        reg: reg.0.clone(),
                        field: Some(field.0.clone()),
                        span: field.1,
                        rhs_static: rhs_static.clone(),
                        rhs_disabling,
                        rhs_literal,
                    });
                }
            }
            // A non-peripheral name (local/struct), an indexed place, or a
            // pointer deref: not a peripheral-register write path.
            LValue::Name(_) | LValue::Index(..) | LValue::Deref(_) => {}
        }
    }
}

/// The compile-time value of a literal RHS (`2`, `true`, `(0)`), or `None`
/// for anything computed.
fn literal_value(e: &Expr) -> Option<u64> {
    match e {
        Expr::Group(inner) => literal_value(inner),
        Expr::BoolLiteral(b, _) => Some(u64::from(*b)),
        Expr::IntLiteral(v, _, _) => Some(*v),
        // Constant folds for the NVIC idioms: `1 << n` and OR-ed bit sets.
        Expr::Binary(l, crate::ast::BinaryOp::Shl, r) => {
            let (l, r) = (literal_value(l)?, literal_value(r)?);
            (r < 64).then(|| l << r)
        }
        Expr::Binary(l, crate::ast::BinaryOp::BitOr, r) => {
            Some(literal_value(l)? | literal_value(r)?)
        }
        _ => None,
    }
}

/// The discriminant of an enum-variant expression (`Enum@Variant`), resolved
/// against the symbol table. An enum-typed register field is written with a
/// variant, not an int literal, so the E618 `when field = N` cross-check uses
/// this to recover the numeric value (e.g. `CH0_CTRL_TRIG_DATA_SIZE@SIZE_WORD`
/// -> 2). `enum_name` is the flattened qualified name the resolver keyed on.
fn enum_discriminant(e: &Expr, symbols: &SymbolTable) -> Option<u64> {
    let Expr::EnumVariant {
        enum_name, variant, ..
    } = e
    else {
        return None;
    };
    let (_, variants) = symbols.enums.get(&enum_name.0)?;
    let disc = variants.iter().find(|(n, _)| *n == variant.0)?.1;
    u64::try_from(disc).ok()
}

/// Whether `e` is a provably-disabling literal (`false` or `0`), possibly
/// parenthesized. Used by the enable-presence check (E609): a `= false` / `= 0`
/// write to a clock-gate register does not count as enabling the agent. Any
/// other RHS (a non-zero literal, `true`, or a non-literal we cannot evaluate)
/// is treated as possibly-enabling, so the check never false-flags an agent
/// that is in fact enabled.
fn is_disabling(e: &Expr) -> bool {
    match e {
        Expr::Group(inner) => is_disabling(inner),
        Expr::BoolLiteral(b, _) => !b,
        Expr::IntLiteral(0, _, _) => true,
        _ => false,
    }
}

/// If `e` is the address of a static (`&S`, `&mut S`, or `&S[..]`, possibly
/// through parentheses or a cast like `&RX_DESC as u32`), return the static's
/// name. This is the descriptor-base delivery form for an in-memory handoff:
/// `Agent.HANDOFF = &RX_DESC` hands the agent the base of `RX_DESC`.
fn addr_of_static(e: &Expr) -> Option<&str> {
    match e {
        Expr::Group(inner) | Expr::Cast(inner, _) => addr_of_static(inner),
        Expr::Unary(UnaryOp::AddrOf | UnaryOp::AddrOfMut, inner) => match inner.as_ref() {
            Expr::Ident((name, _)) => Some(name.as_str()),
            // `&RX_DESC[0]` -- the base element of an array still delivers the
            // whole descriptor block to the agent.
            Expr::Index(base, _) => match base.as_ref() {
                Expr::Ident((name, _)) => Some(name.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// E608: the agent walks the delivered descriptor and dereferences every
/// `addr in R` field inside it, so it must reach each such region's mem block.
/// `validate_regions` already ensures the descriptor's *own* region is
/// reachable; this catches a field that points into a *different* region the
/// walking agent cannot reach (the DTCM footgun one level deeper).
fn check_descriptor_reach(
    static_name: &str,
    agent: &Agent,
    symbols: &SymbolTable,
    target: &Target,
    span: Span,
    diags: &mut DiagnosticBag,
) {
    let Some(sym) = symbols.statics.get(static_name) else {
        return;
    };
    let mut fields = Vec::new();
    collect_addr_fields(&sym.ty, String::new(), &mut fields);
    for (field_path, region_name) in fields {
        // An unknown region is already reported as E607 at the struct def; skip
        // it here rather than double-reporting against the delivery site.
        let Some(region) = target.regions.iter().find(|r| r.name == region_name) else {
            continue;
        };
        if !agent.reaches(&region.mem) {
            diags.error(
                format!(
                    "`{static_name}` is handed to agent `{}`, but its field `{field_path}` is \
                     `addr in {region_name}` (mem `{}`), which `{}` cannot reach. The agent \
                     would dereference an address outside its reach.",
                    agent.name, region.mem, agent.name
                ),
                "E608",
                span,
            );
        }
    }
}

/// Collect every `addr in R` field reachable in `ty` as (dotted-field-path,
/// region-name), descending through structs and arrays so a descriptor that
/// nests another descriptor (or an array of slots) is not a silent gap. The
/// catch-all covers scalar/view/pointer types, none of which carry an `addr`
/// slot (`addr` is field-only today; see `doc/regions-agents.md`).
fn collect_addr_fields(ty: &Type, prefix: String, out: &mut Vec<(String, String)>) {
    match ty {
        Type::Addr(region) => out.push((prefix, region.clone())),
        Type::Struct(_, _, struct_fields) => {
            for (fname, fty) in struct_fields {
                let path = if prefix.is_empty() {
                    fname.clone()
                } else {
                    format!("{prefix}.{fname}")
                };
                collect_addr_fields(fty, path, out);
            }
        }
        Type::Array(inner, _) => collect_addr_fields(inner, prefix, out),
        // Storage wrappers are layout-transparent; descend so an agent-shared
        // descriptor (a region static gets `Type::AgentShared` from derived-Move
        // before this check runs) still exposes its `addr in R` fields.
        Type::Exclusive(inner)
        | Type::Mmio(inner)
        | Type::AgentShared(inner)
        | Type::Shared(inner, _) => collect_addr_fields(inner, prefix, out),
        _ => {}
    }
}

// ─── Agent-pointer taint (volatile lowering + E620) ─────────────────────────
//
// Raw pointers cast from the address of an agent-shared static (`&X as *T`
// where `X` lives in an agent-mutated region) point at memory with a
// concurrent writer the optimizer cannot see: a plain LLVM load of such
// memory may be hoisted out of a loop, turning an OWN-bit spin into an
// infinite branch (observed on the H723, 2026-06-11). The emitter therefore
// lowers every access through such a pointer as `volatile`, and the checker
// (E620) keeps the pointer from escaping the deriving function -- escaped
// pointers would be dereferenced where the taint is invisible, losing the
// volatile lowering silently.
//
// Known limit (documented in doc/regions-agents.md): laundering the address
// through integer arithmetic (`(&X as u32 + off) as *T`) defeats the
// syntactic taint; the idiomatic direct-cast forms are what the lowering
// and E620 cover.

use crate::ast;
use std::collections::HashSet as TaintSet;

/// True when the static's type carries `AgentShared` in its storage wrapping.
#[must_use]
pub fn static_is_agent_shared(name: &str, symbols: &SymbolTable) -> bool {
    fn ty_has(t: &Type) -> bool {
        if matches!(t, Type::AgentShared(_)) {
            return true;
        }
        let inner = t.inner();
        if std::ptr::eq(inner, t) {
            false
        } else {
            ty_has(inner)
        }
    }
    symbols.statics.get(name).is_some_and(|s| ty_has(&s.ty))
}

/// Classifier: does this expression evaluate to an agent pointer? Either a
/// tainted local, or a (possibly cast-wrapped) address-of of an agent-shared
/// static (including `&X[i]` / `&X.f`). Classifier, so the catch-all is fine.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn is_agent_ptr_expr(e: &ast::Expr, locals: &TaintSet<String>, symbols: &SymbolTable) -> bool {
    match e {
        ast::Expr::Ident((n, _)) => locals.contains(n),
        ast::Expr::Group(i) => is_agent_ptr_expr(i, locals, symbols),
        // Only pointer-typed casts stay in the taint: `&BUF as u32` is the
        // handoff delivery idiom (an integer, checked by the handoff
        // machinery), and integer casts deliberately exit the taint -- the
        // documented laundering limit.
        ast::Expr::Cast(i, ty_expr) => {
            let target = crate::types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
            crate::types::is_ptr(&target) && is_agent_ptr_expr(i, locals, symbols)
        }
        ast::Expr::Unary(ast::UnaryOp::AddrOf | ast::UnaryOp::AddrOfMut, inner) => {
            match inner.as_ref() {
                ast::Expr::Ident((n, _)) => static_is_agent_shared(n, symbols),
                ast::Expr::Index(b, _) | ast::Expr::FieldAccess(b, _) => {
                    matches!(b.as_ref(), ast::Expr::Ident((n, _)) if static_is_agent_shared(n, symbols))
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// The set of locals in `body` holding agent pointers: seeded by
/// `&agent_static as *T` initializers/assignments, propagated through
/// local-to-local assignment, iterated to a fixpoint (use-before-def
/// textual order inside loops). Monotone -- once tainted, always tainted;
/// over-tainting only costs an extra `volatile`.
#[must_use]
pub fn agent_ptr_locals(body: &ast::Block, symbols: &SymbolTable) -> TaintSet<String> {
    let mut set = TaintSet::new();
    loop {
        let before = set.len();
        apl_block(body, symbols, &mut set);
        if set.len() == before {
            return set;
        }
    }
}

fn apl_block(b: &ast::Block, symbols: &SymbolTable, set: &mut TaintSet<String>) {
    for stmt in &b.stmts {
        apl_stmt(stmt, symbols, set);
    }
    if let Some(t) = &b.trailing {
        apl_expr(t, symbols, set);
    }
}

// Exhaustive Stmt walker (no catch-all; see hacking.md Code conventions).
fn apl_stmt(stmt: &ast::Stmt, symbols: &SymbolTable, set: &mut TaintSet<String>) {
    match stmt {
        ast::Stmt::VarDecl(v) => {
            apl_expr(&v.init, symbols, set);
            if is_agent_ptr_expr(&v.init, set, symbols) {
                set.insert(v.name.0.clone());
            }
        }
        ast::Stmt::Assign(a) => {
            apl_expr(&a.value, symbols, set);
            if let ast::LValue::Name((n, _)) = &a.target
                && is_agent_ptr_expr(&a.value, set, symbols)
            {
                set.insert(n.clone());
            }
        }
        ast::Stmt::CompoundAssign(c) => apl_expr(&c.value, symbols, set),
        ast::Stmt::Expr(e) => apl_expr(e, symbols, set),
        ast::Stmt::If(i) => {
            apl_expr(&i.cond, symbols, set);
            apl_block(&i.then_block, symbols, set);
            if let Some(e) = &i.else_branch {
                apl_stmt(e, symbols, set);
            }
        }
        ast::Stmt::Loop(l) => apl_block(&l.body, symbols, set),
        ast::Stmt::While(w) => {
            apl_expr(&w.cond, symbols, set);
            apl_block(&w.body, symbols, set);
        }
        ast::Stmt::For(f) => {
            apl_expr(&f.start, symbols, set);
            apl_expr(&f.end, symbols, set);
            if let Some(st) = &f.step {
                apl_expr(st, symbols, set);
            }
            apl_block(&f.body, symbols, set);
        }
        ast::Stmt::Return(r) => {
            if let Some(v) = &r.value {
                apl_expr(v, symbols, set);
            }
        }
        ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {}
        ast::Stmt::Block(b) => apl_block(b, symbols, set),
        ast::Stmt::Match(m) => {
            apl_expr(&m.scrutinee, symbols, set);
            for arm in &m.arms {
                apl_block(&arm.body, symbols, set);
            }
        }
        ast::Stmt::Asm(a) => {
            for (_, e) in &a.inputs {
                apl_expr(e, symbols, set);
            }
        }
        ast::Stmt::Assume(a) => apl_expr(&a.cond, symbols, set),
        ast::Stmt::Assert(a) => apl_expr(&a.cond, symbols, set),
        ast::Stmt::Claim(c) => apl_block(&c.body, symbols, set),
    }
}

// Exhaustive Expr walker: only block-bearing expressions can declare locals,
// but every arm recurses so nested block expressions are reached anywhere.
fn apl_expr(e: &ast::Expr, symbols: &SymbolTable, set: &mut TaintSet<String>) {
    match e {
        ast::Expr::IntLiteral(..)
        | ast::Expr::FloatLiteral(..)
        | ast::Expr::BoolLiteral(..)
        | ast::Expr::StringLiteral(..)
        | ast::Expr::NullLiteral(..)
        | ast::Expr::Ident(_)
        | ast::Expr::SizeOf(..)
        | ast::Expr::EnumVariant { .. } => {}
        ast::Expr::Unary(_, i) | ast::Expr::Group(i) | ast::Expr::Cast(i, _) => {
            apl_expr(i, symbols, set);
        }
        ast::Expr::Binary(l, _, r) => {
            apl_expr(l, symbols, set);
            apl_expr(r, symbols, set);
        }
        ast::Expr::Call(callee, args) => {
            apl_expr(callee, symbols, set);
            for a in args {
                apl_expr(a, symbols, set);
            }
        }
        ast::Expr::FieldAccess(b, _) => apl_expr(b, symbols, set),
        ast::Expr::Index(b, i) => {
            apl_expr(b, symbols, set);
            apl_expr(i, symbols, set);
        }
        ast::Expr::ViewNew {
            base, len, stride, ..
        } => {
            apl_expr(base, symbols, set);
            if let Some(l) = len {
                apl_expr(l, symbols, set);
            }
            if let Some(st) = stride {
                apl_expr(st, symbols, set);
            }
        }
        ast::Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            apl_expr(base, symbols, set);
            if let Some(c) = capacity {
                apl_expr(c, symbols, set);
            }
            apl_expr(head, symbols, set);
            apl_expr(len, symbols, set);
        }
        ast::Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            apl_expr(base, symbols, set);
            if let Some(o) = bit_offset {
                apl_expr(o, symbols, set);
            }
            if let Some(l) = len_bits {
                apl_expr(l, symbols, set);
            }
        }
        ast::Expr::ArrayInit(elems, _) => {
            for el in elems {
                apl_expr(el, symbols, set);
            }
        }
        ast::Expr::StructInit { fields, .. } => {
            for (_, v) in fields {
                apl_expr(v, symbols, set);
            }
        }
        ast::Expr::Match(m) => {
            apl_expr(&m.scrutinee, symbols, set);
            for arm in &m.arms {
                apl_block(&arm.body, symbols, set);
            }
        }
        ast::Expr::Block(b) => apl_block(&b.block, symbols, set),
        ast::Expr::If(i) => {
            apl_expr(&i.cond, symbols, set);
            apl_block(&i.then_block, symbols, set);
            apl_expr(&i.else_branch, symbols, set);
        }
    }
}
