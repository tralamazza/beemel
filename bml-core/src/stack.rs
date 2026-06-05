use std::collections::{HashMap, HashSet};

use crate::ast::{self, Expr, Item, Program, Stmt};
use crate::resolver::SymbolTable;
use crate::types;

/// Per-function stack information for reporting.
#[derive(Debug)]
pub struct StackEntry {
    pub frame: u32,
    pub callees: Vec<String>,
    pub max_depth: u32,
}

/// Overall stack analysis report.
#[derive(Debug)]
pub struct StackReport {
    pub entries: HashMap<String, StackEntry>,
    pub roots: Vec<RootInfo>,
}

/// A root entry point (thread mode main or an ISR).
#[derive(Debug)]
pub struct RootInfo {
    pub name: String,
    pub kind: RootKind,
    pub total_depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Thread,
    Isr(u8),
}

/// Intermediate per-function data collected during the AST walk.
struct FnFrameInfo {
    frame: u32,
    direct_callees: Vec<String>,
    has_indirect_calls: bool,
}

#[must_use]
pub fn analyze(program: &Program, symbols: &SymbolTable) -> StackReport {
    let defined_fns = build_defined_fn_set(program);
    let mut infos: HashMap<String, FnFrameInfo> = HashMap::new();

    // Phase 1: compute per-function frame size, direct callees, and indirect-call flag
    for item in &program.items {
        if let Item::FnDef(ref fn_def) = *item {
            let info = compute_fn_info(fn_def, symbols, &defined_fns);
            infos.insert(fn_def.name.0.clone(), info);
        }
    }

    // Phase 2: propagate depths through the call graph (DFS with memoization)
    let mut visited: HashMap<String, u32> = HashMap::new();
    let mut in_path: HashSet<String> = HashSet::new();

    for name in infos.keys() {
        propagate_depth(name, &infos, &defined_fns, &mut visited, &mut in_path);
    }

    // Build the final report entries
    let mut entries: HashMap<String, StackEntry> = HashMap::new();
    for (name, info) in &infos {
        let depth = visited.get(name.as_str()).copied().unwrap_or(info.frame);
        entries.insert(
            name.clone(),
            StackEntry {
                frame: info.frame,
                callees: info.direct_callees.clone(),
                max_depth: depth,
            },
        );
    }

    // Phase 3: identify roots (main + ISRs)
    let roots = find_roots(program, &entries);

    StackReport { entries, roots }
}

fn build_defined_fn_set(program: &Program) -> HashSet<String> {
    let mut fns = HashSet::new();
    for item in &program.items {
        if let Item::FnDef(ref f) = *item {
            fns.insert(f.name.0.clone());
        }
    }
    fns
}

fn compute_fn_info(
    fn_def: &ast::FnDef,
    symbols: &SymbolTable,
    defined_fns: &HashSet<String>,
) -> FnFrameInfo {
    let mut frame: u32 = 0;

    // Parameters
    for param in &fn_def.params {
        let ty = types::resolve_type_expr(&param.ty, &symbols.structs, &symbols.enums);
        frame += types::element_size(&ty);
    }

    // Walk body for locals and callees
    let contrib = block_contribution(&fn_def.body, symbols, defined_fns);
    frame += contrib.frame;
    let mut direct_callees = contrib.callees;
    let has_indirect_calls = contrib.has_indirect;

    // Deduplicate direct callees and filter to known functions only
    direct_callees.sort();
    direct_callees.dedup();
    direct_callees
        .retain(|name| defined_fns.contains(name) || symbols.functions.contains_key(name));

    // Fixed overhead
    let is_isr = fn_def.isr.is_some();
    let has_calls = !direct_callees.is_empty() || has_indirect_calls;
    if has_calls {
        frame += 4; // LR push
    }
    if is_isr {
        frame += 32; // Cortex-M exception stack frame (xPSR, PC, LR, R12, R0-R3)
    }

    FnFrameInfo {
        frame,
        direct_callees,
        has_indirect_calls,
    }
}

