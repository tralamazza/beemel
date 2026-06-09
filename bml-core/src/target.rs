use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone)]
pub struct Target {
    pub arch: String,
    /// Cortex-M cpu (`cortex-m0`, `cortex-m0plus`, `cortex-m3`, `cortex-m4`, `cortex-m7`).
    /// Optional in the target file; needed by `bml cflags` to disambiguate within an arch
    /// (e.g. armv7em covers both M4 and M7).
    pub cpu: Option<String>,
    pub priority_bits: u8,
    pub has_fpu: bool,
    pub has_bitband: bool,
    pub has_mpu: bool,
    pub flash_base: u64,
    pub flash_size: u64,
    pub ram_base: u64,
    pub ram_size: u64,
    pub vector_table_offset: u64,
    pub interrupts: HashMap<String, u16>,
    /// MMIO read-modify-write OR writes applied at the very start of
    /// `reset_handler`, before `.data`/`.bss` init -- the equivalent of CMSIS
    /// `SystemInit` running before the RAM copy. Each entry is `(address,
    /// or_mask)` meaning `*(volatile u32*)address |= or_mask`. The canonical use
    /// is enabling a RAM clock (e.g. STM32H7 D2 SRAM) when the stack lives in a
    /// clock-gated SRAM, since `reset_handler` touches RAM before `main` runs.
    pub startup_init: Vec<(u64, u64)>,
    /// Named memory blocks from `[mem.*]` sections. Generalizes the flat
    /// `flash_*`/`ram_*` pair: on parts with multiple RAMs of differing
    /// reachability (TCM vs AHB SRAM), each block is named so regions and agent
    /// reach can refer to it. Empty on legacy target files that use only the
    /// flat keys.
    pub mem_blocks: Vec<MemBlock>,
    /// Hardware agents from `[agent.*]` sections -- bus masters (DMA engines,
    /// debug probe) that touch memory on their own initiative. The cpu agent is
    /// implicit (derived from `cpu`); these are the others.
    pub agents: Vec<Agent>,
    /// Memory regions from `[region.*]` sections: a slice of a mem block shared
    /// by a set of agents.
    pub regions: Vec<Region>,
}

/// A named memory block (`[mem.NAME]`).
#[derive(Debug, Clone)]
pub struct MemBlock {
    pub name: String,
    pub base: u64,
    pub size: u64,
    /// Whether this memory is mapped cacheable (write-back/write-through). The
    /// cache-discipline check (failure mode #3) uses it: a cacheable block
    /// shared by a cached CPU and a non-snooping agent has diverging views.
    /// Defaults to `true` (assume cacheable -- the dangerous case -- so a region
    /// shared with a non-coherent agent must be declared `cacheable = false`).
    pub cacheable: bool,
}

impl MemBlock {
    /// Half-open end address (`base + size`).
    #[must_use]
    pub fn end(&self) -> u64 {
        self.base + self.size
    }
}

/// Treatment class of an agent -- decides which checks apply, not what the
/// silicon is called. See `doc/regions-agents-plan.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// Executes BML; the compiler emits its accesses (the implicit one).
    Cpu,
    /// Peripheral-driven bus master; addresses are handed to it via registers.
    Dma,
    /// Debug probe (SWD AHB-AP); built-in, drives nothing in source.
    /// Deliberately inert to the concurrency checks (cache discipline,
    /// derived-Move): it touches memory while the CPU is halted, so it is not a
    /// runtime concurrent mutator -- skipping it is correct, not an oversight.
    /// Its only plausible future job is security (`TrustZone`: "the debugger can
    /// reach this secure region"), a deferred track.
    Debug,
    /// Foreign code; channels only.
    External,
}

/// Whether a bus-master agent can write memory or only read it.
///
/// DORMANT (parsed from `access = read | rw`, default `ReadWrite`, but no check
/// consumes it yet -- a deliberate placeholder, not an oversight). Its one real
/// consumer in this silicon family is the LCD-TFT controller (LTDC), the *only*
/// intrinsically read-only bus master on the STM32H7 (RM0468 Table 2: every
/// other master -- the DMAs, SDMMC, ETH, USB, and even DMA2D/Chrom-Art -- reads
/// *and* writes). When an LTDC-style agent appears, `access = read` should relax
/// derived-Move for its region: the CPU *produces* the framebuffer, so the
/// index-read protection is both unnecessary and counterproductive (you read
/// pixels back). Cache discipline still applies (CPU writes cached, LTDC reads
/// stale). Until such an agent exists, building that check would be premature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess {
    ReadWrite,
    Read,
}

/// How a handoff register encodes the address written to it. Closed set: each
/// encoding is a codegen rule, so adding one is a deliberate compiler change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffEncoding {
    /// Register holds the byte address verbatim.
    ByteAddr,
    /// Register field holds `address >> 2` (the SVD field starts at bit 2).
    WordAddr,
}

/// A handoff register: the place where a written number becomes an address the
/// agent will dereference. `register` is an unresolved SVD path string
/// (`Peripheral.REGISTER.FIELD`); resolution against the peripheral table
/// happens later, where the SVD is visible.
#[derive(Debug, Clone)]
pub struct Handoff {
    pub register: String,
    pub encoding: HandoffEncoding,
    /// Optional minimum byte alignment of the handed-off address (power of two).
    pub align: Option<u32>,
}

/// A hardware agent (`[agent.NAME]`).
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub kind: AgentKind,
    /// Names of mem blocks this agent can reach. Empty + `reach_all` means "all".
    pub reach: Vec<String>,
    /// `reach = *`: the agent reaches every mem block (e.g. the debug probe).
    pub reach_all: bool,
    /// Whether this agent's view of `reach` is cached/snooped.
    pub cached: bool,
    pub access: AgentAccess,
    pub handoffs: Vec<Handoff>,
    /// Register paths that must be enabled for the agent to operate (clock gates).
    pub enabled_by: Vec<String>,
}

