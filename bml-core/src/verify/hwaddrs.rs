use std::path::Path;

use crate::resolver::SymbolTable;

/// Every BML peripheral register is treated as a 4-byte word here. `RegSymbol`
/// does not carry a width, and ARM Cortex-M peripheral registers are 4 bytes
/// in practice. If narrower or wider registers ever land, derive the width
/// from the symbol instead of this constant.
const REG_WIDTH_BYTES: u64 = 4;

/// Write hardware address ranges to a file for IKOS's `--hardware-addresses-file`.
///
/// Format: one hex range per line, `0xADDR-0xADDR`.
/// Strategy: per-register granularity. Each register gets a range covering
/// its full word.
///
/// # Errors
///
/// Returns `io::Error` if the file cannot be written.
pub fn write_hwaddrs_file(symbols: &SymbolTable, path: &Path) -> std::io::Result<()> {
    let mut lines: Vec<String> = Vec::new();

    for periph in symbols.peripherals.values() {
        for reg in periph.regs.values() {
            let start = periph.base_addr + reg.offset;
            let end = start + REG_WIDTH_BYTES - 1; // inclusive end
            lines.push(format!("0x{start:08X}-0x{end:08X}"));
        }
    }

    lines.sort();
    lines.dedup();

    let content = lines.join("\n") + "\n";
    std::fs::write(path, content)
}