fn propagate_depth(
    name: &str,
    infos: &HashMap<String, FnFrameInfo>,
    defined_fns: &HashSet<String>,
    visited: &mut HashMap<String, u32>,
    in_path: &mut HashSet<String>,
) -> u32 {
    if let Some(&depth) = visited.get(name) {
        return depth;
    }

    let Some(info) = infos.get(name) else {
        return 0;
    };

    // Recursion detection
    if !in_path.insert(name.to_string()) {
        // Already in current path → recursion
        eprintln!(
            "warning[W600]: recursive call chain involving `{name}`: stack depth may be under-estimated"
        );
        return info.frame;
    }

    let mut max_callee_depth: u32 = 0;

    // Direct callees
    for callee in &info.direct_callees {
        let d = propagate_depth(callee, infos, defined_fns, visited, in_path);
        if d > max_callee_depth {
            max_callee_depth = d;
        }
    }

    // Indirect callees (function pointers): worst-case across all defined functions
    if info.has_indirect_calls {
        for fn_name in defined_fns {
            if fn_name == name {
                continue;
            }
            let d = propagate_depth(fn_name, infos, defined_fns, visited, in_path);
            if d > max_callee_depth {
                max_callee_depth = d;
            }
        }
    }

    in_path.remove(name);

    let total = info.frame + max_callee_depth;
    visited.insert(name.to_string(), total);
    total
}

fn find_roots(program: &Program, entries: &HashMap<String, StackEntry>) -> Vec<RootInfo> {
    let mut roots = Vec::new();

    for item in &program.items {
        if let Item::FnDef(ref f) = *item {
            let name = f.name.0.clone();
            let depth = entries.get(&name).map_or(0, |e| e.max_depth);

            if let Some(isr) = &f.isr {
                let priority = isr.priority;
                roots.push(RootInfo {
                    name,
                    kind: RootKind::Isr(priority),
                    total_depth: depth,
                });
            } else if name == "main" {
                roots.push(RootInfo {
                    name,
                    kind: RootKind::Thread,
                    total_depth: depth,
                });
            }
        }
    }

    roots
}

// ─── AST walkers ────────────────────────────────────────────────────────

/// Frame + callee contributions from a block of statements.
struct Contribution {
    frame: u32,
    callees: Vec<String>,
    has_indirect: bool,
}

impl Contribution {
    fn empty() -> Self {
        Contribution {
            frame: 0,
            callees: Vec::new(),
            has_indirect: false,
        }
    }

    fn merge(mut self, other: Contribution) -> Self {
        self.frame += other.frame;
        self.callees.extend(other.callees);
        self.has_indirect = self.has_indirect || other.has_indirect;
        self
    }
}

fn block_contribution(
    block: &ast::Block,
    symbols: &SymbolTable,
    defined_fns: &HashSet<String>,
) -> Contribution {
    let mut acc = Contribution::empty();

    for stmt in &block.stmts {
        acc = acc.merge(stmt_contribution(stmt, symbols, defined_fns));
    }

    if let Some(trailing) = &block.trailing {
        let contrib = expr_contribution(trailing, symbols, defined_fns);
        acc.frame += contrib.frame;
        acc.callees.extend(contrib.callees);
        acc.has_indirect = acc.has_indirect || contrib.has_indirect;
    }

    acc
}