impl Agent {
    /// Whether this agent can reach the named mem block.
    #[must_use]
    pub fn reaches(&self, mem: &str) -> bool {
        self.reach_all || self.reach.iter().any(|r| r == mem)
    }
}

/// A memory region (`[region.NAME]`): a slice of a mem block shared by agents.
#[derive(Debug, Clone)]
pub struct Region {
    pub name: String,
    /// Name of the mem block this region is carved from.
    pub mem: String,
    /// Names of agents sharing the region. The cpu agent is always implicit.
    pub agents: Vec<String>,
}

/// Which section the parser is currently inside. Subsectioned headers
/// (`[mem.itcm]`) carry the index of the block being filled so subsequent
/// key/value lines append to it.
enum Section {
    Top,
    Interrupts,
    Startup,
    Mem(usize),
    Agent(usize),
    Region(usize),
}

impl Default for Target {
    fn default() -> Self {
        Target {
            arch: "armv7em".into(),
            cpu: None,
            priority_bits: 4,
            has_fpu: false,
            has_bitband: true,
            has_mpu: true,
            flash_base: 0x0800_0000,
            flash_size: 256 * 1024,
            ram_base: 0x2000_0000,
            ram_size: 64 * 1024,
            vector_table_offset: 0x0800_0000,
            interrupts: HashMap::new(),
            startup_init: Vec::new(),
            mem_blocks: Vec::new(),
            agents: Vec::new(),
            regions: Vec::new(),
        }
    }
}

