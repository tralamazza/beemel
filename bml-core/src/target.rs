use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Target {
    pub arch: String,
    /// Cortex-M cpu (`cortex-m0`, `cortex-m0plus`, `cortex-m3`, `cortex-m4`, `cortex-m7`).
    /// Optional in the target file; needed by `bml cflags` to disambiguate within an arch
    /// (e.g. armv7em covers both M4 and M7).
    pub cpu: Option<String>,
    pub priority_bits: u8,
    pub has_fpu: bool,
    pub has_bitband: bool,
    /// Whether `has_bitband` was written explicitly (vs the default): the
    /// ARMv6-M ignore-warning only makes sense for an explicit `true`.
    has_bitband_set: bool,
    pub has_mpu: bool,
    pub flash_base: u64,
    pub flash_size: u64,
    pub ram_base: u64,
    pub ram_size: u64,
    pub vector_table_offset: u64,
    /// Name of the mem block that holds `.data`/`.bss`/`.stack` (the working
    /// RAM) for mem-block targets. The code/flash block is inferred from
    /// `vector_table_offset`, so with mem blocks the flat `flash_*`/`ram_*` keys
    /// are unneeded. `None` falls back to the block containing `ram_base`.
    pub data_block: Option<String>,
    /// Hardware spinlock physics (`spinlock_base` / `spinlock_count`): a
    /// bank of read-to-claim / write-to-release mutex registers (e.g. the
    /// RP2350 SIO spinlocks at 0xD0000100 x32). Required for cross-core
    /// `claim` windows; absent = cross-core sharing stays rejected (E615).
    pub spinlock_base: Option<u64>,
    pub spinlock_count: u32,
    pub interrupts: HashMap<String, u16>,
    /// MMIO read-modify-write OR writes applied at the very start of
    /// `reset_handler`, before `.data`/`.bss` init -- the equivalent of CMSIS
    /// `SystemInit` running before the RAM copy. Each entry is `(address,
    /// or_mask)` meaning `*(volatile u32*)address |= or_mask`. The canonical use
    /// is enabling a RAM clock (e.g. STM32H7 D2 SRAM) when the stack lives in a
    /// clock-gated SRAM, since `reset_handler` touches RAM before `main` runs.
    pub startup_init: Vec<(u64, u64)>,
    /// Literal words from `[boot_block]`, emitted verbatim by the linker
    /// script directly after the vector table. Chip-agnostic mechanism for
    /// boot metadata the boot ROM scans flash for; the canonical consumer is
    /// the RP2350 `IMAGE_DEF` block (datasheet 5.9.5). Empty = no section.
    pub boot_block: Vec<u32>,
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
/// silicon is called. See `doc/regions-agents.md`.
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

/// A handoff register: the place where a written address is handed to the agent,
/// which then dereferences it. `register` is an unresolved SVD path string
/// (`Peripheral.REGISTER`); resolution against the peripheral table happens
/// later, where the SVD is visible. The full byte address is written to the
/// register verbatim -- these are dedicated address registers whose reserved low
/// bits the hardware ignores, so the compiler applies no encoding or shift (see
/// doc/regions-agents.md).
#[derive(Debug, Clone)]
pub struct Handoff {
    pub register: String,
    /// Optional minimum byte alignment of the handed-off address (power of two).
    pub align: Option<u32>,
    /// Optional software port select (`port_by P.R.F TAG`): the field whose
    /// *set* state routes this handoff's address through the agent's bus
    /// windows tagged `TAG` (clear state = the untagged/other windows). Drives
    /// the port-select check (E612): an address in a TAG-only window requires
    /// the field set; an address outside every TAG window forbids setting it.
    pub port_by: Option<PortBy>,
}

/// See `Handoff::port_by`.
#[derive(Debug, Clone)]
pub struct PortBy {
    /// `Peripheral.REGISTER.FIELD` path, resolved against the program's SVD
    /// peripherals at check time (like `enabled_by`).
    pub field: String,
    /// The window tag the set state selects.
    pub tag: String,
}

/// See `Agent::extent_by`.
#[derive(Debug, Clone)]
pub struct ExtentBy {
    /// `Peripheral.REGISTER.FIELD` path of the transfer-count field.
    pub path: String,
    /// Compile-time byte multiplier: bytes the agent moves per count unit
    /// (1 = the field counts bytes, 4 = words, ...).
    pub scale: u32,
    /// `when P.R.F = V`: the unit-select field write that makes `scale` true
    /// physics (e.g. the RP2350's `CTRL_TRIG.DATA_SIZE = 2` for x4). When
    /// declared, arming the agent without setting the field to exactly V is
    /// rejected (E618) -- the multiplier stops being trusted policy.
    pub unit: Option<(String, u64)>,
}

/// A channel's transfer-extent declaration: how MUCH the agent moves.
#[derive(Debug, Clone)]
pub enum ExtentSpec {
    /// `extent = P.R.F [xN] [when ...]`: a count register the program arms;
    /// verify asserts each write fits the delivered buffer.
    Counter(ExtentBy),
    /// `extent = N`: a fixed block size in bytes (EasyDMA-style engines with
    /// no count register -- the nRF ECB walks exactly 48 bytes). The
    /// obligation moves to the DELIVERY: a buffer handed to this channel
    /// must be at least N bytes (E619, compile time for direct `&X`
    /// deliveries).
    Fixed(u64),
}

/// One transaction channel of an agent (`[agent.NAME.CHANNEL]`), grouping the
/// per-transaction vocabulary: which registers receive buffer addresses
/// (`handoff`), which signal marks the transfer done (`completes_by`), and
/// which field programs how much is moved (`extent`). Transaction keys
/// written directly in `[agent.NAME]` land in an implicit default channel,
/// so single-channel agents never need a channel section -- the grouping
/// exists for multi-channel controllers (8-channel DMACs, ETH TX vs RX).
#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub name: String,
    pub handoffs: Vec<Handoff>,
    pub completes_by: Vec<String>,
    pub extent: Option<ExtentSpec>,
}

