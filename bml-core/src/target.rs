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
        let mut section: Option<&str> = None;

        for (line_num, line) in input.lines().enumerate() {
            let line = line.trim();
            // Skip comments and blank lines
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            // Section header
            if line.starts_with('[') && line.ends_with(']') {
                section = Some(&line[1..line.len() - 1]);
                continue;
            }
            // In [interrupts] section: name = slot
            if section == Some("interrupts") {
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
                "arch" => target.arch = val.to_string(),
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

        Ok(target)
    }

    #[must_use]
    pub fn to_arch(&self) -> crate::arch::Arch {
        match self.arch.as_str() {
            "armv6m" => crate::arch::Arch::Armv6m,
            "armv7m" => crate::arch::Arch::Armv7m,
            "armv7em" => crate::arch::Arch::Armv7em,
            _ => crate::arch::Arch::Armv7em,
        }
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

    #[must_use]
    pub fn generate_linker_script(&self) -> String {
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
    fn cflags_errors_on_unknown_cpu() {
        let target = t("arch = armv7em\ncpu = cortex-x99\n");
        let err = target.to_gcc_flags().unwrap_err();
        assert!(err.contains("cortex-x99"), "got: {err}");
    }
}
