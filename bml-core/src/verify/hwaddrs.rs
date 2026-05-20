use std::path::Path;

use crate::resolver::SymbolTable;

/// Write hardware address ranges to a file for IKOS's `--hardware-addresses-file`.
///
/// Format: one hex range per line, `0xADDR-0xADDR`.
/// Strategy: per-register granularity. Each register gets a range covering
/// its full 4-byte word.
///
/// # Errors
///
/// Returns `io::Error` if the file cannot be written.
pub fn write_hwaddrs_file(symbols: &SymbolTable, path: &Path) -> std::io::Result<()> {
    let mut lines: Vec<String> = Vec::new();

    for periph in symbols.peripherals.values() {
        for reg in periph.regs.values() {
            let start = periph.base_addr + reg.offset;
            let end = start + 3; // registers are 4 bytes wide, inclusive end
            lines.push(format!("0x{start:08X}-0x{end:08X}"));
        }
    }

    lines.sort();
    lines.dedup();

    let content = lines.join("\n") + "\n";
    std::fs::write(path, content)
}