fn stmt_contribution(
    stmt: &Stmt,
    symbols: &SymbolTable,
    defined_fns: &HashSet<String>,
) -> Contribution {
    match stmt {
        Stmt::VarDecl(vd) => {
            let ty = if let Some(ty_ann) = &vd.ty_ann {
                types::resolve_type_expr(ty_ann, &symbols.structs, &symbols.enums)
            } else {
                infer_type_from_expr(&vd.init, symbols)
            };
            let size = types::element_size(&ty);

            // Also walk the init expression for callees
            let init_contrib = expr_contribution(&vd.init, symbols, defined_fns);

            Contribution {
                frame: size + init_contrib.frame,
                callees: init_contrib.callees,
                has_indirect: init_contrib.has_indirect,
            }
        }

        Stmt::For(for_stmt) => {
            let ty = types::resolve_type_expr(&for_stmt.ty, &symbols.structs, &symbols.enums);
            let size = types::element_size(&ty);

            let body_contrib = block_contribution(&for_stmt.body, symbols, defined_fns);

            // Walk range expressions for callees
            let start_contrib = expr_contribution(&for_stmt.start, symbols, defined_fns);
            let end_contrib = expr_contribution(&for_stmt.end, symbols, defined_fns);
            let step_contrib = for_stmt
                .step
                .as_ref()
                .map(|s| expr_contribution(s, symbols, defined_fns));

            Contribution {
                frame: size + body_contrib.frame,
                callees: {
                    let mut c = Vec::new();
                    c.extend(start_contrib.callees);
                    c.extend(end_contrib.callees);
                    if let Some(sc) = &step_contrib {
                        c.extend(sc.callees.iter().cloned());
                    }
                    c.extend(body_contrib.callees);
                    c
                },
                has_indirect: start_contrib.has_indirect
                    || end_contrib.has_indirect
                    || step_contrib.as_ref().is_some_and(|sc| sc.has_indirect)
                    || body_contrib.has_indirect,
            }
        }

        Stmt::If(if_stmt) => {
            let cond_contrib = expr_contribution(&if_stmt.cond, symbols, defined_fns);
            let then_contrib = block_contribution(&if_stmt.then_block, symbols, defined_fns);

            let else_contrib = if let Some(else_branch) = &if_stmt.else_branch {
                match else_branch.as_ref() {
                    Stmt::Block(block) => block_contribution(block, symbols, defined_fns),
                    Stmt::If(inner_if) => {
                        // else if -- wrap in a Block for uniform handling
                        let wrapped = ast::Block {
                            stmts: vec![Stmt::If(inner_if.clone())],
                            trailing: None,
                            span: inner_if.cond.span(),
                        };
                        block_contribution(&wrapped, symbols, defined_fns)
                    }
                    _ => Contribution::empty(),
                }
            } else {
                Contribution::empty()
            };

            // Frame: max across branches (only one executes)
            // Callees: union across all branches
            let mut callees = cond_contrib.callees;
            callees.extend(then_contrib.callees);
            callees.extend(else_contrib.callees);

            Contribution {
                frame: std::cmp::max(then_contrib.frame, else_contrib.frame),
                callees,
                has_indirect: cond_contrib.has_indirect
                    || then_contrib.has_indirect
                    || else_contrib.has_indirect,
            }
        }

        Stmt::Loop(loop_stmt) => block_contribution(&loop_stmt.body, symbols, defined_fns),

        Stmt::While(while_stmt) => {
            let cond_contrib = expr_contribution(&while_stmt.cond, symbols, defined_fns);
            let body_contrib = block_contribution(&while_stmt.body, symbols, defined_fns);

            let mut callees = cond_contrib.callees;
            callees.extend(body_contrib.callees);

            Contribution {
                frame: body_contrib.frame,
                callees,
                has_indirect: cond_contrib.has_indirect || body_contrib.has_indirect,
            }
        }

        Stmt::Match(match_stmt) => {
            let scrut_contrib = expr_contribution(&match_stmt.scrutinee, symbols, defined_fns);
            let mut max_frame: u32 = 0;
            let mut callees = scrut_contrib.callees;
            let mut has_indirect = scrut_contrib.has_indirect;

            for arm in &match_stmt.arms {
                let body_contrib = block_contribution(&arm.body, symbols, defined_fns);
                if body_contrib.frame > max_frame {
                    max_frame = body_contrib.frame;
                }
                callees.extend(body_contrib.callees);
                has_indirect = has_indirect || body_contrib.has_indirect;
            }

            Contribution {
                frame: max_frame,
                callees,
                has_indirect,
            }
        }

        Stmt::Return(ret) => {
            if let Some(value) = &ret.value {
                expr_contribution(value, symbols, defined_fns)
            } else {
                Contribution::empty()
            }
        }

        Stmt::Assign(assign) => {
            let val_contrib = expr_contribution(&assign.value, symbols, defined_fns);
            // LValue doesn't contain allocas, but may contain function calls
            let target_contrib = lvalue_contribution(&assign.target, symbols, defined_fns);
            let mut callees = val_contrib.callees;
            callees.extend(target_contrib.callees);

            Contribution {
                frame: val_contrib.frame + target_contrib.frame,
                callees,
                has_indirect: val_contrib.has_indirect || target_contrib.has_indirect,
            }
        }

        Stmt::CompoundAssign(ca) => {
            let val_contrib = expr_contribution(&ca.value, symbols, defined_fns);
            let target_contrib = lvalue_contribution(&ca.target, symbols, defined_fns);
            let mut callees = val_contrib.callees;
            callees.extend(target_contrib.callees);
            Contribution {
                frame: val_contrib.frame + target_contrib.frame,
                callees,
                has_indirect: val_contrib.has_indirect || target_contrib.has_indirect,
            }
        }

        Stmt::Expr(expr) => expr_contribution(expr, symbols, defined_fns),

        Stmt::Block(block) => block_contribution(block, symbols, defined_fns),

        Stmt::Asm(asm_stmt) => {
            let mut contribution = Contribution::empty();
            for (_, target) in &asm_stmt.outputs {
                contribution = contribution.merge(expr_contribution(target, symbols, defined_fns));
            }
            for (_, value) in &asm_stmt.inputs {
                contribution = contribution.merge(expr_contribution(value, symbols, defined_fns));
            }
            contribution
        }

        Stmt::Assume(assume) => expr_contribution(&assume.cond, symbols, defined_fns),

        Stmt::Assert(assert) => expr_contribution(&assert.cond, symbols, defined_fns),

        Stmt::Break(_) | Stmt::Continue(_) => Contribution::empty(),
    }
}