/// A bus-matrix window: an address range `[start, end)` that an agent's bus
/// master port can physically address, transcribed from the vendor's
/// bus-master-to-bus-slave interconnect table (RM0468 Table 2 for the H72x).
///
/// `reach` is a *claim* (project intent); `bus` is a *transcription* (chip
/// physics from the manual). Declaring windows turns reach from trusted to
/// cross-checked: a reach over a block no window covers fails at target load.
/// Two independent sources must now both be wrong for a bad placement to
/// compile -- the MDMA/DTCM class of error.
#[derive(Debug, Clone)]
pub struct BusWindow {
    pub start: u64,
    pub end: u64,
    /// Optional port tag (`bus = axi: ..., ahbs: ...`): which of the agent's
    /// master ports addresses this window. A tag is sticky over the following
    /// untagged items in the list. `None` = port not modeled for this window.
    /// Consumed by the port-select check (E612) via `Handoff.port_by`.
    pub port: Option<String>,
}

impl BusWindow {
    /// Whether the window fully contains `[base, end)`.
    #[must_use]
    pub fn covers(&self, base: u64, end: u64) -> bool {
        self.start <= base && end <= self.end
    }
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
    /// Register paths that must be enabled for the agent to operate (clock gates).
    pub enabled_by: Vec<String>,
    /// Transaction channels (see `Channel`). Transaction keys written at the
    /// agent level go into an implicit default channel (empty name).
    pub channels: Vec<Channel>,
    /// Entry function for a `cpu`-kind agent (`entry = <fn>`): the function
    /// this core starts executing (launched by another core, e.g. the RP2350
    /// FIFO handshake). Project policy, not chip physics -- it binds CODE to
    /// the core, and core-reachability is derived from it (everything the
    /// entry transitively mentions runs on this core). Drives the cross-core
    /// sharing check (E615).
    pub entry: Option<String>,
    /// Bus-matrix windows (`bus = start..end, ...`): the address ranges this
    /// agent's master ports can physically address. Empty = no transcription
    /// available, `reach` stays trusted. Non-empty = every reached block must
    /// fit inside a window (validated at target load).
    ///
    /// Windows are the UNION over the agent's master ports; the reach check
    /// catches what NO port can address. When a port must be selected by
    /// software (the H7 MDMA reaches the TCMs only via its AHBS port, chosen
    /// by `MDMA_CxTBR.SBUS/DBUS`), tag the windows (`axi:`/`ahbs:`) and
    /// declare `port_by` on the handoff -- the port-select check (E612) then
    /// requires the bit to match where the handed-off address lives.
    pub bus: Vec<BusWindow>,
}

impl Agent {
    /// All handoff registers across every channel.
    pub fn handoffs(&self) -> impl Iterator<Item = &Handoff> {
        self.channels.iter().flat_map(|c| c.handoffs.iter())
    }

    /// All completion flags across every channel. Buffer-to-channel
    /// association is not modeled yet, so checks that need "this buffer's
    /// flags" take the union (recorded follow-up).
    pub fn completes_by(&self) -> impl Iterator<Item = &String> {
        self.channels.iter().flat_map(|c| c.completes_by.iter())
    }

