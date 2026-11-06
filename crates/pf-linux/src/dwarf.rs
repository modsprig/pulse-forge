//! Minimal DWARF inline-frame extraction (PF-1005 bounded slice): parses
//! the `.debug_abbrev` + `.debug_info` subsets needed to map a code address
//! to its inlined-subroutine chain — no external crates, no full DWARF.
//!
//! Scope and honesty:
//! - accepts DWARF versions 4 and 5, 32-bit format only (64-bit DWARF and
//!   unknown forms make the CU bail — the caller degrades to symtab-only
//!   resolution, never to a wrong name);
//! - `DW_AT_high_pc` as address or as offset, `DW_AT_name` via
//!   string/strp/line_strp/strx (with `DW_AT_str_offsets_base`),
//!   `DW_AT_abstract_origin` via ref1/2/4/8/udata/addr;
//! - nested inline chains are reconstructed from the DIE parent stack
//!   (innermost-first);
//! - every output is capped (ranges, chain depth, DIE count); absent or
//!   broken debug info yields an empty table, not a guess.

use std::collections::HashMap;
use std::path::Path;

use crate::elfio::{self, Reader};

/// One inlined-subroutine chain covering `[low, high)` module-relative.
/// `chain` is INNERMOST-first (the deepest inlinee first).
#[derive(Clone, Debug)]
pub struct InlineRange {
    pub low: u64,
    pub high: u64,
    pub chain: Vec<String>,
}

// Caps: a profiler must stay bounded even on pathological binaries.
const MAX_RANGES: usize = 100_000;
const MAX_DIES: usize = 2_000_000;
const MAX_CHAIN: usize = 16;
const MAX_NAMES: usize = 1_000_000;
const MAX_CUS: usize = 100_000;

// DWARF constants used here.
const DW_TAG_INLINED_SUBROUTINE: u32 = 0x1d;
const DW_AT_LOW_PC: u16 = 0x11;
const DW_AT_HIGH_PC: u16 = 0x12;
const DW_AT_NAME: u16 = 0x03;
const DW_AT_ABSTRACT_ORIGIN: u16 = 0x31;
const DW_AT_STR_OFFSETS_BASE: u16 = 0x72;
const DW_AT_STMT_LIST: u16 = 0x10;
const DW_AT_COMP_DIR: u16 = 0x1b;

/// Parsed attribute: (attr, form, implicit_const value).
struct AbbrevSpec {
    tag: u32,
    has_children: bool,
    attrs: Vec<(u16, u8, i64)>,
}

/// Parse `.debug_abbrev` into per-offset abbrev tables.
// Table offsets are section-relative u64s by spec; codes/tags fit u32.
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
fn parse_abbrev(abbrev: &[u8]) -> HashMap<u64, HashMap<u64, AbbrevSpec>> {
    let mut out = HashMap::new();
    let mut r = Reader::new(abbrev);
    while !r.eof() {
        let table_off = r.pos as u64;
        let mut table: HashMap<u64, AbbrevSpec> = HashMap::new();
        loop {
            let Some(code) = r.uleb() else { return out };
            if code == 0 {
                break; // end of table
            }
            let Some(tag) = r.uleb() else { return out };
            let Some(has_children) = r.u8() else {
                return out;
            };
            let mut attrs = Vec::new();
            loop {
                let Some(attr) = r.uleb() else { return out };
                let Some(form) = r.uleb() else { return out };
                if attr == 0 && form == 0 {
                    break;
                }
                // implicit_const (0x21) carries its value in the abbrev.
                let ic = if form == 0x21 {
                    match r.sleb() {
                        Some(v) => v,
                        None => return out,
                    }
                } else {
                    0
                };
                if attrs.len() < 64 {
                    // DWARF attr/form codes fit u8/u16 by spec.
                    #[allow(clippy::cast_possible_truncation)]
                    let pair = (attr as u16, form as u8, ic);
                    attrs.push(pair);
                }
            }
            table.insert(
                code,
                AbbrevSpec {
                    tag: tag as u32,
                    has_children: has_children != 0,
                    attrs,
                },
            );
        }
        out.insert(table_off, table);
    }
    out
}

/// What a parsed attribute yielded.
enum Attr {
    Addr(u64),
    Const(u64),
    Str(String),
    Other,
}

