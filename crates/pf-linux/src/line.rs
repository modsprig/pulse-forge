//! Minimal DWARF line-number program (PF-1005 remainder): a state machine
//! over the special/standard/extended opcodes of `.debug_line`, producing
//! a bounded address → (file, line) matrix for source attribution.
//!
//! Scope and honesty:
//! - DWARF v2–v4 headers (the dominant layout) plus v5 directory/file
//!   entry formats; VLIW units (`maximum_operations_per_instruction != 1`)
//!   are skipped honestly — every x86-64 target uses 1;
//! - rows collapse consecutive same-(file,line) runs (lookup semantics
//!   preserved: the last row at or below an IP is its location);
//! - the row matrix is capped; a truncated map answers only the addresses
//!   it covered and says so;
//! - directories resolve relative to the compilation directory only when
//!   known; otherwise paths stay relative and honest.

use std::collections::HashMap;
use std::path::Path;

use crate::elfio::{self, Reader};

/// One resolved source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLoc {
    pub file: String,
    pub line: u32,
}

/// Bounded address → source-location map for one binary.
#[derive(Clone, Debug, Default)]
pub struct LineMap {
    /// (address, file index, line), sorted by address, runs collapsed.
    rows: Vec<(u64, u32, u32)>,
    files: Vec<String>,
    /// True when the row cap cut the program short (coverage is partial).
    pub truncated: bool,
}

// Lookup index arithmetic is guarded by partition_point bounds.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
impl LineMap {
    /// Source location for `addr`: the last row at or below it.
    pub fn lookup(&self, addr: u64) -> Option<SourceLoc> {
        let idx = self.rows.partition_point(|r| r.0 <= addr);
        if idx == 0 {
            return None;
        }
        let (a, f, l) = self.rows[idx - 1];
        if a == 0 {
            return None; // post-reset row, not a real location
        }
        let file = self.files.get(f as usize)?;
        if file.is_empty() {
            return None;
        }
        Some(SourceLoc {
            file: file.clone(),
            line: l,
        })
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }
}

const MAX_LINE_ROWS: usize = 1_000_000;

