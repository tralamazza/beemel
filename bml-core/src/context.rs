#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Thread,
    Isr(u8), // NVIC priority: lower = higher actual priority (ARM convention)
    Any,
}

impl Context {
    /// NVIC priority level. Lower = higher actual priority in ARM.
    /// Thread = 255 (arbitrary high number = lowest priority).
    #[must_use]
    pub fn level(self) -> u8 {
        match self {
            Context::Thread => 255,
            Context::Isr(p) => p,
            Context::Any => 0,
        }
    }

    #[must_use]
    pub fn is_isr(self) -> bool {
        matches!(self, Context::Isr(_))
    }

    /// Can this context access a resource with the given ceiling?
    ///
    /// Ceiling = the highest priority (lowest ARM number) among tasks that
    /// access the resource. Tasks at equal or lower priority can access.
    /// Tasks at HIGHER priority than the ceiling are rejected.
    ///
    /// Thread context always passes (auto-inserts critical section).
    #[must_use]
    pub fn can_access(self, ceiling: u8) -> bool {
        match self {
            Context::Thread => true,
            Context::Any => true,
            Context::Isr(p) => p >= ceiling,
        }
    }

    /// Does access require a critical section (cpsid i / cpsie i)?
    ///
    /// Thread always needs one. ISRs only need one if their priority
    /// is lower (higher number) than the ceiling -- meaning higher-priority
    /// ISRs that also use this resource could preempt them.
    #[must_use]
    pub fn needs_critical_section(self, ceiling: u8) -> bool {
        match self {
            Context::Thread => true,
            Context::Isr(p) => p > ceiling,
            Context::Any => true, // conservative: unknown caller context
        }
    }
}