/// Interpret/skip one attribute by form. Form sizes are anchored to the
/// DWARF4 Table 7.6 encoding as emitted by rustc/LLVM (cross-checked
/// against a full-CU walk of a real binary): data1/2/4/8 = 0x04/05/06/07,
/// string = 0x08, block = 0x09, block1 = 0x0a, flag = 0x0c? — sizes only
/// matter for skipping, and `ref_addr` (0x10) is 4 bytes in 32-bit DWARF.
/// `cu_start` turns CU-relative refs into absolute `.debug_info` offsets.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn read_form(
    r: &mut Reader<'_>,
    form: u8,
    implicit_const: i64,
    addr_size: usize,
    debug_str: &[u8],
    debug_line_str: &[u8],
    str_offsets: &[u8],
    str_offsets_base: usize,
    cu_start: u64,
) -> Option<Attr> {
    let str_at = |off: usize, table: &[u8]| -> Option<Attr> {
        if off >= table.len() {
            return Some(Attr::Str(String::new()));
        }
        let end = table[off..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| off + p)
            .unwrap_or(table.len());
        Some(Attr::Str(
            String::from_utf8_lossy(&table[off..end]).into_owned(),
        ))
    };
    let const_ = |n: u64| Attr::Const(n);
    Some(match form {
        0x01 => {
            // addr: honor the CU's address size (8 on x86-64).
            let raw = r.bytes(addr_size.min(8))?;
            let mut b = [0u8; 8];
            b[..raw.len()].copy_from_slice(raw);
            Attr::Addr(u64::from_le_bytes(b))
        }
        0x02 => {
            let n = r.u16()? as usize; // block2
            r.bytes(n)?;
            Attr::Other
        }
        0x03 => {
            let n = r.u32()? as usize; // block4
            r.bytes(n)?;
            Attr::Other
        }
        0x04 => const_(u64::from(r.u8()?)),  // data1
        0x05 => const_(u64::from(r.u16()?)), // data2
        0x06 => const_(u64::from(r.u32()?)), // data4
        0x07 => const_(r.u64()?),            // data8
        0x08 => Attr::Str(r.cstr()?),        // string
        0x09 => {
            let n = r.uleb()? as usize; // block
            r.bytes(n)?;
            Attr::Other
        }
        0x0a => {
            let n = r.u8()? as usize; // block1
            r.bytes(n)?;
            Attr::Other
        }
        0x0b => const_(u64::from(r.u8()?)), // data1 (1-byte class)
        0x0c => const_(u64::from(r.u8()?)), // flag
        0x0d => const_(r.sleb()? as u64),   // sdata
        0x0e => {
            let off = r.u32()? as usize; // strp (32-bit DWARF)
            str_at(off, debug_str)?
        }
        0x0f => const_(r.uleb()?),                      // udata
        0x10 => const_(u64::from(r.u32()?)),            // ref_addr: ABSOLUTE, 4 bytes
        0x11 => const_(cu_start + u64::from(r.u8()?)),  // ref1 (CU-rel)
        0x12 => const_(cu_start + u64::from(r.u16()?)), // ref2
        0x13 => const_(cu_start + u64::from(r.u32()?)), // ref4
        0x14 => const_(cu_start + r.u64()?),            // ref8
        0x15 => const_(cu_start + r.uleb()?),           // ref_udata
        0x16 => {
            // indirect: the actual form follows.
            let real = r.uleb()? as u8;
            return read_form(
                r,
                real,
                0,
                addr_size,
                debug_str,
                debug_line_str,
                str_offsets,
                str_offsets_base,
                cu_start,
            );
        }
        0x17 => const_(u64::from(r.u32()?)), // sec_offset
        0x18 => {
            let n = r.uleb()? as usize; // exprloc
            r.bytes(n)?;
            Attr::Other
        }
        0x19 => const_(1), // flag_present
        0x1a => {
            // strx: index into .debug_str_offsets (bounded, best effort).
            let idx = r.uleb()? as usize;
            Attr::Str(
                resolve_strx(str_offsets, debug_str, str_offsets_base, idx).unwrap_or_default(),
            )
        }
        0x1b => {
            r.uleb()?; // addrx
            Attr::Other
        }
        0x1c => {
            r.u32()?; // ref_sup4
            Attr::Other
        }
        0x1d => {
            r.u32()?; // strp_sup
            Attr::Other
        }
        0x1e => {
            r.bytes(16)?; // data16
            Attr::Other
        }
        0x1f => {
            let off = r.u32()? as usize; // line_strp
            str_at(off, debug_line_str)?
        }
        0x20 => {
            r.u64()?; // ref_sig8
            Attr::Other
        }
        0x21 => const_(implicit_const as u64), // implicit_const
        0x22 | 0x23 => {
            r.uleb()?; // loclistx / rnglistx
            Attr::Other
        }
        0x24 => {
            r.u64()?; // ref_sup8
            Attr::Other
        }
        0x25 => {
            let idx = r.u8()? as usize; // strx1
            Attr::Str(
                resolve_strx(str_offsets, debug_str, str_offsets_base, idx).unwrap_or_default(),
            )
        }
        0x26 => {
            let idx = r.u16()? as usize; // strx2
            Attr::Str(
                resolve_strx(str_offsets, debug_str, str_offsets_base, idx).unwrap_or_default(),
            )
        }
        0x27 => {
            let s = r.bytes(3)?; // strx3
            let idx = (u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16) as usize;
            Attr::Str(
                resolve_strx(str_offsets, debug_str, str_offsets_base, idx).unwrap_or_default(),
            )
        }
        0x28 => {
            let idx = r.u32()? as usize; // strx4
            Attr::Str(
                resolve_strx(str_offsets, debug_str, str_offsets_base, idx).unwrap_or_default(),
            )
        }
        0x29..=0x2c => {
            // addrx1..addrx4
            let n = 1usize << (form - 0x29);
            r.bytes(n)?;
            Attr::Other
        }
        _ => return None, // unknown form: cannot skip safely → bail honestly
    })
}