// The line-program walk uses saturating register math (spec registers are
// small; caps bound everything).
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::while_let_loop
)]
pub fn line_map(path: &Path) -> Option<LineMap> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > elfio::MAX_FILE {
        return None;
    }
    let debug_line = elfio::section(&bytes, b".debug_line")?;
    let debug_str = elfio::section(&bytes, b".debug_str").unwrap_or(&[]);
    let debug_line_str = elfio::section(&bytes, b".debug_line_str").unwrap_or(&[]);

    // PF-1005 comp-dir remainder: bind each `.debug_line` unit to its
    // compilation unit's DW_AT_comp_dir so relative paths resolve
    // absolutely. Keyed by the unit's offset in `.debug_line`.
    let comp_dirs: HashMap<u64, String> = crate::dwarf::comp_dirs(path).into_iter().collect();

    let mut r = Reader::new(debug_line);
    let mut rows: Vec<(u64, u32, u32)> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut file_index: HashMap<String, u32> = HashMap::new();
    let mut truncated = false;

    fn intern(name: &str, files: &mut Vec<String>, index: &mut HashMap<String, u32>) -> u32 {
        if let Some(&i) = index.get(name) {
            return i;
        }
        let i = files.len() as u32;
        files.push(name.to_string());
        index.insert(name.to_string(), i);
        i
    }

    while !r.eof() && rows.len() < MAX_LINE_ROWS {
        let unit_start = r.pos;
        let Some(unit_len) = r.u32() else { break };
        if unit_len == 0xffff_ffff {
            break; // 64-bit DWARF: out of scope
        }
        let unit_end = unit_start + 4 + unit_len as usize;
        if unit_end > debug_line.len() {
            break;
        }
        let Some(version) = r.u16() else { break };
        if !(2..=5).contains(&version) {
            r.pos = unit_end;
            continue;
        }
        if version >= 5 {
            let (Some(_addr_size), Some(_seg_sel)) = (r.u8(), r.u8()) else {
                break;
            };
        }
        let Some(header_len) = r.u32() else { break };
        let program_start = r.pos + header_len as usize;
        if program_start > unit_end {
            break;
        }
        let Some(min_inst_len) = r.u8() else { break };
        if version >= 4 {
            let Some(max_ops) = r.u8() else { break };
            if max_ops != 1 {
                r.pos = unit_end;
                continue; // VLIW: out of scope, degrade honestly
            }
        }
        let Some(default_is_stmt) = r.u8() else { break };
        let _ = default_is_stmt;
        let Some(line_base) = r.u8() else { break };
        let line_base = line_base as i8;
        let Some(line_range) = r.u8() else { break };
        if line_range == 0 {
            r.pos = unit_end;
            continue;
        }
        let Some(opcode_base) = r.u8() else { break };
        if opcode_base == 0 {
            r.pos = unit_end;
            continue;
        }
        // standard_opcode_lengths: operand counts for the standard opcodes.
        let mut std_lens = [0u8; 12];
        for slot in std_lens.iter_mut().take(opcode_base as usize - 1) {
            let Some(l) = r.u8() else { break };
            *slot = l;
        }

        let comp_dir = comp_dirs.get(&(unit_start as u64)).map(String::as_str);
        // File tables. v2–v4: include_directories + 1-based file entries.
        // v5: format-described directory/file entries, 0-based files.
        let file_base: u32 = if version <= 4 {
            let mut dirs: Vec<String> = Vec::new();
            loop {
                let Some(d) = r.cstr() else { break };
                if d.is_empty() {
                    break;
                }
                dirs.push(d);
            }
            loop {
                let Some(name) = r.cstr() else { break };
                if name.is_empty() {
                    break;
                }
                let (Some(dir_idx), Some(_mtime), Some(_size)) = (r.uleb(), r.uleb(), r.uleb())
                else {
                    break;
                };
                let joined = resolve_path(
                    comp_dir,
                    dirs.get(dir_idx.saturating_sub(1) as usize),
                    &name,
                );
                intern(&joined, &mut files, &mut file_index);
            }
            1
        } else {
            let dirs = parse_v5_entries(
                &mut r,
                debug_str,
                debug_line_str,
                comp_dir,
                DirOrFile::Dir,
                &mut files,
                &mut file_index,
            );
            parse_v5_entries(
                &mut r,
                debug_str,
                debug_line_str,
                comp_dir,
                DirOrFile::File(&dirs),
                &mut files,
                &mut file_index,
            );
            0
        };

        // The opcode program.
        let mut address: u64 = 0;
        let mut line: u64 = 1;
        // v4: file register starts at 1 (1-based; 0 = unspecified). v5:
        // 0-based — the register starts at 0, the FIRST declared file
        // (a v5 program that never emits set_file must not silently
        // point at the second file).
        let mut file_reg: u64 = if version >= 5 { 0 } else { 1 };
        while r.pos < unit_end && rows.len() < MAX_LINE_ROWS {
            let Some(op) = r.u8() else { break };
            if op >= opcode_base {
                // Special opcode: advance address/line, emit a row.
                let adjusted = u64::from(op - opcode_base);
                let operation_advance = adjusted / u64::from(line_range);
                address = address
                    .saturating_add(u64::from(min_inst_len).saturating_mul(operation_advance));
                let li = i64::from(line_base) + (adjusted % u64::from(line_range)) as i64;
                line = (line as i64 + li).max(0) as u64;
                if !emit_row(&mut rows, address, file_reg, line, file_base) {
                    truncated = true;
                }
            } else if op == 0 {
                // Extended opcode: uleb length, sub-opcode, operands.
                let Some(ext_len) = r.uleb() else { break };
                let ext_end = r.pos + ext_len as usize;
                if ext_len == 0 || ext_end > unit_end {
                    r.pos = unit_end;
                    break;
                }
                let Some(sub) = r.u8() else { break };
                match sub {
                    1 => {
                        // end_sequence: emit the final row, reset registers.
                        if !emit_row(&mut rows, address, file_reg, line, file_base) {
                            truncated = true;
                        }
                        address = 0;
                        line = 1;
                        file_reg = if version >= 5 { 0 } else { 1 };
                    }
                    2 => {
                        // set_address: the address fills the record minus
                        // the sub-opcode byte (8 bytes on x86-64).
                        if ext_len == 9 {
                            let Some(v) = r.u64() else { break };
                            address = v;
                        }
                        r.pos = ext_end;
                    }
                    3 | 4 => {
                        let Some(_) = r.uleb() else { break };
                        r.pos = ext_end;
                    }
                    _ => {
                        r.pos = ext_end; // unknown extended: skip honestly
                    }
                }
                if r.pos > ext_end {
                    r.pos = ext_end;
                }
            } else {
                // Standard opcode.
                match op {
                    1 => {
                        // copy: emit row, reset ephemeral state.
                        if !emit_row(&mut rows, address, file_reg, line, file_base) {
                            truncated = true;
                        }
                    }
                    2 => {
                        let Some(adv) = r.uleb() else { break };
                        address =
                            address.saturating_add(u64::from(min_inst_len).saturating_mul(adv));
                    }
                    3 => {
                        let Some(d) = r.sleb() else { break };
                        line = (line as i64 + d).max(0) as u64;
                    }
                    4 => {
                        let Some(f) = r.uleb() else { break };
                        file_reg = f;
                    }
                    5 => {
                        let Some(_) = r.uleb() else { break }; // set_column
                    }
                    6 | 7 | 10 | 11 | 12 => {} // state flags, no operands
                    8 => {
                        // const_add_pc: like a special opcode 255.
                        let adjusted = u64::from(255 - opcode_base);
                        let operation_advance = adjusted / u64::from(line_range);
                        address = address.saturating_add(
                            u64::from(min_inst_len).saturating_mul(operation_advance),
                        );
                    }
                    9 => {
                        let Some(v) = r.u16() else { break }; // fixed_advance_pc
                        address = address.saturating_add(u64::from(v));
                    }
                    other => {
                        // Unknown standard: skip its declared ULEB operands.
                        let n = std_lens.get((other - 1) as usize).copied().unwrap_or(0);
                        for _ in 0..n {
                            if r.uleb().is_none() {
                                break;
                            }
                        }
                    }
                }
            }
        }
        r.pos = unit_end;
    }

    if rows.is_empty() {
        return None; // no usable line info — honest absence
    }
    // Collapse consecutive same-(file,line) runs (keep the first address).
    rows.dedup_by(|a, b| a.1 == b.1 && a.2 == b.2);
    // Sequences may interleave across CUs: sort by address, then keep the
    // LAST row per address (partition_point wants the highest row ≤ addr).
    rows.sort_unstable_by_key(|r| r.0);
    let mut collapsed: Vec<(u64, u32, u32)> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if collapsed.last().map(|last| last.0) == Some(row.0) {
            *collapsed.last_mut().unwrap() = row;
        } else {
            collapsed.push(row);
        }
    }
    Some(LineMap {
        rows: collapsed,
        files,
        truncated,
    })
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn emit_row(
    rows: &mut Vec<(u64, u32, u32)>,
    address: u64,
    file_reg: u64,
    line: u64,
    file_base: u32,
) -> bool {
    if rows.len() >= MAX_LINE_ROWS {
        return false;
    }
    // v4 file registers are 1-based (0 = unspecified); v5 are 0-based into
    // the same interned order the entries were read in.
    let fidx = if file_base == 1 {
        file_reg.saturating_sub(1)
    } else {
        file_reg
    };
    rows.push((address, fidx as u32, line as u32));
    true
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
/// Resolve a file path under its directory and compilation directory:
/// absolute names stay untouched; relative names join their directory
/// (when known); both join the comp dir when that is known. "." and
/// empty directories collapse instead of polluting the path.
fn resolve_path(comp: Option<&str>, dir: Option<&String>, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_string();
    }
    let dir = dir.filter(|d| !d.is_empty() && d.as_str() != ".");
    match (comp, dir) {
        (Some(c), Some(d)) if !d.starts_with('/') => format!("{c}/{d}/{name}"),
        (Some(c), _) => format!("{c}/{name}"),
        (None, Some(d)) => format!("{d}/{name}"),
        (None, _) => name.to_string(),
    }
}