    /// The implicit default channel (agent-level transaction keys),
    /// created on first use.
    fn default_channel_mut(&mut self) -> &mut Channel {
        if !self.channels.iter().any(|c| c.name.is_empty()) {
            self.channels.insert(0, Channel::default());
        }
        self.channels
            .iter_mut()
            .find(|c| c.name.is_empty())
            .expect("default channel just ensured")
    }

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
    /// `[agent.NAME.CHANNEL]` -- (agent index, channel index).
    AgentChannel(usize, usize),
    Top,
    Interrupts,
    Startup,
    BootBlock,
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
            has_bitband_set: false,
            has_mpu: true,
            flash_base: 0x0800_0000,
            flash_size: 256 * 1024,
            ram_base: 0x2000_0000,
            ram_size: 64 * 1024,
            vector_table_offset: 0x0800_0000,
            data_block: None,
            spinlock_base: None,
            spinlock_count: 0,
            interrupts: HashMap::new(),
            startup_init: Vec::new(),
            boot_block: Vec::new(),
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
        let mut target = Target::default();
        let mut loaded = std::collections::HashSet::new();
        target.load_file(path, &mut loaded)?;
        target.finalize()?;
        Ok(target)
    }

    /// Load `path` and the targets it `include`s onto `self`. Each `include` is
    /// resolved relative to the including file and applied *first*, so a project
    /// target that includes a base inherits its definitions and may then
    /// override or extend them (later wins). Each file is applied at most once
    /// (`loaded` dedups diamonds and terminates cycles). Validation happens once
    /// on the merged result, in `from_file`.
    fn load_file(
        &mut self,
        path: &std::path::Path,
        loaded: &mut std::collections::HashSet<std::path::PathBuf>,
    ) -> Result<(), String> {
        let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !loaded.insert(canon) {
            return Ok(());
        }
        let content = fs::read_to_string(path)
            .map_err(|e| format!("cannot read target {}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        for inc in scan_includes(&content) {
            self.load_file(&dir.join(inc), loaded)?;
        }
        self.apply(&content)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parse target configuration from a string.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is malformed or contains unknown keys.
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut target = Target::default();
        target.apply(input)?;
        target.finalize()?;
        Ok(target)
    }

    /// Apply one target file's directives on top of `self`. `parse` runs it on a
    /// default target; `from_file` runs it on top of any `include`d bases, so a
    /// later definition overrides or extends an earlier one (re-opening a named
    /// `[mem/agent/region.NAME]` resumes editing that entity -- single
    /// assignments overwrite, accumulator lines like `handoff` append).
    /// `include = ...` directives are resolved by `from_file` and skipped here.
    fn apply(&mut self, input: &str) -> Result<(), String> {
        let target = self;
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
                // Find-or-add by name: re-opening a section (e.g. from an
                // including file) resumes editing the existing entity rather
                // than creating a duplicate, which is what makes `include`
                // override/extend work.
                section = match header.split_once('.') {
                    Some(("mem", name)) => {
                        let idx = if let Some(i) =
                            target.mem_blocks.iter().position(|m| m.name == name)
                        {
                            i
                        } else {
                            target.mem_blocks.push(MemBlock {
                                name: name.to_string(),
                                base: 0,
                                size: 0,
                                cacheable: true,
                            });
                            target.mem_blocks.len() - 1
                        };
                        Section::Mem(idx)
                    }
                    Some(("agent", name)) => {
                        // `[agent.NAME.CHANNEL]`: a transaction channel of an
                        // agent (find-or-add both, like every other section).
                        let (name, channel) = match name.split_once('.') {
                            Some((a, c)) => (a, Some(c)),
                            None => (name, None),
                        };
                        let idx = if let Some(i) = target.agents.iter().position(|a| a.name == name)
                        {
                            i
                        } else {
                            target.agents.push(Agent {
                                name: name.to_string(),
                                kind: AgentKind::Dma,
                                reach: Vec::new(),
                                reach_all: false,
                                cached: false,
                                enabled_by: Vec::new(),
                                channels: Vec::new(),
                                entry: None,
                                bus: Vec::new(),
                            });
                            target.agents.len() - 1
                        };
                        match channel {
                            None => Section::Agent(idx),
                            Some(ch) => {
                                let agent = &mut target.agents[idx];
                                let ci = if let Some(i) =
                                    agent.channels.iter().position(|c| c.name == ch)
                                {
                                    i
                                } else {
                                    agent.channels.push(Channel {
                                        name: ch.to_string(),
                                        ..Channel::default()
                                    });
                                    agent.channels.len() - 1
                                };
                                Section::AgentChannel(idx, ci)
                            }
                        }
                    }
                    Some(("region", name)) => {
                        let idx =
                            if let Some(i) = target.regions.iter().position(|r| r.name == name) {
                                i
                            } else {
                                target.regions.push(Region {
                                    name: name.to_string(),
                                    mem: String::new(),
                                    agents: Vec::new(),
                                });
                                target.regions.len() - 1
                            };
                        Section::Region(idx)
                    }
                    _ => match header {
                        "interrupts" => Section::Interrupts,
                        "startup" => Section::Startup,
                        "boot_block" => Section::BootBlock,
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
            // In [agent.NAME.CHANNEL] section: transaction keys only
            if let Section::AgentChannel(aidx, cidx) = section {
                let (key, val) = split_kv(line, line_num)?;
                let ch = &mut target.agents[aidx].channels[cidx];
                parse_channel_kv(ch, key, val, line_num)?;
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
            // In [boot_block] section: one literal 32-bit word per line.
            if let Section::BootBlock = section {
                let word = parse_int(line).map_err(|_| {
                    format!("line {}: invalid boot_block word `{line}`", line_num + 1)
                })?;
                let word = u32::try_from(word).map_err(|_| {
                    format!("line {}: boot_block word must fit in 32 bits", line_num + 1)
                })?;
                target.boot_block.push(word);
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
                "has_bitband" => {
                    target.has_bitband = parse_bool(val, key, line_num)?;
                    target.has_bitband_set = true;
                }
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
                "spinlock_base" => {
                    target.spinlock_base = Some(parse_int(val).map_err(|_| {
                        format!("line {}: invalid spinlock_base: {val}", line_num + 1)
                    })?);
                }
                "spinlock_count" => {
                    target.spinlock_count = parse_int(val).map_err(|_| {
                        format!("line {}: invalid spinlock_count: {val}", line_num + 1)
                    })? as u32;
                }
                "vector_table_offset" => {
                    target.vector_table_offset = parse_int(val).map_err(|_| {
                        format!("line {}: invalid vector_table_offset: {val}", line_num + 1)
                    })?;
                }
                "data_block" => target.data_block = Some(val.to_string()),
                // Resolved by `from_file` (relative to the including file) before
                // this file's directives are applied; a no-op here.
                "include" => {}
                _ => return Err(format!("line {}: unknown key `{key}`", line_num + 1)),
            }
        }
        Ok(())
    }

    /// Final post-processing and self-consistency checks, run once after all
    /// `include`s are merged so the checks see the fully composed target.
    fn finalize(&mut self) -> Result<(), String> {
        // ARMv6-M (Cortex-M0/M0+) does not support bit-banding. The default
        // is silently corrected; only an EXPLICIT `has_bitband = true` warns.
        if self.has_bitband && self.arch == "armv6m" {
            if self.has_bitband_set {
                eprintln!(
                    "warning: ARMv6-M does not support bit-banding; ignoring `has_bitband = true`"
                );
            }
            self.has_bitband = false;
        }
        self.validate_regions()
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
            // A non-cacheable block becomes an MPU region (when the part has
            // an MPU). The shape rules follow the MPU flavor: PMSAv7 needs a
            // power-of-two size >= 32 with a size-aligned base; PMSAv8 is
            // [base, limit] at 32-byte granularity (no power-of-two rule).
            if self.has_mpu && !m.cacheable {
                match self.mpu_flavor() {
                    crate::arch::MpuFlavor::Pmsa7 => {
                        if !m.size.is_power_of_two() || m.size < 32 {
                            return Err(format!(
                                "mem `{}` is `cacheable = false` (an MPU region) but its size {} \
                                 is not a power of two >= 32 (PMSAv7)",
                                m.name, m.size
                            ));
                        }
                        if m.base % m.size != 0 {
                            return Err(format!(
                                "mem `{}` is `cacheable = false` (an MPU region) but its base \
                                 0x{:08X} is not aligned to its size {} (PMSAv7)",
                                m.name, m.base, m.size
                            ));
                        }
                    }
                    crate::arch::MpuFlavor::Pmsa8 => {
                        if m.size < 32 || m.size % 32 != 0 || m.base % 32 != 0 {
                            return Err(format!(
                                "mem `{}` is `cacheable = false` (an MPU region) but PMSAv8 \
                                 needs base and size 32-byte aligned (size >= 32); got base \
                                 0x{:08X} size {}",
                                m.name, m.base, m.size
                            ));
                        }
                    }
                }
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
            // `entry` binds code to a core: only meaningful for cpu agents.
            if agent.entry.is_some() && agent.kind != AgentKind::Cpu {
                return Err(format!(
                    "agent `{}` declares `entry`, but only `kind = cpu` agents execute code",
                    agent.name
                ));
            }
            // Bus-matrix cross-check: if the agent declares bus windows (a
            // transcription of the vendor interconnect table), every block it
            // claims to reach must fit inside one window. This turns `reach`
            // from a trusted claim into one cross-checked against an
            // independent source -- a wrong placement now needs both the claim
            // and the transcription to be wrong.
            if !agent.bus.is_empty() {
                let reached: Vec<&MemBlock> = if agent.reach_all {
                    self.mem_blocks.iter().collect()
                } else {
                    self.mem_blocks
                        .iter()
                        .filter(|m| agent.reach.iter().any(|r| r == &m.name))
                        .collect()
                };
                for m in reached {
                    if !agent.bus.iter().any(|w| w.covers(m.base, m.end())) {
                        return Err(format!(
                            "agent `{}` declares reach over `{}` (0x{:08X}..0x{:08X}), but none \
                             of its bus windows covers it -- the bus matrix says this master \
                             cannot address that memory",
                            agent.name,
                            m.name,
                            m.base,
                            m.end()
                        ));
                    }
                }
            }
            for h in agent.handoffs() {
                if let Some(n) = h.align
                    && !n.is_power_of_two()
                {
                    return Err(format!(
                        "agent `{}` handoff `{}` align {n} is not a power of two",
                        agent.name, h.register
                    ));
                }
                // A port_by tag must name a port that actually has windows;
                // otherwise the port-select check could never fire (or fires
                // on a typo'd tag, silently checking nothing).
                if let Some(pb) = &h.port_by
                    && !agent
                        .bus
                        .iter()
                        .any(|w| w.port.as_deref() == Some(pb.tag.as_str()))
                {
                    return Err(format!(
                        "agent `{}` handoff `{}` has `port_by {} {}`, but no bus window of the \
                         agent is tagged `{}:`",
                        agent.name, h.register, pb.field, pb.tag, pb.tag
                    ));
                }
            }
        }
        // The mem-block-driven linker script needs a code block (for .text) and
        // a working-RAM block (for .data/.bss/.stack). Required whenever mem
        // blocks are present, since that switches generation to the mem-block
        // layout.
        if !self.mem_blocks.is_empty() {
            if self.flash_block().is_none() {
                return Err(format!(
                    "mem blocks are defined but none holds the vector table \
                     (vector_table_offset 0x{:08X}); add a [mem.*] covering it",
                    self.vector_table_offset
                ));
            }
            if let Some(name) = &self.data_block
                && !self.mem_blocks.iter().any(|m| &m.name == name)
            {
                return Err(format!("data_block = `{name}` names no [mem.*]"));
            }
            if self.ram_block().is_none() {
                return Err("mem blocks are defined but no working-RAM block; set \
                     `data_block = <name>` (or point ram_base into a [mem.*])"
                    .to_string());
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
    /// statics; regions without it have no floor. See `doc/regions-agents.md`.
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

    /// Base + size of each mem block that must be made non-cacheable by the MPU
    /// (`cacheable = false`), so a CPU with caches on stays coherent with the DMA
    /// agents sharing it -- turning the trusted claim into enforced config. Empty
    /// when the part has no MPU. `validate_regions` guarantees each is
    /// MPU-encodable (power-of-two size >= 32, base aligned to size); the emitter
    /// Which MPU programming model the part implements -- decided by the CPU
    /// core (`cortex-m33` is ARMv8-M / `PMSAv8`), defaulting to `PMSAv7` for the
    /// v6/v7-M parts. Drives both the region-shape validation and the
    /// `reset_handler` emission.
    #[must_use]
    pub fn mpu_flavor(&self) -> crate::arch::MpuFlavor {
        match self.cpu.as_deref() {
            Some("cortex-m33") => crate::arch::MpuFlavor::Pmsa8,
            _ => crate::arch::MpuFlavor::Pmsa7,
        }
    }

    /// programs one MPU region per entry at the start of `reset_handler`.
    #[must_use]
    pub fn mpu_regions(&self) -> Vec<(u64, u64)> {
        if !self.has_mpu {
            return Vec::new();
        }
        self.mem_blocks
            .iter()
            .filter(|m| !m.cacheable)
            .map(|m| (m.base, m.size))
            .collect()
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
            // ARMv8-M Mainline; executes the v7e-m code we emit. Single-
            // precision FPU (RP2350).
            "cortex-m33" => (true, "fpv5-sp-d16"),
            other => {
                return Err(format!(
                    "unrecognized cpu `{other}` (expected cortex-m0, cortex-m0plus, cortex-m3, cortex-m4, cortex-m7, or cortex-m33)"
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

    /// The code/flash block: the mem block holding the vector table (`.text`/
    /// `.rodata` link here, and it is the load region for `.data`). Inferred
    /// from `vector_table_offset`, which is in flash by definition; falls back to
    /// the block containing `flash_base` for targets predating the inference.
    #[must_use]
    pub fn flash_block(&self) -> Option<&MemBlock> {
        let contains = |addr: u64| {
            self.mem_blocks
                .iter()
                .find(move |m| addr >= m.base && addr < m.end())
        };
        contains(self.vector_table_offset).or_else(|| contains(self.flash_base))
    }

    /// The working-RAM block (holds `.data`/`.bss`/`.stack`): the block named by
    /// `data_block`, else the block containing `ram_base` (back-compat).
    #[must_use]
    pub fn ram_block(&self) -> Option<&MemBlock> {
        if let Some(name) = &self.data_block {
            return self.mem_blocks.iter().find(|m| &m.name == name);
        }
        self.mem_blocks
            .iter()
            .find(|m| self.ram_base >= m.base && self.ram_base < m.end())
    }

    #[must_use]
    pub fn generate_linker_script(&self) -> String {
        if self.mem_blocks.is_empty() {
            self.generate_flat_linker_script()
        } else {
            self.generate_region_linker_script()
        }
    }

    /// The legacy single-FLASH/single-RAM layout, used when no `[mem.*]` blocks
    /// are present. Driven by the flat `flash_*`/`ram_*` keys.
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

        // Boot metadata words ([boot_block]), emitted verbatim directly after
        // the vector table -- inside the boot ROM's flash scan window (the
        // RP2350 IMAGE_DEF must start within the first 4 kB).
        let mut boot_block = String::new();
        if !self.boot_block.is_empty() {
            boot_block.push_str("  .boot_block :\n  {\n");
            for w in &self.boot_block {
                let _ = writeln!(boot_block, "    LONG(0x{w:08X})");
            }
            let _ = writeln!(boot_block, "  }} > {}\n", flash.name);
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

{boot_block}  .text :
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

/// Collect the values of top-level `include = <path>` directives -- those before
/// the first `[section]` header (top-level keys must precede any section). Paths
/// are resolved relative to the including file by `load_file`.
fn scan_includes(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if line.starts_with('[') {
            break;
        }
        if let Some((key, val)) = line.split_once('=')
            && key.trim() == "include"
        {
            out.push(val.trim().to_string());
        }
    }
    out
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
        // Transaction keys at the agent level land in the implicit default
        // channel -- single-channel agents never need a channel section.
        "handoff" | "completes_by" | "extent" => {
            parse_channel_kv(agent.default_channel_mut(), key, val, line_num)?;
        }
        "enabled_by" => agent.enabled_by = parse_list(val),
        "bus" => agent.bus = parse_bus_windows(val, line_num)?,
        "entry" => agent.entry = Some(val.to_string()),
        _ => {
            return Err(format!("line {}: unknown agent key `{key}`", line_num + 1));
        }
    }
    Ok(())
}

/// Parse one `key = value` line of a transaction channel (a
/// `[agent.NAME.CHANNEL]` section, or a transaction key written at the agent
/// level for the implicit default channel).
fn parse_channel_kv(ch: &mut Channel, key: &str, val: &str, line_num: usize) -> Result<(), String> {
    match key {
        "handoff" => ch.handoffs.push(parse_handoff(val, line_num)?),
        "completes_by" => ch.completes_by = parse_list(val),
        "extent" => {
            // A bare integer is the fixed-block form; anything else is the
            // count-register form.
            ch.extent = Some(if let Ok(n) = parse_int(val.trim()) {
                if n == 0 {
                    return Err(format!("line {}: fixed extent must be > 0", line_num + 1));
                }
                ExtentSpec::Fixed(n)
            } else {
                ExtentSpec::Counter(parse_extent(val, line_num)?)
            });
        }
        _ => {
            return Err(format!(
                "line {}: unknown channel key `{key}` (channels take handoff, completes_by,                  extent; reach/bus/enabled_by belong on the agent)",
                line_num + 1
            ));
        }
    }
    Ok(())
}

/// Parse an extent spec: `Peripheral.REGISTER.FIELD [xN] [when P.R.F = V]` --
/// the count field, an optional compile-time byte multiplier (default 1 =
/// the field counts bytes), and the optional unit-select condition that
/// makes the multiplier checked physics (E618).
fn parse_extent(val: &str, line_num: usize) -> Result<ExtentBy, String> {
    let mut parts = val.split_whitespace();
    let path = parts
        .next()
        .ok_or_else(|| format!("line {}: empty extent", line_num + 1))?
        .to_string();
    if path.split('.').count() != 3 {
        return Err(format!(
            "line {}: extent expects `Peripheral.REGISTER.FIELD`, got `{path}`",
            line_num + 1
        ));
    }
    let mut scale = 1u32;
    let mut pending = parts.next();
    if let Some(tok) = pending
        && tok != "when"
    {
        let n = tok.strip_prefix('x').and_then(|n| n.parse::<u32>().ok());
        match n {
            Some(n) if n > 0 => scale = n,
            _ => {
                return Err(format!(
                    "line {}: extent multiplier must be `xN` (N > 0), got `{tok}`",
                    line_num + 1
                ));
            }
        }
        pending = parts.next();
    }
    let mut unit = None;
    if let Some(tok) = pending {
        if tok != "when" {
            return Err(format!(
                "line {}: expected `when P.R.F = V` after the extent multiplier, got `{tok}`",
                line_num + 1
            ));
        }
        let upath = parts
            .next()
            .ok_or_else(|| format!("line {}: extent `when` needs a field path", line_num + 1))?;
        if upath.split('.').count() != 3 {
            return Err(format!(
                "line {}: extent `when` expects `Peripheral.REGISTER.FIELD`, got `{upath}`",
                line_num + 1
            ));
        }
        let (Some("="), Some(v)) = (parts.next(), parts.next()) else {
            return Err(format!(
                "line {}: extent `when` expects `= <value>` after the field path",
                line_num + 1
            ));
        };
        let value = parse_int(v)
            .map_err(|e| format!("line {}: bad extent unit value `{v}`: {e}", line_num + 1))?;
        unit = Some((upath.to_string(), value));
    }
    if let Some(extra) = parts.next() {
        return Err(format!(
            "line {}: unexpected token `{extra}` after extent",
            line_num + 1
        ));
    }
    Ok(ExtentBy { path, scale, unit })
}

/// Parse a handoff spec: `Peripheral.REGISTER [align N] [port_by P.R.F TAG]`.
/// The full byte address is written to the register verbatim; `align` is its
/// optional minimum alignment; `port_by` names the software port-select field
/// and the window tag its set state routes through.
fn parse_handoff(val: &str, line_num: usize) -> Result<Handoff, String> {
    let mut tokens = val.split_whitespace();
    let register = tokens
        .next()
        .ok_or_else(|| format!("line {}: handoff has empty register", line_num + 1))?
        .to_string();
    let mut align = None;
    let mut port_by = None;
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
            "port_by" => {
                let field = tokens.next().ok_or_else(|| {
                    format!(
                        "line {}: handoff `{register}` port_by needs `P.R.F TAG`",
                        line_num + 1
                    )
                })?;
                let tag = tokens.next().ok_or_else(|| {
                    format!(
                        "line {}: handoff `{register}` port_by `{field}` needs a window tag",
                        line_num + 1
                    )
                })?;
                port_by = Some(PortBy {
                    field: field.to_string(),
                    tag: tag.to_string(),
                });
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
        align,
        port_by,
    })
}

/// Parse `bus = [tag:] start..end, start..end, ...` into half-open windows. A
/// `tag:` prefix names the master port the following windows belong to and is
/// sticky until the next tag. Restating the key replaces the list
/// (include/override semantics, like `reach`).
fn parse_bus_windows(val: &str, line_num: usize) -> Result<Vec<BusWindow>, String> {
    let mut out = Vec::new();
    let mut port: Option<String> = None;
    for item in val.split(',') {
        let mut item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Some((tag, rest)) = item.split_once(':') {
            let tag = tag.trim();
            if tag.is_empty() {
                return Err(format!(
                    "line {}: bus window `{item}` has an empty port tag",
                    line_num + 1
                ));
            }
            port = Some(tag.to_string());
            item = rest.trim();
        }
        let (s, e) = item.split_once("..").ok_or_else(|| {
            format!(
                "line {}: bus window `{item}` must be `start..end`",
                line_num + 1
            )
        })?;
        let start = parse_int(s.trim())
            .map_err(|_| format!("line {}: bad bus window start `{s}`", line_num + 1))?;
        let end = parse_int(e.trim())
            .map_err(|_| format!("line {}: bad bus window end `{e}`", line_num + 1))?;
        if start >= end {
            return Err(format!(
                "line {}: bus window `{item}` is empty (start >= end)",
                line_num + 1
            ));
        }
        out.push(BusWindow {
            start,
            end,
            port: port.clone(),
        });
    }
    Ok(out)
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
handoff = Ethernet_DMA.DMACTxDLAR align 4
handoff = Ethernet_DMA.DMACTxDTPR
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
        assert_eq!(eth.enabled_by.len(), 2);

        let handoffs: Vec<_> = eth.handoffs().collect();
        assert_eq!(handoffs.len(), 2);
        assert_eq!(handoffs[0].register, "Ethernet_DMA.DMACTxDLAR");
        assert_eq!(handoffs[0].align, Some(4));
        assert_eq!(handoffs[1].align, None);

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

    // D2 SRAM1+SRAM2 window, as the bus matrix gives the H723 ETH MAC. Reopens
    // the agent section (find-or-add), exercising the include/merge path too.
    const ETH_BUS_D2: &str = "\n[agent.eth_dma]\nbus = 0x30000000..0x30008000\n";

    #[test]
    fn bus_windows_parse() {
        let src = H723_REGIONS.replace(
            "kind = dma",
            "kind = dma\nbus = 0x08000000..0x08100000, 0x30000000..0x30008000",
        );
        let eth = &t(&src).agents[0];
        assert_eq!(eth.bus.len(), 2);
        assert_eq!(eth.bus[0].start, 0x0800_0000);
        assert_eq!(eth.bus[0].end, 0x0810_0000);
        assert!(eth.bus[1].covers(0x3000_4000, 0x3000_8000));
        assert!(!eth.bus[1].covers(0x3000_7000, 0x3000_9000)); // straddles the edge
    }

    #[test]
    fn reach_outside_bus_windows_is_error() {
        // The bus matrix says the ETH MAC cannot address the TCMs. With windows
        // declared, a reach claim over dtcm dies at target load instead of as a
        // runtime bus error.
        let src = format!("{H723_REGIONS}{ETH_BUS_D2}")
            .replace("reach = sram1, sram2", "reach = sram1, sram2, dtcm");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("bus window"), "got: {err}");
        assert!(err.contains("dtcm"), "got: {err}");
    }

    #[test]
    fn reach_inside_bus_windows_is_ok() {
        let src = format!("{H723_REGIONS}{ETH_BUS_D2}");
        t(&src); // reach = sram1, sram2 -- both inside the D2 window
    }

    #[test]
    fn reach_all_is_checked_against_bus_windows() {
        // `reach = *` with windows means every mem block must be coverable;
        // flash and dtcm are outside the D2 window.
        let src =
            format!("{H723_REGIONS}{ETH_BUS_D2}").replace("reach = sram1, sram2", "reach = *");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("bus window"), "got: {err}");
    }

    #[test]
    fn bus_window_syntax_errors() {
        let src = H723_REGIONS.replace("kind = dma", "kind = dma\nbus = 0x30000000");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("start..end"), "got: {err}");

        let src = H723_REGIONS.replace("kind = dma", "kind = dma\nbus = 0x10..0x10");
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn bus_window_port_tags_parse_and_stick() {
        let src = H723_REGIONS.replace(
            "kind = dma",
            "kind = dma\nbus = axi: 0x08000000..0x08100000, 0x30000000..0x30008000, \
             ahbs: 0x20000000..0x20020000",
        );
        let eth = &t(&src).agents[0];
        assert_eq!(eth.bus.len(), 3);
        assert_eq!(eth.bus[0].port.as_deref(), Some("axi"));
        assert_eq!(eth.bus[1].port.as_deref(), Some("axi")); // sticky
        assert_eq!(eth.bus[2].port.as_deref(), Some("ahbs"));
    }

    #[test]
    fn handoff_port_by_parses() {
        let src = H723_REGIONS.replace(
            "handoff = Ethernet_DMA.DMACTxDLAR align 4",
            "handoff = Ethernet_DMA.DMACTxDLAR align 4 port_by MDMA.MDMA_C0TBR.DBUS ahbs\n\
             bus = axi: 0x30000000..0x30008000, ahbs: 0x20000000..0x20020000",
        );
        let tgt = t(&src);
        let h = tgt.agents[0].handoffs().next().unwrap();
        assert_eq!(h.align, Some(4));
        let pb = h.port_by.as_ref().unwrap();
        assert_eq!(pb.field, "MDMA.MDMA_C0TBR.DBUS");
        assert_eq!(pb.tag, "ahbs");
    }

    #[test]
    fn port_by_without_matching_tagged_window_is_error() {
        // port_by names tag `ahbs` but the agent has only untagged windows --
        // the check could never fire, so the target must not load.
        let src = format!("{H723_REGIONS}{ETH_BUS_D2}").replace(
            "handoff = Ethernet_DMA.DMACTxDLAR align 4",
            "handoff = Ethernet_DMA.DMACTxDLAR align 4 port_by MDMA.MDMA_C0TBR.DBUS ahbs",
        );
        let err = Target::parse(&src).unwrap_err();
        assert!(err.contains("no bus window"), "got: {err}");
        assert!(err.contains("ahbs"), "got: {err}");
    }

    #[test]
    fn bus_windows_override_last_wins() {
        // Include/override semantics: a later [agent.*] section restating `bus`
        // replaces the list, so a project file can widen (or fix) the physics.
        let narrow = format!("{H723_REGIONS}{ETH_BUS_D2}")
            .replace("reach = sram1, sram2", "reach = sram1, sram2, dtcm");
        Target::parse(&narrow).unwrap_err();
        let widened = format!(
            "{narrow}\n[agent.eth_dma]\nbus = 0x20000000..0x20020000, 0x30000000..0x30008000\n"
        );
        t(&widened);
    }

    #[test]
    fn boot_block_words_emitted_after_vector_table() {
        // [boot_block] words land verbatim in a .boot_block output section in
        // the code block, directly after the vector table (the RP2350
        // IMAGE_DEF must start within the boot ROM's 4 kB scan window).
        let src = "\
arch = armv7em
vector_table_offset = 0x10000000
data_block = sram
[boot_block]
0xffffded3
0x10210142
0x000001ff
0x00000000
0xab123579
[mem.flash]
base = 0x10000000
size = 4M
[mem.sram]
base = 0x20000000
size = 512K
";
        let target = t(src);
        assert_eq!(target.boot_block.len(), 5);
        assert_eq!(target.boot_block[0], 0xFFFF_DED3);
        let ld = target.generate_linker_script();
        let vt = ld.find(".vector_table").unwrap();
        let bb = ld.find(".boot_block").unwrap();
        let text = ld.find(".text").unwrap();
        assert!(
            vt < bb && bb < text,
            "boot_block between table and text:\n{ld}"
        );
        assert!(ld.contains("LONG(0xAB123579)"), "got:\n{ld}");
    }

    #[test]
    fn boot_block_bad_word_is_error() {
        let err = Target::parse("arch = armv7em\n[boot_block]\nnope\n").unwrap_err();
        assert!(err.contains("boot_block word"), "got: {err}");
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
handoff = P.R align 3
";
        let err = Target::parse(src).unwrap_err();
        assert!(err.contains("power of two"), "got: {err}");
    }

    #[test]
    fn unknown_section_is_error() {
        let err = Target::parse("arch = armv7em\n[bogus]\nx = 1\n").unwrap_err();
        assert!(err.contains("unknown section"), "got: {err}");
    }

    // A project target `include`s a base (chip physics) and inherits all of it,
    // then overrides a property (cacheable, key-level merge keeps base/size) and
    // adds a region. The whole point of moving regions (policy) out of the
    // per-chip physics file.
    #[test]
    fn include_inherits_overrides_and_extends() {
        let dir = std::env::temp_dir().join(format!("bml_tgt_inc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.target");
        let proj = dir.join("proj.target");
        fs::write(
            &base,
            "arch = armv7em\n\
             cpu = cortex-m7\n\
             flash_base = 0x08000000\n\
             ram_base = 0x20000000\n\
             [mem.flash]\n\
             base = 0x08000000\n\
             size = 64K\n\
             [mem.sram]\n\
             base = 0x20000000\n\
             size = 128K\n\
             [mem.dma_pool]\n\
             base = 0x30000000\n\
             size = 16K\n\
             cacheable = true\n\
             [agent.eth]\n\
             kind = dma\n\
             reach = dma_pool\n\
             handoff = Ethernet_DMA.DMACTxDLAR\n",
        )
        .unwrap();
        fs::write(
            &proj,
            "include = base.target\n\
             [mem.dma_pool]\n\
             cacheable = false\n\
             [region.dma_shared]\n\
             mem = dma_pool\n\
             agents = eth\n",
        )
        .unwrap();

        let t = Target::from_file(&proj).expect("include should resolve and merge");
        // Inherited from base verbatim.
        assert_eq!(t.cpu.as_deref(), Some("cortex-m7"));
        assert_eq!(t.agents.len(), 1);
        assert_eq!(t.agents[0].name, "eth");
        assert_eq!(
            t.agents[0].handoffs().next().unwrap().register,
            "Ethernet_DMA.DMACTxDLAR"
        );
        // Key-level override: cacheable flipped, base/size kept from the base.
        let m = t.mem_blocks.iter().find(|m| m.name == "dma_pool").unwrap();
        assert_eq!(m.base, 0x3000_0000);
        assert_eq!(m.size, 16 * 1024);
        assert!(!m.cacheable);
        // Region added by the project (would fail the cache check if still cacheable).
        assert_eq!(t.regions.len(), 1);
        assert_eq!(t.regions[0].name, "dma_shared");
        assert_eq!(t.regions[0].mem, "dma_pool");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_missing_file_errors() {
        let dir = std::env::temp_dir().join(format!("bml_tgt_incmiss_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proj = dir.join("proj.target");
        fs::write(&proj, "include = nope.target\narch = armv7em\n").unwrap();
        let err = Target::from_file(&proj).unwrap_err();
        assert!(err.contains("cannot read target"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    // With mem blocks, the code block is inferred from vector_table_offset and
    // the working RAM is chosen by `data_block` -- no flat flash_*/ram_* keys.
    #[test]
    fn data_block_selects_working_ram() {
        let t = t("arch = armv7em\n\
                   vector_table_offset = 0x08000000\n\
                   data_block = sram2\n\
                   [mem.flash]\n\
                   base = 0x08000000\n\
                   size = 256K\n\
                   [mem.sram1]\n\
                   base = 0x20000000\n\
                   size = 64K\n\
                   [mem.sram2]\n\
                   base = 0x30000000\n\
                   size = 32K\n");
        assert_eq!(t.flash_block().unwrap().name, "flash");
        assert_eq!(t.ram_block().unwrap().name, "sram2");
        let ld = t.generate_linker_script();
        assert!(ld.contains("flash (rx) : ORIGIN = 0x08000000"), "ld:\n{ld}");
        // .data/.bss/.stack go to the selected working RAM, not the other SRAM.
        assert!(ld.contains("} > sram2"), "ld:\n{ld}");
        assert!(!ld.contains("} > sram1"), "ld:\n{ld}");
    }

    #[test]
    fn data_block_unknown_is_error() {
        let err = Target::parse(
            "arch = armv7em\n\
             data_block = nope\n\
             [mem.flash]\n\
             base = 0x08000000\n\
             size = 256K\n\
             [mem.sram]\n\
             base = 0x20000000\n\
             size = 64K\n",
        )
        .unwrap_err();
        assert!(
            err.contains("data_block") && err.contains("no [mem"),
            "got: {err}"
        );
    }

    // A `cacheable = false` mem block becomes an MPU non-cacheable region; a
    // cacheable one does not. The emitter programs one MPU region per entry.
    #[test]
    fn mpu_regions_from_noncacheable_blocks() {
        let t = t("arch = armv7em\n\
                   cpu = cortex-m7\n\
                   vector_table_offset = 0x08000000\n\
                   data_block = sram\n\
                   [mem.flash]\n\
                   base = 0x08000000\n\
                   size = 256K\n\
                   [mem.sram]\n\
                   base = 0x20000000\n\
                   size = 64K\n\
                   [mem.dma]\n\
                   base = 0x30000000\n\
                   size = 4K\n\
                   cacheable = false\n");
        assert_eq!(t.mpu_regions(), vec![(0x3000_0000, 4096)]);
    }

    // A non-cacheable (MPU) block must be MPU-encodable: power-of-two size,
    // size-aligned base.
    #[test]
    fn pmsa8_accepts_non_pow2_regions() {
        // PMSAv8 (cortex-m33) is [base, limit] at 32-byte granularity: a
        // 12K region (not a power of two) is fine, and the flavor follows
        // the core.
        let src = "\
arch = armv7em
cpu = cortex-m33
vector_table_offset = 0x10000000
data_block = sram
[mem.flash]
base = 0x10000000
size = 4M
[mem.sram]
base = 0x20000000
size = 512K
[mem.pool]
base = 0x20080000
size = 12K
cacheable = false
";
        let target = t(src);
        assert_eq!(target.mpu_flavor(), crate::arch::MpuFlavor::Pmsa8);
        assert_eq!(target.mpu_regions(), vec![(0x2008_0000, 12 * 1024)]);
    }

    #[test]
    fn pmsa8_rejects_unaligned_regions() {
        let src = "\
arch = armv7em
cpu = cortex-m33
vector_table_offset = 0x10000000
data_block = sram
[mem.flash]
base = 0x10000000
size = 4M
[mem.sram]
base = 0x20000000
size = 512K
[mem.pool]
base = 0x20080010
size = 48
cacheable = false
";
        let err = Target::parse(src).unwrap_err();
        assert!(err.contains("PMSAv8"), "got: {err}");
    }

    #[test]
    fn noncacheable_block_must_be_mpu_shaped() {
        let err = Target::parse(
            "arch = armv7em\n\
             cpu = cortex-m7\n\
             vector_table_offset = 0x08000000\n\
             data_block = sram\n\
             [mem.flash]\n\
             base = 0x08000000\n\
             size = 256K\n\
             [mem.sram]\n\
             base = 0x20000000\n\
             size = 64K\n\
             [mem.dma]\n\
             base = 0x30000000\n\
             size = 3K\n\
             cacheable = false\n",
        )
        .unwrap_err();
        assert!(err.contains("power of two"), "got: {err}");
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
        // sram1 backs the region but nothing covers the vector table -> rejected.
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
        assert!(err.contains("vector table"), "got: {err}");
    }

    #[test]
    fn extent_parses_and_validates() {
        let src = "\
arch = armv7em
ram_base = 0x30000000
[agent.d]
kind = dma
extent = DMA.CNT.COUNT x4
";
        let target = t(src);
        let Some(ExtentSpec::Counter(eb)) = &target.agents[0].channels[0].extent else {
            panic!("expected counter extent");
        };
        assert_eq!(eb.path, "DMA.CNT.COUNT");
        assert_eq!(eb.scale, 4);

        // Default scale is 1 (the field counts bytes).
        let target = t(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\nextent = DMA.CNT.COUNT\n",
        );
        let Some(ExtentSpec::Counter(eb1)) = &target.agents[0].channels[0].extent else {
            panic!("expected counter extent");
        };
        assert_eq!(eb1.scale, 1);

        // Path must be Peripheral.REGISTER.FIELD; multiplier must be xN.
        let err = Target::parse(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\nextent = DMA.CNT\n",
        )
        .unwrap_err();
        assert!(err.contains("Peripheral.REGISTER.FIELD"), "got: {err}");
        let err = Target::parse(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\nextent = DMA.CNT.COUNT 4\n",
        )
        .unwrap_err();
        assert!(err.contains("xN"), "got: {err}");

        // Unit cross-check clause: `when P.R.F = V`, with or without an
        // explicit multiplier before it.
        let target = t(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\nextent = DMA.CNT.COUNT x4 when DMA.CTRL.SIZE = 2\n",
        );
        let Some(ExtentSpec::Counter(eb)) = &target.agents[0].channels[0].extent else {
            panic!("expected counter extent");
        };
        assert_eq!(eb.unit.as_ref().unwrap(), &("DMA.CTRL.SIZE".to_string(), 2));
        let target = t(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\nextent = DMA.CNT.COUNT when DMA.CTRL.SIZE = 1\n",
        );
        let Some(ExtentSpec::Counter(eb)) = &target.agents[0].channels[0].extent else {
            panic!("expected counter extent");
        };
        assert_eq!(eb.scale, 1);
        assert_eq!(eb.unit.as_ref().unwrap().1, 1);
        let err = Target::parse(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\nextent = DMA.CNT.COUNT x4 when DMA.CTRL.SIZE\n",
        )
        .unwrap_err();
        assert!(err.contains("= <value>"), "got: {err}");
    }

    #[test]
    fn agent_channels_parse_and_merge() {
        let src = "\
arch = armv7em
ram_base = 0x30000000
[agent.dma]
kind = dma
enabled_by = RCC.EN.DMA
[agent.dma.ch0]
handoff = DMA.CH0_SAR
completes_by = DMA.ISR.TC0
extent = DMA.CH0_CNT.N x4
[agent.dma.ch1]
handoff = DMA.CH1_SAR
";
        let target = t(src);
        let dma = &target.agents[0];
        assert_eq!(dma.channels.len(), 2);
        assert_eq!(dma.channels[0].name, "ch0");
        assert_eq!(dma.channels[0].handoffs.len(), 1);
        assert_eq!(dma.channels[0].completes_by, vec!["DMA.ISR.TC0"]);
        assert!(matches!(
            dma.channels[0].extent,
            Some(ExtentSpec::Counter(ref e)) if e.scale == 4
        ));
        assert_eq!(dma.channels[1].name, "ch1");
        // The flat iterator spans channels.
        assert_eq!(dma.handoffs().count(), 2);

        // Agent-level keys inside a channel section are rejected.
        let err = Target::parse(
            "arch = armv7em\nram_base = 0x30000000\n[agent.d]\nkind = dma\n[agent.d.ch0]\nreach = sram\n",
        )
        .unwrap_err();
        assert!(err.contains("belong on the agent"), "got: {err}");

        // Re-opening a channel section (include semantics) resumes it:
        // handoff lines accumulate, no duplicate channel appears.
        let src2 = format!("{src}[agent.dma.ch0]\nhandoff = DMA.CH0_DAR\n");
        let target = t(&src2);
        assert_eq!(target.agents[0].channels.len(), 2);
        assert_eq!(target.agents[0].channels[0].handoffs.len(), 2);
    }

    #[test]
    fn agent_level_transaction_keys_form_default_channel() {
        let src = "\
arch = armv7em
ram_base = 0x30000000
[agent.d]
kind = dma
handoff = DMA.SAR
completes_by = DMA.ISR.TC
";
        let target = t(src);
        let d = &target.agents[0];
        assert_eq!(d.channels.len(), 1);
        assert_eq!(d.channels[0].name, "");
        assert_eq!(d.handoffs().count(), 1);
        assert_eq!(d.completes_by().count(), 1);
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