fn expr_contribution(
    expr: &Expr,
    symbols: &SymbolTable,
    defined_fns: &HashSet<String>,
) -> Contribution {
    match expr {
        Expr::Call(callee, args) => {
            let mut contribution = Contribution::empty();

            // Arguments may contain allocas (struct/array inits) and callees
            for arg in args {
                contribution = contribution.merge(expr_contribution(arg, symbols, defined_fns));
            }

            // Determine if this is a direct or indirect call
            if let Expr::Ident((name, _)) = callee.as_ref() {
                if defined_fns.contains(name) || symbols.functions.contains_key(name) {
                    contribution.callees.push(name.clone());
                } else {
                    // Ident doesn't match any known function → function pointer
                    contribution.has_indirect = true;
                }
            } else {
                // Non-ident callee expression → function pointer call
                contribution.has_indirect = true;
                let callee_contrib = expr_contribution(callee, symbols, defined_fns);
                contribution.frame += callee_contrib.frame;
                contribution.callees.extend(callee_contrib.callees);
                contribution.has_indirect =
                    contribution.has_indirect || callee_contrib.has_indirect;
            }

            contribution
        }

        Expr::StructInit { name, fields, .. } => {
            let struct_size = struct_frame_size(&name.0, symbols);
            let mut callees = Vec::new();
            let mut has_indirect = false;

            for (_, field_expr) in fields {
                let contrib = expr_contribution(field_expr, symbols, defined_fns);
                callees.extend(contrib.callees);
                has_indirect = has_indirect || contrib.has_indirect;
            }

            Contribution {
                frame: struct_size,
                callees,
                has_indirect,
            }
        }

        Expr::ArrayInit(elements, _) => {
            let count = elements.len() as u32;
            let elem_size = if let Some(first) = elements.first() {
                let ty = infer_type_from_expr(first, symbols);
                types::element_size(&ty)
            } else {
                4 // default for empty arrays
            };

            let mut contribution = Contribution::empty();
            for elem in elements {
                contribution = contribution.merge(expr_contribution(elem, symbols, defined_fns));
            }

            Contribution {
                frame: count * elem_size + contribution.frame,
                callees: contribution.callees,
                has_indirect: contribution.has_indirect,
            }
        }

        Expr::If(if_expr) => {
            let cond_contrib = expr_contribution(&if_expr.cond, symbols, defined_fns);
            let then_contrib = block_contribution(&if_expr.then_block, symbols, defined_fns);
            let else_contrib = expr_contribution(&if_expr.else_branch, symbols, defined_fns);

            let mut callees = cond_contrib.callees;
            callees.extend(then_contrib.callees);
            callees.extend(else_contrib.callees);

            // Frame: no extra alloca (if-expr doesn't generate a temp),
            // but branches may contain allocas -- take max
            Contribution {
                frame: std::cmp::max(then_contrib.frame, else_contrib.frame),
                callees,
                has_indirect: cond_contrib.has_indirect
                    || then_contrib.has_indirect
                    || else_contrib.has_indirect,
            }
        }

        Expr::Match(match_expr) => {
            let scrut_contrib = expr_contribution(&match_expr.scrutinee, symbols, defined_fns);
            let mut max_frame: u32 = 0;
            let mut callees = scrut_contrib.callees;
            let mut has_indirect = scrut_contrib.has_indirect;

            for arm in &match_expr.arms {
                let body_contrib = block_contribution(&arm.body, symbols, defined_fns);
                if body_contrib.frame > max_frame {
                    max_frame = body_contrib.frame;
                }
                callees.extend(body_contrib.callees);
                has_indirect = has_indirect || body_contrib.has_indirect;
            }

            Contribution {
                frame: max_frame,
                callees,
                has_indirect,
            }
        }

        Expr::Block(block_expr) => block_contribution(&block_expr.block, symbols, defined_fns),

        Expr::Binary(left, _, right) => {
            let left_contrib = expr_contribution(left, symbols, defined_fns);
            let right_contrib = expr_contribution(right, symbols, defined_fns);
            left_contrib.merge(right_contrib)
        }

        Expr::Unary(_, inner) => expr_contribution(inner, symbols, defined_fns),

        Expr::FieldAccess(base, _) => expr_contribution(base, symbols, defined_fns),

        Expr::Index(base, index) => {
            let base_contrib = expr_contribution(base, symbols, defined_fns);
            let index_contrib = expr_contribution(index, symbols, defined_fns);
            base_contrib.merge(index_contrib)
        }

        Expr::Group(inner) => expr_contribution(inner, symbols, defined_fns),

        Expr::Cast(inner, _) => expr_contribution(inner, symbols, defined_fns),

        Expr::SizeOf(_, _)
        | Expr::IntLiteral(_, _, _)
        | Expr::FloatLiteral(_, _, _)
        | Expr::BoolLiteral(_, _)
        | Expr::StringLiteral(_, _)
        | Expr::NullLiteral(_)
        | Expr::Ident(_) => Contribution::empty(),

        Expr::EnumVariant { .. } => Contribution::empty(),

        Expr::ViewNew {
            base, len, stride, ..
        } => {
            let mut c = expr_contribution(base, symbols, defined_fns);
            if let Some(len) = len {
                c = c.merge(expr_contribution(len, symbols, defined_fns));
            }
            if let Some(stride) = stride {
                c = c.merge(expr_contribution(stride, symbols, defined_fns));
            }
            c
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            let mut c = expr_contribution(base, symbols, defined_fns);
            if let Some(capacity) = capacity {
                c = c.merge(expr_contribution(capacity, symbols, defined_fns));
            }
            c.merge(expr_contribution(head, symbols, defined_fns))
                .merge(expr_contribution(len, symbols, defined_fns))
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            let mut c = expr_contribution(base, symbols, defined_fns);
            if let Some(bit_offset) = bit_offset {
                c = c.merge(expr_contribution(bit_offset, symbols, defined_fns));
            }
            if let Some(len_bits) = len_bits {
                c = c.merge(expr_contribution(len_bits, symbols, defined_fns));
            }
            c
        }
    }
}