/// Which v5 entry kind is being parsed.
enum DirOrFile<'a> {
    Dir,
    File(&'a [String]),
}

/// Parse one v5 entry table (directories or file names) with its
/// format-described rows; files are interned so row indices resolve.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn parse_v5_entries(
    r: &mut Reader<'_>,
    debug_str: &[u8],
    debug_line_str: &[u8],
    comp_dir: Option<&str>,
    kind: DirOrFile<'_>,
    files: &mut Vec<String>,
    index: &mut HashMap<String, u32>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(format_count) = r.u8() else {
        return out;
    };
    let mut formats = Vec::new();
    for _ in 0..format_count {
        let (Some(ct), Some(form)) = (r.uleb(), r.uleb()) else {
            return out;
        };
        // DWARF content-type and form codes fit u8.
        #[allow(clippy::cast_possible_truncation)]
        let pair = (ct as u8, form as u8);
        formats.push(pair);
    }
    let Some(count) = r.uleb() else { return out };
    for _ in 0..count.min(100_000) {
        let mut path = String::new();
        let mut dir_idx = 0usize;
        for &(ct, form) in &formats {
            let (val, num) = read_line_form(r, form, debug_str, debug_line_str);
            match ct {
                1 => path = val,             // DW_LNCT_path
                2 => dir_idx = num as usize, // DW_LNCT_directory_index
                _ => {}
            }
        }
        match &kind {
            DirOrFile::Dir => out.push(path),
            DirOrFile::File(dirs) => {
                let joined = resolve_path(comp_dir, dirs.get(dir_idx), &path);
                if !index.contains_key(&joined) {
                    index.insert(joined.clone(), files.len() as u32);
                    files.push(joined.clone());
                }
                out.push(joined);
            }
        }
    }
    out
}