impl Target {
    /// Load target configuration from a file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or contains invalid content.
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let content =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&content)
    }

    /// Parse target configuration from a string.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is malformed or contains unknown keys.
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut target = Target::default();
        let mut section = Section::Top;

        for (line_num, line) in input.lines().enumerate() {
            let line = line.trim();
            // Skip comments and blank lines
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            // Section header
            if line.starts_with('[') && line.ends_with(']') {
                let header = &line[1..line.len() - 1];
                section = match header.split_once('.') {
                    Some(("mem", name)) => {
                        target.mem_blocks.push(MemBlock {
                            name: name.to_string(),
                            base: 0,
                            size: 0,
                            cacheable: true,
                        });
                        Section::Mem(target.mem_blocks.len() - 1)
                    }
                    Some(("agent", name)) => {
                        target.agents.push(Agent {
                            name: name.to_string(),
                            kind: AgentKind::Dma,
                            reach: Vec::new(),
                            reach_all: false,
                            cached: false,
                            access: AgentAccess::ReadWrite,
                            handoffs: Vec::new(),
                            enabled_by: Vec::new(),
                        });
                        Section::Agent(target.agents.len() - 1)
                    }
                    Some(("region", name)) => {
                        target.regions.push(Region {
                            name: name.to_string(),
                            mem: String::new(),
                            agents: Vec::new(),
                        });
                        Section::Region(target.regions.len() - 1)
                    }
                    _ => match header {
                        "interrupts" => Section::Interrupts,
                        "startup" => Section::Startup,
                        other => {
                            return Err(format!(
                                "line {}: unknown section `[{other}]`",
                                line_num + 1
                            ));
                        }
                    },
                };
                continue;
            }
            // In [mem.NAME] section: base/size
            if let Section::Mem(idx) = section {
                let (key, val) = split_kv(line, line_num)?;
                let block = &mut target.mem_blocks[idx];
                match key {
                    "base" => {
                        block.base = parse_int(val).map_err(|_| {
                            format!("line {}: invalid mem base `{val}`", line_num + 1)
                        })?;
                    }
                    "size" => {
                        block.size = parse_int(val).map_err(|_| {
                            format!("line {}: invalid mem size `{val}`", line_num + 1)
                        })?;
                    }
                    "cacheable" => {
                        block.cacheable = parse_bool(val, "cacheable", line_num)?;
                    }
                    _ => {
                        return Err(format!("line {}: unknown mem key `{key}`", line_num + 1));
                    }
                }
                continue;
            }
            // In [agent.NAME] section
            if let Section::Agent(idx) = section {
                let (key, val) = split_kv(line, line_num)?;
                parse_agent_kv(&mut target.agents[idx], key, val, line_num)?;
                continue;
            }
            // In [region.NAME] section
            if let Section::Region(idx) = section {
                let (key, val) = split_kv(line, line_num)?;
                let region = &mut target.regions[idx];
                match key {
                    "mem" => region.mem = val.to_string(),
                    "agents" => region.agents = parse_list(val),
                    _ => {
                        return Err(format!("line {}: unknown region key `{key}`", line_num + 1));
                    }
                }
                continue;
            }
            // In [interrupts] section: name = slot
            if let Section::Interrupts = section {
                let (label, slot) = line.split_once('=').ok_or_else(|| {
                    format!(
                        "line {}: expected `label = slot`, got `{line}`",
                        line_num + 1
                    )
                })?;
                let label = label.trim().to_string();
                let slot: u16 = slot.trim().parse().map_err(|_| {
                    format!("line {}: invalid interrupt slot `{slot}`", line_num + 1)
                })?;
                target.interrupts.insert(label, slot);
                continue;
            }
            // In [startup] section: address = or_mask (RMW: *address |= or_mask)
            if let Section::Startup = section {
                let (addr_s, mask_s) = line.split_once('=').ok_or_else(|| {
                    format!(
                        "line {}: expected `address = or_mask`, got `{line}`",
                        line_num + 1
                    )
                })?;
                let addr = parse_int(addr_s.trim()).map_err(|_| {
                    format!(
                        "line {}: invalid startup address `{}`",
                        line_num + 1,
                        addr_s.trim()
                    )
                })?;
                let mask = parse_int(mask_s.trim()).map_err(|_| {
                    format!(
                        "line {}: invalid startup or_mask `{}`",
                        line_num + 1,
                        mask_s.trim()
                    )
                })?;
                if addr > u64::from(u32::MAX) || mask > u64::from(u32::MAX) {
                    return Err(format!(
                        "line {}: startup address/or_mask must fit in 32 bits",
                        line_num + 1
                    ));
                }
                target.startup_init.push((addr, mask));
                continue;
            }
            // Top-level key = value
            let (key, val) = line.split_once('=').ok_or_else(|| {
                format!(
                    "line {}: expected `key = value`, got `{line}`",
                    line_num + 1
                )
            })?;
            let key = key.trim();
            let val = val.trim();

            match key {
                "arch" => {
                    if !is_supported_arch(val) {
                        return Err(format!(
                            "line {}: unknown arch `{val}` (expected armv6m, armv7m, or armv7em)",
                            line_num + 1
                        ));
                    }
                    target.arch = val.to_string();
                }
                "cpu" => target.cpu = Some(val.to_string()),
                "priority_bits" => {
                    target.priority_bits = val.parse::<u8>().map_err(|_| {
                        format!("line {}: invalid priority_bits: {val}", line_num + 1)
                    })?;
                }
                "has_fpu" => target.has_fpu = parse_bool(val, key, line_num)?,
                "has_bitband" => target.has_bitband = parse_bool(val, key, line_num)?,
                "has_mpu" => target.has_mpu = parse_bool(val, key, line_num)?,
                "flash_base" => {
                    target.flash_base = parse_int(val)
                        .map_err(|_| format!("line {}: invalid flash_base: {val}", line_num + 1))?;
                }
                "flash_size" => {
                    target.flash_size = parse_int(val)
                        .map_err(|_| format!("line {}: invalid flash_size: {val}", line_num + 1))?;
                }
                "ram_base" => {
                    target.ram_base = parse_int(val)
                        .map_err(|_| format!("line {}: invalid ram_base: {val}", line_num + 1))?;
                }
                "ram_size" => {
                    target.ram_size = parse_int(val)
                        .map_err(|_| format!("line {}: invalid ram_size: {val}", line_num + 1))?;
                }
                "vector_table_offset" => {
                    target.vector_table_offset = parse_int(val).map_err(|_| {
                        format!("line {}: invalid vector_table_offset: {val}", line_num + 1)
                    })?;
                }
                _ => return Err(format!("line {}: unknown key `{key}`", line_num + 1)),
            }
        }
        // ARMv6-M (Cortex-M0/M0+) does not support bit-banding
        if target.has_bitband && target.arch == "armv6m" {
            eprintln!(
                "warning: ARMv6-M does not support bit-banding; ignoring `has_bitband = true`"
            );
            target.has_bitband = false;
        }

        target.validate_regions()?;

        Ok(target)
    }

    /// Self-consistency checks on `[mem.*]`/`[agent.*]`/`[region.*]` that need
    /// only the target file (no SVD). These fail loudly at target load, before
    /// any source is compiled -- the DTCM-footgun class dies here.
    ///
    /// Register paths (`handoff`/`enabled_by`) are NOT checked: `target.rs`
    /// cannot see the program's peripheral table. They are resolved later.
    fn validate_regions(&self) -> Result<(), String> {
        // Mem blocks must be non-empty and non-overlapping.
        for m in &self.mem_blocks {
            if m.size == 0 {
                return Err(format!("mem `{}` has zero size", m.name));
            }
        }
        for (i, a) in self.mem_blocks.iter().enumerate() {
            for b in &self.mem_blocks[i + 1..] {
                if a.base < b.end() && b.base < a.end() {
                    return Err(format!(
                        "mem `{}` (0x{:08X}..0x{:08X}) overlaps `{}` (0x{:08X}..0x{:08X})",
                        a.name,
                        a.base,
                        a.end(),
                        b.name,
                        b.base,
                        b.end()
                    ));
                }
            }
        }
        // Agent reach must name real mem blocks; handoff align must be pow2.
        for agent in &self.agents {
            for r in &agent.reach {
                if !self.mem_blocks.iter().any(|m| &m.name == r) {
                    return Err(format!("agent `{}` reaches unknown mem `{r}`", agent.name));
                }
            }
            for h in &agent.handoffs {
                if let Some(n) = h.align
                    && !n.is_power_of_two()
                {
                    return Err(format!(
                        "agent `{}` handoff `{}` align {n} is not a power of two",
                        agent.name, h.register
                    ));
                }
            }
        }
        // The mem-block-driven linker script needs a flash block (for .text)
        // and a ram block (for .data/.bss/.stack). Required only when regions
        // are present, since that is what switches generation to mem blocks.
        if !self.regions.is_empty() {
            if self.flash_block().is_none() {
                return Err(format!(
                    "regions are defined but no mem block contains flash_base (0x{:08X}); \
                     add a [mem.*] covering it",
                    self.flash_base
                ));
            }
            if self.ram_block().is_none() {
                return Err(format!(
                    "regions are defined but no mem block contains ram_base (0x{:08X}); \
                     add a [mem.*] covering it",
                    self.ram_base
                ));
            }
        }
        // Regions must name a real mem block and real agents, and the region's
        // memory must lie within the reach of every listed agent.
        for region in &self.regions {
            if !self.mem_blocks.iter().any(|m| m.name == region.mem) {
                return Err(format!(
                    "region `{}` uses unknown mem `{}`",
                    region.name, region.mem
                ));
            }
            for a in &region.agents {
                let agent =
                    self.agents.iter().find(|ag| &ag.name == a).ok_or_else(|| {
                        format!("region `{}` lists unknown agent `{a}`", region.name)
                    })?;
                if !agent.reaches(&region.mem) {
                    return Err(format!(
                        "region `{}` is in mem `{}`, which agent `{a}` cannot reach",
                        region.name, region.mem
                    ));
                }
            }

            // Cache discipline (failure mode #3): if the region's memory is
            // cacheable and it is accessed by both a cached CPU (D-cache on) and
            // a non-snooping agent (a DMA/external master that does not see the
            // cache), their views diverge -- the CPU reads/writes the cache, the
            // agent physical memory. Require the memory to be non-cacheable. The
            // CPU is implicit via `reaches` (software touches the region); the
            // non-snooping agent is one the region explicitly lists.
            if let Some(mem) = self.mem_blocks.iter().find(|m| m.name == region.mem)
                && mem.cacheable
            {
                let non_snooping = region.agents.iter().find_map(|a| {
                    self.agents.iter().find(|ag| {
                        &ag.name == a
                            && matches!(ag.kind, AgentKind::Dma | AgentKind::External)
                            && !ag.cached
                    })
                });
                let cached_cpu = self.agents.iter().find(|ag| {
                    matches!(ag.kind, AgentKind::Cpu) && ag.cached && ag.reaches(&region.mem)
                });
                if let (Some(dma), Some(cpu)) = (non_snooping, cached_cpu) {
                    return Err(format!(
                        "region `{}` is in cacheable mem `{}`, accessed by the cached CPU `{}` \
                         and the non-snooping agent `{}`; their cache views diverge. Mark mem \
                         `{}` `cacheable = false` (and configure it non-cacheable), or make the \
                         agent cache-coherent.",
                        region.name, region.mem, cpu.name, dma.name, region.mem
                    ));
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn to_arch(&self) -> crate::arch::Arch {
        match self.arch.as_str() {
            "armv6m" => crate::arch::Arch::Armv6m,
            "armv7m" => crate::arch::Arch::Armv7m,
            "armv7em" => crate::arch::Arch::Armv7em,
            other => unreachable!("target arch was validated during parse: {other}"),
        }
    }

    /// L1 D-cache line size in bytes, from the CPU. Cortex-M7 has a 32-byte
    /// line; the cacheless cores (M0/M0+/M3/M4) return 0 (no cache-line physics).
    #[must_use]
    pub fn cache_line_size(&self) -> u32 {
        match self.cpu.as_deref() {
            Some("cortex-m7") => 32,
            _ => 0,
        }
    }

    /// Derived alignment floor per region (bytes). A region whose memory is
    /// cacheable and that is shared with a non-coherent agent (a DMA/external
    /// master that does not snoop the cache, `cached = false`) must be
    /// cache-line aligned, so per-buffer cache maintenance stays line-granular
    /// and does not corrupt line-neighbors. The line size comes from the CPU.
    /// This is the physics that replaces hand-written `@align` on agent-shared
    /// statics; regions without it have no floor. See `doc/regions-agents-plan.md`.
    #[must_use]
    pub fn region_alignments(&self) -> HashMap<String, u32> {
        let line = self.cache_line_size();
        let mut out = HashMap::new();
        if line == 0 {
            return out;
        }
        for region in &self.regions {
            let Some(mem) = self.mem_blocks.iter().find(|m| m.name == region.mem) else {
                continue;
            };
            if !mem.cacheable {
                continue;
            }
            let non_coherent = region.agents.iter().any(|a| {
                self.agents.iter().any(|ag| {
                    &ag.name == a
                        && matches!(ag.kind, AgentKind::Dma | AgentKind::External)
                        && !ag.cached
                })
            });
            if non_coherent {
                out.insert(region.name.clone(), line);
            }
        }
        out
    }

    #[must_use]
    pub fn to_llvm_target_triple(&self) -> &'static str {
        self.to_arch().llvm_target_triple()
    }

    /// Compute the `arm-none-eabi-gcc` flags implied by this target.
    /// Always emits `-mthumb`. Requires `cpu` to be set; for FPU-capable cpus the FPU
    /// variant defaults to a sensible single-precision choice (override by editing the
    /// `system_*.c` build command directly if you need DP).
    ///
    /// # Errors
    /// Returns an error if `cpu` is missing or unrecognized.
    pub fn to_gcc_flags(&self) -> Result<Vec<String>, String> {
        let cpu = self.cpu.as_deref().ok_or_else(|| {
            "target file has no `cpu = cortex-mX`; add one to use `bml cflags`".to_string()
        })?;

        let (has_fpu_capable, default_fpu): (bool, &'static str) = match cpu {
            "cortex-m0" | "cortex-m0plus" | "cortex-m3" => (false, ""),
            "cortex-m4" => (true, "fpv4-sp-d16"),
            "cortex-m7" => (true, "fpv5-d16"),
            other => {
                return Err(format!(
                    "unrecognized cpu `{other}` (expected cortex-m0, cortex-m0plus, cortex-m3, cortex-m4, or cortex-m7)"
                ));
            }
        };

        let mut flags = vec![format!("-mcpu={cpu}"), "-mthumb".to_string()];
        if has_fpu_capable && self.has_fpu {
            flags.push("-mfloat-abi=hard".to_string());
            flags.push(format!("-mfpu={default_fpu}"));
        } else {
            flags.push("-mfloat-abi=soft".to_string());
        }
        Ok(flags)
    }

    /// The mem block that contains `flash_base` (holds `.text`/`.rodata` and is
    /// the load region for `.data`). Only meaningful when `[mem.*]` is used.
    #[must_use]
    pub fn flash_block(&self) -> Option<&MemBlock> {
        self.mem_blocks
            .iter()
            .find(|m| self.flash_base >= m.base && self.flash_base < m.end())
    }

    /// The mem block that contains `ram_base` (holds `.data`/`.bss`/`.stack`).
    #[must_use]
    pub fn ram_block(&self) -> Option<&MemBlock> {
        self.mem_blocks
            .iter()
            .find(|m| self.ram_base >= m.base && self.ram_base < m.end())
    }

    #[must_use]
    pub fn generate_linker_script(&self) -> String {
        if self.regions.is_empty() {
            self.generate_flat_linker_script()
        } else {
            self.generate_region_linker_script()
        }
    }

    /// The legacy single-FLASH/single-RAM layout, used when no `[region.*]`
    /// sections are present. Unchanged behavior for existing targets.
    fn generate_flat_linker_script(&self) -> String {
        let flash_base = format!("0x{:08X}", self.flash_base);
        let flash_size = format_size(self.flash_size);
        let ram_base = format!("0x{:08X}", self.ram_base);
        let ram_size = format_size(self.ram_size);
        let vt_offset = format!("0x{:08X}", self.vector_table_offset);

        format!(
            r"/* Auto-generated linker script for bml */
MEMORY
{{
  FLASH (rx) : ORIGIN = {flash_base}, LENGTH = {flash_size}
  RAM   (rwx) : ORIGIN = {ram_base}, LENGTH = {ram_size}
}}

ENTRY(reset_handler)

SECTIONS
{{
  .vector_table {vt_offset} :
  {{
    KEEP(*(.vector_table))
  }} > FLASH

  .text :
  {{
    *(.text*)
    *(.rodata*)
  }} > FLASH

  .data :
  {{
    _sdata = .;
    *(.data*)
    _edata = .;
    _sidata = LOADADDR(.data);
  }} > RAM AT > FLASH

  .bss :
  {{
    _sbss = .;
    *(.bss*)
    _ebss = .;
  }} > RAM

  .stack (NOLOAD) :
  {{
    . = ALIGN(8);
    _stack_bottom = .;
    . = . + 0x800; /* 2KB stack */
    _stack_top = .;
  }} > RAM

  /DISCARD/ :
  {{
    *(.ARM.exidx*)
    *(.ARM.attributes*)
  }}
}}
"
        )
    }

    /// Mem-block-driven layout, used when `[region.*]` sections are present.
    /// Every mem block becomes a MEMORY entry at its own address; `.text` goes
    /// to the flash block, `.data`/`.bss`/`.stack` to the ram block, and each
    /// region into its own mem block's `.region.<name>` section. This is what
    /// lets a DMA region sit at a different physical address (e.g. a specific
    /// SRAM) than the CPU's working RAM.
    ///
    /// `validate_regions` guarantees a flash block and a ram block exist before
    /// we get here, so the `expect`s below cannot fire on a parsed target.
    fn generate_region_linker_script(&self) -> String {
        use std::fmt::Write;

        let flash = self
            .flash_block()
            .expect("validate_regions ensures a flash block");
        let ram = self
            .ram_block()
            .expect("validate_regions ensures a ram block");
        let vt_offset = format!("0x{:08X}", self.vector_table_offset);

        // One MEMORY entry per mem block. The flash-containing block is rx; all
        // others are rwx (RAM-side, including DMA regions the CPU also writes).
        let mut memory = String::new();
        for m in &self.mem_blocks {
            let perms = if m.name == flash.name { "rx" } else { "rwx" };
            let _ = writeln!(
                memory,
                "  {} ({perms}) : ORIGIN = 0x{:08X}, LENGTH = {}",
                m.name,
                m.base,
                format_size(m.size)
            );
        }

        // Region output sections, each mapped to its own mem block. NOLOAD: DMA
        // buffers/descriptors carry no flash image (they are initialized at
        // runtime), like .bss.
        let mut region_sections = String::new();
        for r in &self.regions {
            let _ = write!(
                region_sections,
                "  .region.{name} (NOLOAD) :\n  {{\n    KEEP(*(.region.{name}*))\n  }} > {mem}\n\n",
                name = r.name,
                mem = r.mem
            );
        }

        let flash_name = &flash.name;
        let ram_name = &ram.name;
        format!(
            r"/* Auto-generated linker script for bml (regions) */
MEMORY
{{
{memory}}}

ENTRY(reset_handler)

SECTIONS
{{
  .vector_table {vt_offset} :
  {{
    KEEP(*(.vector_table))
  }} > {flash_name}

  .text :
  {{
    *(.text*)
    *(.rodata*)
  }} > {flash_name}

{region_sections}  .data :
  {{
    _sdata = .;
    *(.data*)
    _edata = .;
    _sidata = LOADADDR(.data);
  }} > {ram_name} AT > {flash_name}

  .bss :
  {{
    _sbss = .;
    *(.bss*)
    _ebss = .;
  }} > {ram_name}

  .stack (NOLOAD) :
  {{
    . = ALIGN(8);
    _stack_bottom = .;
    . = . + 0x800; /* 2KB stack */
    _stack_top = .;
  }} > {ram_name}

  /DISCARD/ :
  {{
    *(.ARM.exidx*)
    *(.ARM.attributes*)
  }}
}}
"
        )
    }
}

fn is_supported_arch(arch: &str) -> bool {
    matches!(arch, "armv6m" | "armv7m" | "armv7em")
}

/// Split a `key = value` line, trimming both sides. Shared by the subsectioned
/// `[mem.*]`/`[agent.*]`/`[region.*]` parsers.
fn split_kv(line: &str, line_num: usize) -> Result<(&str, &str), String> {
    let (key, val) = line.split_once('=').ok_or_else(|| {
        format!(
            "line {}: expected `key = value`, got `{line}`",
            line_num + 1
        )
    })?;
    Ok((key.trim(), val.trim()))
}

/// Parse a comma-separated list, trimming and dropping empties.
fn parse_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse one `key = value` line inside an `[agent.*]` section into `agent`.
fn parse_agent_kv(agent: &mut Agent, key: &str, val: &str, line_num: usize) -> Result<(), String> {
    match key {
        "kind" => {
            agent.kind = match val {
                "cpu" => AgentKind::Cpu,
                "dma" => AgentKind::Dma,
                "debug" => AgentKind::Debug,
                "external" => AgentKind::External,
                _ => {
                    return Err(format!(
                        "line {}: unknown agent kind `{val}` (expected cpu, dma, debug, external)",
                        line_num + 1
                    ));
                }
            };
        }
        "reach" => {
            if val.trim() == "*" {
                agent.reach_all = true;
                agent.reach.clear();
            } else {
                agent.reach = parse_list(val);
            }
        }
        "cached" => agent.cached = parse_bool(val, "cached", line_num)?,
        "access" => {
            agent.access = match val {
                "read" => AgentAccess::Read,
                "readwrite" | "rw" => AgentAccess::ReadWrite,
                _ => {
                    return Err(format!(
                        "line {}: unknown agent access `{val}` (expected read or readwrite)",
                        line_num + 1
                    ));
                }
            };
        }
        "handoff" => agent.handoffs.push(parse_handoff(val, line_num)?),
        "enabled_by" => agent.enabled_by = parse_list(val),
        _ => {
            return Err(format!("line {}: unknown agent key `{key}`", line_num + 1));
        }
    }
    Ok(())
}

/// Parse a handoff spec: `Peripheral.REG.FIELD : <encoding> [align N]`.
fn parse_handoff(val: &str, line_num: usize) -> Result<Handoff, String> {
    let (reg, spec) = val.split_once(':').ok_or_else(|| {
        format!(
            "line {}: handoff needs `register : encoding`, got `{val}`",
            line_num + 1
        )
    })?;
    let register = reg.trim().to_string();
    if register.is_empty() {
        return Err(format!("line {}: handoff has empty register", line_num + 1));
    }
    let mut tokens = spec.split_whitespace();
    let encoding = match tokens.next() {
        Some("byte_addr") => HandoffEncoding::ByteAddr,
        Some("word_addr") => HandoffEncoding::WordAddr,
        other => {
            return Err(format!(
                "line {}: handoff `{register}` has unknown encoding `{}` (expected byte_addr or word_addr)",
                line_num + 1,
                other.unwrap_or("")
            ));
        }
    };
    let mut align = None;
    while let Some(tok) = tokens.next() {
        match tok {
            "align" => {
                let n = tokens.next().ok_or_else(|| {
                    format!(
                        "line {}: handoff `{register}` align needs a value",
                        line_num + 1
                    )
                })?;
                let n: u32 = n.parse().map_err(|_| {
                    format!(
                        "line {}: handoff `{register}` invalid align `{n}`",
                        line_num + 1
                    )
                })?;
                align = Some(n);
            }
            _ => {
                return Err(format!(
                    "line {}: handoff `{register}` unexpected token `{tok}`",
                    line_num + 1
                ));
            }
        }
    }
    Ok(Handoff {
        register,
        encoding,
        align,
    })
}

fn parse_bool(val: &str, key: &str, line: usize) -> Result<bool, String> {
    match val {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!(
            "line {}: {key} must be `true` or `false`, got `{val}`",
            line + 1
        )),
    }
}

fn parse_int(val: &str) -> Result<u64, std::num::ParseIntError> {
    if val.starts_with("0x") || val.starts_with("0X") {
        u64::from_str_radix(&val[2..], 16)
    } else if val.ends_with('K') || val.ends_with('k') {
        let num: u64 = val[..val.len() - 1].parse()?;
        Ok(num * 1024)
    } else if val.ends_with('M') || val.ends_with('m') {
        let num: u64 = val[..val.len() - 1].parse()?;
        Ok(num * 1024 * 1024)
    } else {
        val.parse()
    }
}

fn format_size(n: u64) -> String {
    if n >= 1024 * 1024 && n.is_multiple_of(1024 * 1024) {
        format!("{}M", n / (1024 * 1024))
    } else if n >= 1024 && n.is_multiple_of(1024) {
        format!("{}K", n / 1024)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Target {
        Target::parse(s).unwrap()
    }

    #[test]
    fn parses_cpu_field() {
        let target = t("arch = armv7em\ncpu = cortex-m4\n");
        assert_eq!(target.cpu.as_deref(), Some("cortex-m4"));
    }

    #[test]
    fn parse_errors_on_unknown_arch() {
        let err = Target::parse("arch = armv9m\n").unwrap_err();
        assert!(err.contains("armv9m"), "got: {err}");
    }

    #[test]
    fn cflags_m3() {
        let target = t("arch = armv7m\ncpu = cortex-m3\n");
        assert_eq!(
            target.to_gcc_flags().unwrap(),
            vec!["-mcpu=cortex-m3", "-mthumb", "-mfloat-abi=soft"]
        );
    }

    #[test]
    fn cflags_m4_hard_fpu() {
        let target = t("arch = armv7em\ncpu = cortex-m4\nhas_fpu = true\n");
        assert_eq!(
            target.to_gcc_flags().unwrap(),
            vec![
                "-mcpu=cortex-m4",
                "-mthumb",
                "-mfloat-abi=hard",
                "-mfpu=fpv4-sp-d16",
            ]
        );
    }

    #[test]
    fn cflags_m4_no_fpu() {
        let target = t("arch = armv7em\ncpu = cortex-m4\nhas_fpu = false\n");
        assert_eq!(
            target.to_gcc_flags().unwrap(),
            vec!["-mcpu=cortex-m4", "-mthumb", "-mfloat-abi=soft"]
        );
    }

    #[test]
    fn cflags_m7_hard_fpu() {
        let target = t("arch = armv7em\ncpu = cortex-m7\nhas_fpu = true\n");
        assert_eq!(
            target.to_gcc_flags().unwrap(),
            vec![
                "-mcpu=cortex-m7",
                "-mthumb",
                "-mfloat-abi=hard",
                "-mfpu=fpv5-d16",
            ]
        );
    }

    #[test]
    fn cflags_errors_when_cpu_missing() {
        let target = t("arch = armv7em\n");
        let err = target.to_gcc_flags().unwrap_err();
        assert!(err.contains("cpu"), "got: {err}");
    }

    #[test]
    fn parses_startup_section() {
        let target = t("arch = armv7em\n[startup]\n0x5802453C = 0x60000000\n");
        assert_eq!(target.startup_init, vec![(0x5802_453C, 0x6000_0000)]);
    }

    #[test]
    fn startup_rejects_out_of_range() {
        let err = Target::parse("arch = armv7em\n[startup]\n0x100000000 = 0x1\n").unwrap_err();
        assert!(err.contains("32 bits"), "got: {err}");
    }

    #[test]
    fn cflags_errors_on_unknown_cpu() {
        let target = t("arch = armv7em\ncpu = cortex-x99\n");
        let err = target.to_gcc_flags().unwrap_err();
        assert!(err.contains("cortex-x99"), "got: {err}");
    }

    // ---- regions / agents (slice 0) ----

    /// A trimmed-down H723 layout: two D2 SRAMs, ETH DMA reaching them but not
    /// DTCM, and a region shared by the cpu and `eth_dma`.
    const H723_REGIONS: &str = "\
arch = armv7em
cpu = cortex-m7
flash_base = 0x08000000
ram_base = 0x20000000

[mem.flash]
base = 0x08000000
size = 1M

[mem.dtcm]
base = 0x20000000
size = 128K

[mem.sram1]
base = 0x30000000
size = 16K

[mem.sram2]
base = 0x30004000
size = 16K

[agent.eth_dma]
kind = dma
reach = sram1, sram2
cached = false
handoff = Ethernet_DMA.DMACTxDLAR.TDESLA : word_addr align 4
handoff = Ethernet_DMA.DMACTxDTPR.TDT : word_addr
enabled_by = RCC.C1_AHB1ENR.ETH1MACEN, RCC.C1_AHB1ENR.ETH1TXEN

[region.dma_shared]
mem = sram1
agents = eth_dma
";

    #[test]
    fn parses_mem_agent_region() {
        let target = t(H723_REGIONS);

        assert_eq!(target.mem_blocks.len(), 4); // flash, dtcm, sram1, sram2
        let sram1 = target
            .mem_blocks
            .iter()
            .find(|m| m.name == "sram1")
            .unwrap();
        assert_eq!(sram1.base, 0x3000_0000);
        assert_eq!(sram1.size, 16 * 1024);
        assert_eq!(sram1.end(), 0x3000_4000);

        assert_eq!(target.agents.len(), 1);
        let eth = &target.agents[0];
        assert_eq!(eth.name, "eth_dma");
        assert_eq!(eth.kind, AgentKind::Dma);
        assert!(eth.reaches("sram1") && eth.reaches("sram2"));
        assert!(!eth.reaches("dtcm"));
        assert!(!eth.cached);
        assert_eq!(eth.access, AgentAccess::ReadWrite);
        assert_eq!(eth.enabled_by.len(), 2);

        assert_eq!(eth.handoffs.len(), 2);
        assert_eq!(eth.handoffs[0].register, "Ethernet_DMA.DMACTxDLAR.TDESLA");
        assert_eq!(eth.handoffs[0].encoding, HandoffEncoding::WordAddr);
        assert_eq!(eth.handoffs[0].align, Some(4));
        assert_eq!(eth.handoffs[1].align, None);

        assert_eq!(target.regions.len(), 1);
        assert_eq!(target.regions[0].mem, "sram1");
        assert_eq!(target.regions[0].agents, vec!["eth_dma".to_string()]);
    }

    #[test]
    fn region_unreachable_mem_is_error() {
        // The DTCM footgun: ETH DMA cannot reach DTCM, so a region placing
        // shared buffers there must be rejected at target load.
        let src = H723_REGIONS.replace("mem = sram1", "mem = dtcm");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("cannot reach"), "got: {err}");
        assert!(err.contains("dtcm"), "got: {err}");
    }

    #[test]
    fn region_unknown_mem_is_error() {
        let src = H723_REGIONS.replace("mem = sram1", "mem = nope");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("unknown mem"), "got: {err}");
    }

    // A cached CPU and the (default) cacheable sram1, shared with the
    // non-snooping eth_dma -- this is the appended cpu agent below.
    const H723_CACHED_CPU: &str = "\n[agent.cpu]\nkind = cpu\ncached = true\nreach = *\n";

    #[test]
    fn cacheable_region_shared_with_noncoherent_agent_is_error() {
        // Failure mode #3: enabling the D-cache (cached cpu) while a non-snooping
        // DMA shares a cacheable region makes their views diverge. Rejected at
        // target load.
        let src = format!("{H723_REGIONS}{H723_CACHED_CPU}");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("cache views diverge"), "got: {err}");
        assert!(err.contains("dma_shared"), "got: {err}");
    }

    #[test]
    fn noncacheable_region_with_noncoherent_agent_is_ok() {
        // Same hazard, resolved by declaring the region's mem non-cacheable.
        let src = format!("{H723_REGIONS}{H723_CACHED_CPU}").replace(
            "[mem.sram1]\nbase = 0x30000000\nsize = 16K",
            "[mem.sram1]\nbase = 0x30000000\nsize = 16K\ncacheable = false",
        );
        Target::parse(&src).expect("a non-cacheable shared region must be accepted");
    }

    #[test]
    fn cacheable_region_with_dcache_off_is_ok() {
        // The current bring-up: D-cache never enabled (cpu cached=false), so the
        // CPU also sees physical memory -- no divergence even though cacheable.
        let src =
            format!("{H723_REGIONS}{H723_CACHED_CPU}").replace("cached = true", "cached = false");
        Target::parse(&src).expect("a non-cached cpu must not trip the cache check");
    }

    #[test]
    fn region_alignment_derived_from_cache_line() {
        // dma_shared is cacheable (default) and shared with the non-coherent
        // eth_dma; on a cortex-m7 (32-byte line) the derived alignment is 32.
        let aligns = t(H723_REGIONS).region_alignments();
        assert_eq!(aligns.get("dma_shared"), Some(&32), "got: {aligns:?}");
    }

    #[test]
    fn region_alignment_none_when_noncacheable() {
        // Non-cacheable memory needs no line alignment -> no derived floor.
        let src = H723_REGIONS.replace(
            "[mem.sram1]\nbase = 0x30000000\nsize = 16K",
            "[mem.sram1]\nbase = 0x30000000\nsize = 16K\ncacheable = false",
        );
        assert!(!t(&src).region_alignments().contains_key("dma_shared"));
    }

    #[test]
    fn region_alignment_none_on_cacheless_core() {
        // A cacheless core (cortex-m4) has no cache line -> no derived floor.
        let src = H723_REGIONS.replace("cpu = cortex-m7", "cpu = cortex-m4");
        assert!(!t(&src).region_alignments().contains_key("dma_shared"));
    }

    #[test]
    fn region_unknown_agent_is_error() {
        let src = H723_REGIONS.replace("agents = eth_dma", "agents = ghost");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[test]
    fn agent_reach_unknown_mem_is_error() {
        let src = H723_REGIONS.replace("reach = sram1, sram2", "reach = sram1, sram9");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("unknown mem"), "got: {err}");
    }

    #[test]
    fn overlapping_mem_is_error() {
        let src = "\
arch = armv7em
[mem.a]
base = 0x30000000
size = 32K
[mem.b]
base = 0x30004000
size = 16K
";
        let err = Target::parse(src).unwrap_err();
        assert!(err.contains("overlaps"), "got: {err}");
    }

    #[test]
    fn reach_star_reaches_everything() {
        let src = "\
arch = armv7em
flash_base = 0x08000000
ram_base = 0x30000000
[mem.flash]
base = 0x08000000
size = 64K
[mem.sram1]
base = 0x30000000
size = 16K
[agent.probe]
kind = debug
reach = *
[region.r]
mem = sram1
agents = probe
";
        let target = t(src);
        assert!(target.agents[0].reach_all);
        assert!(target.agents[0].reaches("sram1"));
    }

    #[test]
    fn handoff_non_pow2_align_is_error() {
        let src = "\
arch = armv7em
[mem.sram1]
base = 0x30000000
size = 16K
[agent.d]
kind = dma
reach = sram1
handoff = P.R.F : word_addr align 3
";
        let err = Target::parse(src).unwrap_err();
        assert!(err.contains("power of two"), "got: {err}");
    }

    #[test]
    fn unknown_section_is_error() {
        let err = Target::parse("arch = armv7em\n[bogus]\nx = 1\n").unwrap_err();
        assert!(err.contains("unknown section"), "got: {err}");
    }

    #[test]
    fn region_linker_script_places_section_in_mem_block() {
        // sram1 (0x30000000) holds a region; dtcm (0x20000000) is the working
        // RAM. The region section must map to sram1, and .data/.bss to dtcm.
        let target = t(H723_REGIONS);
        let ld = target.generate_linker_script();

        // Every mem block gets a MEMORY entry at its own origin.
        assert!(ld.contains("flash (rx) : ORIGIN = 0x08000000"), "ld:\n{ld}");
        assert!(ld.contains("dtcm (rwx) : ORIGIN = 0x20000000"), "ld:\n{ld}");
        assert!(
            ld.contains("sram1 (rwx) : ORIGIN = 0x30000000"),
            "ld:\n{ld}"
        );
        // The region section is mapped to its mem block.
        assert!(
            ld.contains(".region.dma_shared") && ld.contains("} > sram1"),
            "ld:\n{ld}"
        );
        // Working sections land in the ram block (dtcm), loaded from flash.
        assert!(ld.contains("} > dtcm AT > flash"), "ld:\n{ld}");
    }

    #[test]
    fn flat_linker_script_unchanged_without_regions() {
        // A target with no [region.*] keeps the legacy FLASH/RAM script.
        let target = t("arch = armv7em\nram_base = 0x20000000\nram_size = 20K\n");
        let ld = target.generate_linker_script();
        assert!(
            ld.contains("FLASH (rx)") && ld.contains("RAM   (rwx)"),
            "ld:\n{ld}"
        );
        assert!(!ld.contains(".region."), "ld:\n{ld}");
    }

    #[test]
    fn regions_without_flash_block_is_error() {
        // sram1 backs the region but nothing covers flash_base -> rejected.
        let src = "\
arch = armv7em
ram_base = 0x30000000
[mem.sram1]
base = 0x30000000
size = 16K
[agent.d]
kind = dma
reach = sram1
[region.r]
mem = sram1
agents = d
";
        let err = Target::parse(src).unwrap_err();
        assert!(err.contains("flash_base"), "got: {err}");
    }

    #[test]
    fn legacy_flat_keys_still_parse() {
        // Slice 0 is additive: target files with no [mem.*]/[agent.*] sections
        // keep working on the flat flash_*/ram_* keys.
        let target = t("arch = armv7em\nram_base = 0x30000000\nram_size = 32K\n");
        assert!(target.mem_blocks.is_empty());
        assert!(target.agents.is_empty());
        assert!(target.regions.is_empty());
        assert_eq!(target.ram_base, 0x3000_0000);
    }
}
