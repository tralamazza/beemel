//! Region/agent checks that need both the program and the target file.
//!
//! Unlike the type checker and borrow checker, this pass takes the `Target`:
//! regions and agents are declared in the target file (the hardware physics),
//! while placement (`in <region>`) and ownership live in source. This is the
//! seam where the two meet.
//!
//! Slice 1 implements only the placement-name check: a static's `in <region>`
//! must name a real `[region.*]`. Ownership, handoff, and provenance checks
//! (slices 2-4 of `doc/regions-agents-plan.md`) extend this module.

use crate::ast::{Item, Program, StaticDef, StorageAnnotation};
use crate::errors::DiagnosticBag;
use crate::target::Target;

/// Run the region/agent checks against `target`.
pub fn check(program: &Program, target: &Target, diags: &mut DiagnosticBag) {
    for item in &program.items {
        if let Item::StaticDef(s) = item {
            check_static(s, target, diags);
        }
    }
}

fn check_static(s: &StaticDef, target: &Target, diags: &mut DiagnosticBag) {
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