fn lvalue_contribution(
    lvalue: &ast::LValue,
    symbols: &SymbolTable,
    defined_fns: &HashSet<String>,
) -> Contribution {
    match lvalue {
        ast::LValue::Name(_) => Contribution::empty(),
        ast::LValue::Field(base, _) => lvalue_contribution(base, symbols, defined_fns),
        ast::LValue::Index(base, index) => {
            let base_contrib = lvalue_contribution(base, symbols, defined_fns);
            let index_contrib = expr_contribution(index, symbols, defined_fns);
            base_contrib.merge(index_contrib)
        }
        ast::LValue::Deref(expr) => expr_contribution(expr, symbols, defined_fns),
    }
}

/// Infer the approximate type of an expression for stack-sizing purposes.
fn infer_type_from_expr(expr: &Expr, symbols: &SymbolTable) -> types::Type {
    match expr {
        Expr::IntLiteral(_, suffix, _) => match suffix {
            ast::IntSuffix::I8 => types::Type::I8,
            ast::IntSuffix::I16 => types::Type::I16,
            ast::IntSuffix::I32 | ast::IntSuffix::None => types::Type::I32,
            ast::IntSuffix::I64 => types::Type::I64,
            ast::IntSuffix::U8 => types::Type::U8,
            ast::IntSuffix::U16 => types::Type::U16,
            ast::IntSuffix::U32 => types::Type::U32,
            ast::IntSuffix::U64 => types::Type::U64,
        },
        Expr::FloatLiteral(_, suffix, _) => match suffix {
            ast::FloatSuffix::H => types::Type::F16,
            ast::FloatSuffix::F | ast::FloatSuffix::None => types::Type::F32,
            ast::FloatSuffix::D => types::Type::F64,
        },
        Expr::BoolLiteral(_, _) => types::Type::B1,
        Expr::StringLiteral(s, _) => {
            // Approximate as u8 array sized to string length
            types::Type::Array(Box::new(types::Type::U8), s.len())
        }
        Expr::NullLiteral(_) => types::Type::Ptr(Box::new(types::Type::U8)),
        Expr::StructInit { name, .. } => {
            // Look up the struct in the symbol table
            if let Some(fields) = symbols.structs.get(&name.0) {
                types::Type::Struct(name.0.clone(), fields.clone())
            } else {
                types::Type::U32 // safe default
            }
        }
        Expr::EnumVariant { enum_name, .. } => {
            if let Some((inner_ty, variants)) = symbols.enums.get(&enum_name.0) {
                types::Type::Enum(
                    enum_name.0.clone(),
                    Box::new(inner_ty.clone()),
                    variants.clone(),
                )
            } else {
                types::Type::U32
            }
        }
        Expr::ArrayInit(elements, _) => {
            // Infer element type from the first element
            if let Some(first) = elements.first() {
                let inner = infer_type_from_expr(first, symbols);
                types::Type::Array(Box::new(inner), elements.len())
            } else {
                types::Type::Array(Box::new(types::Type::U8), 0)
            }
        }
        Expr::Call(_, _) => types::Type::U32, // return type unknown, safe default
        _ => types::Type::U32,                // safe default for any expression
    }
}