/// Read one v5 line-table form: (string content, constant value).
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn read_line_form(
    r: &mut Reader<'_>,
    form: u8,
    debug_str: &[u8],
    debug_line_str: &[u8],
) -> (String, u64) {
    match form {
        0x08 => (r.cstr().unwrap_or_default(), 0), // string
        0x0e | 0x1f => {
            // strp / line_strp (32-bit DWARF)
            let Some(off) = r.u32() else {
                return (String::new(), 0);
            };
            let table = if form == 0x0e {
                debug_str
            } else {
                debug_line_str
            };
            let off = off as usize;
            if off >= table.len() {
                return (String::new(), 0);
            }
            let end = table[off..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| off + p)
                .unwrap_or(table.len());
            (String::from_utf8_lossy(&table[off..end]).into_owned(), 0)
        }
        0x0b => (String::new(), u64::from(r.u8().unwrap_or(0))),
        0x04 => (String::new(), u64::from(r.u16().unwrap_or(0))),
        0x05 => (String::new(), u64::from(r.u32().unwrap_or(0))),
        0x06 | 0x07 | 0x20 => (String::new(), r.u64().unwrap_or(0)),
        0x0d | 0x0f | 0x1a | 0x1b | 0x22 | 0x23 => (String::new(), r.uleb().unwrap_or(0)),
        0x1e => {
            if r.bytes(16).is_none() {
                return (String::new(), 0);
            }
            (String::new(), 0) // MD5
        }
        _ => (String::new(), 0), // unknown form: contributes nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_map_lookup_semantics() {
        let m = LineMap {
            files: vec!["a.rs".into(), "b.rs".into()],
            rows: vec![(0x100, 0, 10), (0x140, 1, 20), (0x180, 1, 22)],
            ..Default::default()
        };
        assert_eq!(m.lookup(0x0FF), None, "before the first row");
        assert_eq!(m.lookup(0x100).unwrap().line, 10);
        assert_eq!(m.lookup(0x13F).unwrap().line, 10, "runs extend");
        assert_eq!(m.lookup(0x140).unwrap().file, "b.rs");
        assert_eq!(m.lookup(0x1FF).unwrap().line, 22, "last row covers tail");
    }
}