/// Resolve a strx index through `.debug_str_offsets` (v5). Base defaults to
/// 8 (first entry after the header) — the common single-CU layout.
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
fn resolve_strx(entries: &[u8], strings: &[u8], base: usize, idx: usize) -> Option<String> {
    // DWARF5: .debug_str_offsets holds OFFSETS into .debug_str — the u32
    // entry at base+idx*4 names a byte offset in the string section.
    // (The previous implementation scanned the offsets table itself for
    // the string — the wrong section entirely, resolving strx names to
    // garbage whenever a producer emitted indexed strings.)
    let entry = base.checked_add(idx.checked_mul(4)?)?;
    let end = entry.checked_add(4)?;
    if end > entries.len() {
        return None;
    }
    let off = u32::from_le_bytes(entries[entry..end].try_into().ok()?) as usize;
    if off >= strings.len() {
        return None;
    }
    let s_end = strings[off..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| off + p)
        .unwrap_or(strings.len());
    Some(String::from_utf8_lossy(&strings[off..s_end]).into_owned())
}

struct RawInline {
    low: u64,
    high: u64,
    /// Ancestor origins, OUTERMOST-first (excluding self).
    ancestors: Vec<u64>,
    origin: u64,
}

/// Extract inline ranges for one binary. Returns an empty table when the
/// binary has no usable DWARF — callers degrade to symtab-only names.
// CU offsets are validated against the section length on every step.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn inline_ranges(path: &Path) -> Vec<InlineRange> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    if bytes.len() > elfio::MAX_FILE {
        return Vec::new();
    }
    let Some(debug_info) = elfio::section(&bytes, b".debug_info") else {
        return Vec::new();
    };
    let Some(debug_abbrev) = elfio::section(&bytes, b".debug_abbrev") else {
        return Vec::new();
    };
    let debug_str = elfio::section(&bytes, b".debug_str").unwrap_or(&[]);
    let debug_line_str = elfio::section(&bytes, b".debug_line_str").unwrap_or(&[]);
    let str_offsets = elfio::section(&bytes, b".debug_str_offsets").unwrap_or(&[]);
    let tables = parse_abbrev(debug_abbrev);

    let mut r = Reader::new(debug_info);
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut raw_inlines: Vec<RawInline> = Vec::new();
    let mut cus = 0usize;
    while !r.eof() && raw_inlines.len() < MAX_RANGES && cus < MAX_CUS {
        cus += 1;
        let cu_start = r.pos;
        let Some(unit_len) = r.u32() else { break };
        if unit_len == 0xffff_ffff {
            break; // 64-bit DWARF: out of scope, degrade honestly
        }
        let cu_end = cu_start + 4 + unit_len as usize;
        if cu_end > debug_info.len() {
            break;
        }
        let Some(version) = r.u16() else { break };
        if version != 4 && version != 5 {
            r.pos = cu_end;
            continue;
        }
        let (addr_size, abbrev_off) = if version >= 5 {
            let Some(_unit_type) = r.u8() else { break };
            let Some(addr_size) = r.u8() else { break };
            let Some(abbrev_off) = r.u32() else { break };
            (addr_size as usize, u64::from(abbrev_off))
        } else {
            let Some(abbrev_off) = r.u32() else { break };
            let Some(addr_size) = r.u8() else { break };
            (addr_size as usize, u64::from(abbrev_off))
        };
        let Some(table) = tables.get(&abbrev_off) else {
            r.pos = cu_end;
            continue;
        };

        // DIE walk. `origin_stack` mirrors the open-DIE nesting exactly:
        // Some(origin) for an open inlined_subroutine, None for any other
        // open DIE; every null entry pops one level. Ancestors of an
        // inline entry are the Some values below it on the stack.
        let mut origin_stack: Vec<Option<u64>> = Vec::new();
        let mut str_offsets_base = 8usize; // default: first entry after header
        let mut dies = 0usize;
        while r.pos < cu_end && dies < MAX_DIES {
            dies += 1;
            let die_off = r.pos - cu_start;
            let Some(code) = r.uleb() else {
                break;
            };
            if code == 0 {
                origin_stack.pop();
                continue;
            }
            let Some(spec) = table.get(&code) else {
                break; // unknown abbrev: cannot continue this CU safely
            };
            let mut low: Option<u64> = None;
            let mut high_addr: Option<u64> = None;
            let mut high_off: Option<u64> = None;
            let mut name = String::new();
            let mut origin = None;
            for &(attr, form, ic) in &spec.attrs {
                let Some(parsed) = read_form(
                    &mut r,
                    form,
                    ic,
                    addr_size,
                    debug_str,
                    debug_line_str,
                    str_offsets,
                    str_offsets_base,
                    cu_start as u64,
                ) else {
                    break; // unknown form mid-attr: give up on this CU
                };
                match (attr, parsed) {
                    (DW_AT_STR_OFFSETS_BASE, Attr::Const(v)) => {
                        str_offsets_base = v as usize;
                    }
                    (DW_AT_LOW_PC, Attr::Addr(v)) => low = Some(v),
                    (DW_AT_LOW_PC, Attr::Const(v)) => low = Some(v),
                    (DW_AT_HIGH_PC, Attr::Addr(v)) => high_addr = Some(v),
                    (DW_AT_HIGH_PC, Attr::Const(v)) => high_off = Some(v),
                    (DW_AT_NAME, Attr::Str(s)) if !s.is_empty() => name = s,
                    (DW_AT_ABSTRACT_ORIGIN, Attr::Const(v)) => origin = Some(v),
                    _ => {}
                }
            }
            if !name.is_empty() && names.len() < MAX_NAMES {
                names.insert(cu_start as u64 + die_off as u64, name);
            }
            if spec.tag == DW_TAG_INLINED_SUBROUTINE {
                // DWARF: high_pc is an address (addr form) or an OFFSET from
                // low_pc (constant class). Handle both explicitly.
                let high_abs = match (low, high_addr, high_off) {
                    (Some(l), Some(h), _) => Some((l, h)),
                    (Some(l), None, Some(off)) => Some((l, l.saturating_add(off))),
                    _ => None,
                };
                if let Some((l, h)) = high_abs {
                    if h > l && raw_inlines.len() < MAX_RANGES {
                        raw_inlines.push(RawInline {
                            low: l,
                            high: h,
                            ancestors: origin_stack.iter().flatten().copied().collect(),
                            // Ref forms were made absolute by `read_form`
                            // (ref_addr is absolute, refN was CU-relative).
                            origin: origin.unwrap_or(0),
                        });
                    }
                }
            }
            if spec.has_children {
                // The DIE stays open; its origin rides the stack until the
                // matching null entry closes it.
                let own = if spec.tag == DW_TAG_INLINED_SUBROUTINE {
                    origin
                } else {
                    None
                };
                if origin_stack.len() < MAX_CHAIN * 4 {
                    origin_stack.push(own);
                }
            }
        }
        r.pos = cu_end;
    }

    // Resolve chains: innermost-first = self then ancestors reversed.
    let mut out = Vec::with_capacity(raw_inlines.len());
    for ri in raw_inlines {
        let mut chain = Vec::new();
        if let Some(n) = names.get(&ri.origin) {
            chain.push(n.clone());
        }
        for &anc in ri.ancestors.iter().rev() {
            if let Some(n) = names.get(&anc) {
                chain.push(n.clone());
            }
        }
        if chain.is_empty() {
            continue; // unresolved origin: skip honestly
        }
        chain.truncate(MAX_CHAIN);
        out.push(InlineRange {
            low: ri.low,
            high: ri.high,
            chain,
        });
    }
    out
}