/// Get the struct size in bytes by looking up its fields.
fn struct_frame_size(name: &str, symbols: &SymbolTable) -> u32 {
    if let Some(fields) = symbols.structs.get(name) {
        fields.iter().map(|(_, ty)| types::element_size(ty)).sum()
    } else {
        4 // safe default for unknown struct
    }
}

// ─── Report output ──────────────────────────────────────────────────────

/// Print a human-readable stack usage report.
///
/// # Panics
///
/// Panics if a stack entry has no callees while trying to format the callee list.
pub fn print_report(report: &StackReport, symbols: &SymbolTable, available_stack: u32) {
    println!();
    println!("--- Stack usage ---");

    let mut entries: Vec<(&String, &StackEntry)> = report.entries.iter().collect();
    entries.sort_by_key(|(name, _)| *name);

    for (name, entry) in &entries {
        let sym = symbols.functions.get(*name);
        let is_isr = sym.is_some_and(|s| s.context.is_isr());
        let ctx_str = if is_isr {
            if let Some(crate::context::Context::Isr(p)) = sym.map(|s| &s.context) {
                format!("isr(prio={p})")
            } else {
                "isr".to_string()
            }
        } else {
            "thread".to_string()
        };

        let callee_str = if entry.callees.is_empty() {
            "leaf".to_string()
        } else if entry.callees.len() <= 3 {
            format!("→ {}", entry.callees.join(", "))
        } else {
            format!(
                "→ {} (...) {}",
                entry.callees[0],
                entry.callees.last().unwrap()
            )
        };

        println!(
            "  fn {name:22} {ctx_str:16} frame={:>4}  total={:>6}  ({callee_str})",
            entry.frame, entry.max_depth
        );
    }

    println!();
    println!("--- Worst-case nesting ---");

    let mut thread_depth: u32 = 0;
    let mut isr_depths: Vec<(String, u32, u8)> = Vec::new();

    for root in &report.roots {
        match root.kind {
            RootKind::Thread => thread_depth = root.total_depth,
            RootKind::Isr(p) => {
                isr_depths.push((root.name.clone(), root.total_depth, p));
            }
        }
    }

    // Sort ISRs by priority (lower number = higher priority in ARM NVIC)
    isr_depths.sort_by_key(|(_, _, p)| *p);

    println!("  thread main = {thread_depth}");
    if let Some((name, depth, prio)) = isr_depths.first() {
        println!("  + ISR {name} (prio={prio}) = {depth}");
    }

    let worst_case = thread_depth + isr_depths.first().map_or(&0, |(_, d, _)| d);

    println!("  = {worst_case} bytes");

    let pct = if available_stack > 0 {
        (worst_case * 100).div_ceil(available_stack)
    } else {
        0
    };

    if worst_case <= available_stack {
        println!("  Stack utilization: {pct}% of {available_stack} available  ✓");
    } else {
        println!("  ⚠  Stack overflow: {worst_case} > {available_stack} available");
    }

    println!(
        "  {} function(s) analyzed, {} root(s)",
        report.entries.len(),
        report.roots.len()
    );
}