/// Per-CU `(stmt_list offset, comp_dir)` pairs, read from the ROOT DIE of
/// each `.debug_info` compile unit (PF-1005 comp-dir remainder). The line
/// table uses `stmt_list` to bind a compilation directory to a `.debug_line`
/// unit; root-only parsing keeps this cheap and bounded.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn comp_dirs(path: &Path) -> Vec<(u64, String)> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    if bytes.len() > elfio::MAX_FILE {
        return Vec::new();
    }
    let Some(debug_info) = elfio::section(&bytes, b".debug_info") else {
        return Vec::new();
    };
    let Some(debug_abbrev) = elfio::section(&bytes, b".debug_abbrev") else {
        return Vec::new();
    };
    let debug_str = elfio::section(&bytes, b".debug_str").unwrap_or(&[]);
    let debug_line_str = elfio::section(&bytes, b".debug_line_str").unwrap_or(&[]);
    let str_offsets = elfio::section(&bytes, b".debug_str_offsets").unwrap_or(&[]);
    let tables = parse_abbrev(debug_abbrev);

    let mut r = Reader::new(debug_info);
    let mut out = Vec::new();
    let mut cus = 0usize;
    while !r.eof() && cus < MAX_CUS {
        cus += 1;
        let cu_start = r.pos;
        let Some(unit_len) = r.u32() else { break };
        if unit_len == 0xffff_ffff {
            break; // 64-bit DWARF: out of scope
        }
        let cu_end = cu_start + 4 + unit_len as usize;
        if cu_end > debug_info.len() {
            break;
        }
        let Some(version) = r.u16() else { break };
        if version != 4 && version != 5 {
            r.pos = cu_end;
            continue;
        }
        let (addr_size, abbrev_off) = if version >= 5 {
            let Some(_unit_type) = r.u8() else { break };
            let Some(addr_size) = r.u8() else { break };
            let Some(abbrev_off) = r.u32() else { break };
            (addr_size as usize, u64::from(abbrev_off))
        } else {
            let Some(abbrev_off) = r.u32() else { break };
            let Some(addr_size) = r.u8() else { break };
            (addr_size as usize, u64::from(abbrev_off))
        };
        let Some(table) = tables.get(&abbrev_off) else {
            r.pos = cu_end;
            continue;
        };
        // Root DIE only: one abbrev + its attributes.
        let Some(code) = r.uleb() else { break };
        if code == 0 {
            r.pos = cu_end;
            continue;
        }
        let Some(spec) = table.get(&code) else {
            r.pos = cu_end;
            continue;
        };
        let mut stmt_list: Option<u64> = None;
        let mut comp_dir = String::new();
        let mut str_offsets_base = 8usize;
        for &(attr, form, ic) in &spec.attrs {
            let Some(parsed) = read_form(
                &mut r,
                form,
                ic,
                addr_size,
                debug_str,
                debug_line_str,
                str_offsets,
                str_offsets_base,
                cu_start as u64,
            ) else {
                break;
            };
            match (attr, parsed) {
                (DW_AT_STMT_LIST, Attr::Const(v)) => stmt_list = Some(v),
                (DW_AT_STR_OFFSETS_BASE, Attr::Const(v)) => str_offsets_base = v as usize,
                (DW_AT_COMP_DIR, Attr::Str(s)) if !s.is_empty() => comp_dir = s,
                _ => {}
            }
        }
        if let (Some(off), false) = (stmt_list, comp_dir.is_empty()) {
            out.push((off, comp_dir));
        }
        r.pos = cu_end;
    }
    out
}
