//! Seeded-mutation fuzz tests for the Linux binary/symbol parsers that
//! face files from disk (PF-1202 extension: ELF symtab, DWARF
//! .debug_abbrev/.debug_info/.debug_line, perf ring regions, /proc maps
//! text). Dependency-free, same method as pf-types' fuzz_json: a
//! deterministic xorshift PRNG generates byte soups and mutations of
//! structurally-valid seeds; the property under test is panic-freedom,
//! termination, and bounded output — every input either parses or yields
//! an honest empty result, never a panic, never an unbounded loop or
//! allocation.

// The parsers live in Linux-gated modules; the suite vanishes elsewhere.
#![cfg(target_os = "linux")]
// Fuzz loops use bounded counters; seed-builder lengths are
// test-controlled and far below the target widths.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::vec_init_then_push
)]

use std::path::PathBuf;

use pf_linux::dwarf::inline_ranges;
use pf_linux::line::line_map;
use pf_linux::perf::parse_ring;
use pf_linux::symbols::{MapsEntry, elf_functions, parse_maps, resolve_ip_chains};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % n.max(1) as u64).unwrap_or(0)
    }
}

const SEEDS: [u64; 6] = [1, 2, 3, 5, 8, 13];

/// Campaign-scale multiplier for hostile-case counts (PF_FUZZ_SCALE,
/// default 1, clamped 1..=64) — lets a CI or a manual deep run expand the
/// corpus without recompiling; gate runs stay at scale 1.
fn fuzz_scale() -> usize {
    std::env::var("PF_FUZZ_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .clamp(1, 64)
}

/// Mutate `base`: bit flips, byte pokes, truncation, duplication, splice.
fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    let rounds = 1 + rng.below(24);
    for _ in 0..rounds {
        if out.is_empty() {
            break;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(out.len());
                out[i] = (rng.next_u64() & 0xFF) as u8;
            }
            2 => {
                let keep = rng.below(out.len());
                out.truncate(keep.max(1));
            }
            3 => {
                // duplicate a slice (grows the file; bounded by 2× base)
                if out.len() < 4096 {
                    let s = rng.below(out.len());
                    let e = (s + 1 + rng.below(64)).min(out.len());
                    let dup: Vec<u8> = out[s..e].to_vec();
                    out.extend_from_slice(&dup);
                }
            }
            _ => {
                // splice: overwrite a run with bytes from elsewhere
                if out.len() > 8 {
                    let dst = rng.below(out.len() - 1);
                    let src = rng.below(out.len());
                    let n = (1 + rng.below(8)).min(out.len() - dst);
                    for k in 0..n {
                        out[dst + k] = out[(src + k) % out.len()];
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Minimal structurally-valid ELF64 seed: symtab + .debug_abbrev +
// .debug_info (CU + subprogram + inlined child) + .debug_line (v4 header +
// address set + row run + end_sequence). Mutations of this drive every
// parser loop with near-valid structure — far more effective than soups.
// ---------------------------------------------------------------------------

fn uleb(v: u64, out: &mut Vec<u8>) {
    let mut x = v;
    loop {
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn build_seed_elf(version: u32) -> Vec<u8> {
    build_seed_elf_inner(version, false)
}

/// Variant with DWARF5 indexed strings: the DIE names use DW_FORM_strx
/// (0x1a) and the CU carries DW_AT_str_offsets_base — exercising the
/// two-section strx resolution (entries in .debug_str_offsets pointing
/// into .debug_str).
fn build_seed_elf_inner(version: u32, strx: bool) -> Vec<u8> {
    // --- .debug_abbrev: table @0 with codes 1 (CU), 2 (subprogram),
    // 3 (inlined_subroutine, abstract_origin ref).
    let mut abbrev = Vec::new();
    uleb(1, &mut abbrev);
    uleb(0x11, &mut abbrev); // DW_TAG_compile_unit
    abbrev.push(0); // no children
    uleb(0x03, &mut abbrev);
    uleb(0x08, &mut abbrev); // DW_AT_name, DW_FORM_string
    if strx {
        uleb(0x72, &mut abbrev);
        uleb(0x17, &mut abbrev); // DW_AT_str_offsets_base, sec_offset
    }
    uleb(0, &mut abbrev);
    uleb(0, &mut abbrev);
    uleb(2, &mut abbrev);
    uleb(0x2e, &mut abbrev); // DW_TAG_subprogram
    abbrev.push(1); // has children
    uleb(0x11, &mut abbrev);
    uleb(0x01, &mut abbrev); // low_pc, addr
    uleb(0x12, &mut abbrev);
    uleb(0x21, &mut abbrev); // high_pc, implicit_const — the VALUE rides
    uleb(0x40, &mut abbrev); // in the abbrev table, not the DIE body
    uleb(0x03, &mut abbrev);
    uleb(if strx { 0x1a } else { 0x08 }, &mut abbrev); // name, strx|string
    uleb(0, &mut abbrev);
    uleb(0, &mut abbrev);
    uleb(3, &mut abbrev);
    uleb(0x1d, &mut abbrev); // DW_TAG_inlined_subroutine
    abbrev.push(0);
    uleb(0x11, &mut abbrev);
    uleb(0x01, &mut abbrev); // low_pc, addr
    uleb(0x12, &mut abbrev);
    uleb(0x0f, &mut abbrev); // high_pc, udata (constant class)
    uleb(0x31, &mut abbrev);
    uleb(0x10, &mut abbrev); // abstract_origin, ref_addr (4 bytes)
    uleb(0x03, &mut abbrev);
    uleb(if strx { 0x1a } else { 0x08 }, &mut abbrev); // name, strx|string
    uleb(0, &mut abbrev);
    uleb(0, &mut abbrev);
    abbrev.push(0); // end of table 0
    // Table 1 (at table1_off): different semantics for the SAME codes —
    // a CU referencing this table must parse its DIEs by THESE shapes.
    // code 1: CU DIE (name, no children); code 2: subprogram (low_pc,
    // high_pc udata, name, children); code 3: inlined (low_pc, high_pc
    // udata, origin ref_addr, name, no children).
    let table1_off = abbrev.len() as u32;
    uleb(1, &mut abbrev);
    uleb(0x03, &mut abbrev);
    abbrev.push(0); // no children
    uleb(0x03, &mut abbrev); // name
    uleb(0x08, &mut abbrev); // string
    uleb(0, &mut abbrev);
    uleb(0, &mut abbrev);
    uleb(2, &mut abbrev);
    uleb(0x2e, &mut abbrev); // DW_TAG_subprogram
    abbrev.push(1);
    uleb(0x11, &mut abbrev);
    uleb(0x01, &mut abbrev);
    uleb(0x12, &mut abbrev);
    uleb(0x0f, &mut abbrev);
    uleb(0x03, &mut abbrev);
    uleb(0x08, &mut abbrev);
    uleb(0, &mut abbrev);
    uleb(0, &mut abbrev);
    uleb(3, &mut abbrev);
    uleb(0x1d, &mut abbrev); // DW_TAG_inlined_subroutine
    abbrev.push(0);
    uleb(0x11, &mut abbrev);
    uleb(0x01, &mut abbrev);
    uleb(0x12, &mut abbrev);
    uleb(0x0f, &mut abbrev);
    uleb(0x31, &mut abbrev);
    uleb(0x10, &mut abbrev); // abstract_origin, ref_addr (ABSOLUTE)
    uleb(0x03, &mut abbrev);
    uleb(0x08, &mut abbrev);
    uleb(0, &mut abbrev);
    uleb(0, &mut abbrev);
    abbrev.push(0); // end of table 1

    // --- .debug_info: CU v4. DIE offsets: CU DIE @11, subprogram @0x1f
    // (computed below by layout), inlined child after. Chain: inlined's
    // abstract_origin refs the subprogram DIE offset.
    let mut info = Vec::new();
    let mut body = Vec::new();
    uleb(1, &mut body); // CU DIE, abbrev 1
    body.extend_from_slice(b"seed.c\0");
    // Subprogram DIE absolute offset = CU header + CU DIE (1 + 7). The
    // header is 11 bytes for v4 (4+2+4+1) and 12 for v5 (4+2+1+1+4) —
    // the DIE offset is version-dependent, exactly as real compilers
    // emit it; the inlined child's abstract_origin ref_addr points
    // there.
    if strx {
        // DW_AT_str_offsets_base (sec_offset): the entries begin after
        // the 8-byte .debug_str_offsets header. The attr CODE+FORM live
        // in the abbrev; the body carries only the VALUE.
        body.extend_from_slice(&8u32.to_le_bytes());
    }
    // The CU DIE's size depends on the strx attribute (the DIE body
    // carries only the 4-byte sec_offset VALUE — the attr code/form live
    // in the abbrev), so the subprogram DIE's absolute offset shifts.
    let cu_die_size = if strx { 12u32 } else { 8u32 };
    let subprogram_off = (if version >= 5 { 12u32 } else { 11u32 }) + cu_die_size;
    uleb(2, &mut body); // abbrev 2
    body.extend_from_slice(&0x1000u64.to_le_bytes()); // low_pc (addr form)
    // high_pc omitted: implicit_const's value lives in the abbrev.
    if strx {
        uleb(0, &mut body); // name: strx index 0 -> "outer"
    } else {
        body.extend_from_slice(b"outer\0");
    }
    uleb(3, &mut body); // abbrev 3
    body.extend_from_slice(&0x1010u64.to_le_bytes()); // low_pc (addr form)
    uleb(0x8, &mut body); // high_pc (udata form -> uleb body)
    body.extend_from_slice(&subprogram_off.to_le_bytes()); // ref_addr → subprogram DIE
    if strx {
        uleb(1, &mut body); // name: strx index 1 -> "inner"
    } else {
        body.extend_from_slice(b"inner\0");
    }
    body.push(0); // end of subprogram's children
    // CU header: v4 = version, abbrev_offset, addr_size; v5 = version,
    // unit_type, addr_size, abbrev_offset (unit_type 0x01 = full CU).
    // Both are 8 bytes after the version, so the DIE layout — and the
    // ref_addr offset above — is identical across versions.
    // unit_length covers everything after itself: version (2) + the
    // version-specific header (v4: abbrev_off 4 + addr_size 1 = 7 total;
    // v5: unit_type 1 + addr_size 1 + abbrev_off 4 = 8 total) + body.
    let hdr_after_ver = if version >= 5 { 8 } else { 7 };
    let unit_len = u32::try_from(hdr_after_ver + body.len()).unwrap_or(0);
    info.extend_from_slice(&unit_len.to_le_bytes());
    info.extend_from_slice(&(version as u16).to_le_bytes());
    if version >= 5 {
        info.push(0x01); // DW_UT_compile
        info.push(8); // addr_size
        info.extend_from_slice(&0u32.to_le_bytes()); // abbrev_offset
    } else {
        info.extend_from_slice(&0u32.to_le_bytes()); // abbrev_offset
        info.push(8); // addr_size
    }
    info.extend_from_slice(&body);

    // --- CU2: a second compile unit with its OWN abbrev table (at
    // table1_off) and a CROSS-CU inline — its abstract_origin points at
    // CU1's subprogram by ABSOLUTE offset. Exercises multi-CU walking,
    // per-CU abbrev selection, and absolute-ref name resolution.
    let mut body2 = Vec::new();
    uleb(1, &mut body2); // CU DIE (table1 code 1)
    body2.extend_from_slice(b"cu2.c\0");
    uleb(2, &mut body2); // subprogram (table1 code 2)
    body2.extend_from_slice(&0x3000u64.to_le_bytes()); // low_pc
    uleb(0x10, &mut body2); // high_pc (udata)
    body2.extend_from_slice(b"cu2_fn\0");
    uleb(3, &mut body2); // inlined (table1 code 3)
    body2.extend_from_slice(&0x3010u64.to_le_bytes()); // low_pc
    uleb(0x8, &mut body2); // high_pc (udata)
    body2.extend_from_slice(&subprogram_off.to_le_bytes()); // origin → CU1 (cross-CU)
    body2.extend_from_slice(b"cu2_inner\0");
    body2.push(0); // end of subprogram's children
    let unit_len2 = u32::try_from(2 + 4 + 1 + body2.len()).unwrap_or(0);
    info.extend_from_slice(&unit_len2.to_le_bytes());
    info.extend_from_slice(&4u16.to_le_bytes());
    info.extend_from_slice(&table1_off.to_le_bytes()); // abbrev_offset → table 1
    info.push(8);
    info.extend_from_slice(&body2);

    // --- .debug_line v4: unit_length, version, header_length, header,
    // then the opcode program (kept separate for exact length math).
    let mut ops = Vec::new();
    let mut hdr = Vec::new();
    hdr.push(1); // min_inst_len
    hdr.push(1); // max_ops_per_inst
    hdr.push(1); // default_is_stmt
    hdr.push(0xFB); // line_base (-5)
    hdr.push(14); // line_range
    hdr.push(13); // opcode_base (12 std opcodes)
    hdr.extend_from_slice(&[0u8; 12]); // std_opcode_lengths
    hdr.push(0); // include_directories: empty
    // v4 file entry: name, dir index (uleb), mtime (uleb), length (uleb);
    // an empty name ends the table.
    hdr.extend_from_slice(b"seed.c\0");
    hdr.push(0); // dir index
    hdr.push(0); // mtime
    hdr.push(0); // length
    hdr.push(0); // end of file_names
    // Program: extended set_address, advance_line +5, two advance_pc+copy
    // rows, extended end_sequence.
    ops.extend_from_slice(&[0x00, 0x09, 0x02]); // ext, len 9, DW_LNE_set_address
    ops.extend_from_slice(&0x1000u64.to_le_bytes());
    ops.extend_from_slice(&[0x03, 0x05]); // DW_LNS_advance_line, sleb +5
    ops.extend_from_slice(&[0x02, 0x04]); // DW_LNS_advance_pc, uleb +4
    ops.push(0x01); // DW_LNS_copy → row (0x1004, line 6)
    ops.extend_from_slice(&[0x02, 0x04]); // advance_pc +4
    ops.push(0x01); // copy → row (0x1008, line 6)
    ops.extend_from_slice(&[0x00, 0x01, 0x01]); // ext end_sequence
    // unit_length covers version + header_length field + header + program.
    let unit_len = (2 + 4 + hdr.len() + ops.len()) as u32;
    let mut line = Vec::new();
    line.extend_from_slice(&unit_len.to_le_bytes());
    line.extend_from_slice(&4u16.to_le_bytes());
    line.extend_from_slice(&u32::try_from(hdr.len()).unwrap_or(0).to_le_bytes()); // header_length
    line.extend_from_slice(&hdr);
    line.extend_from_slice(&ops);

    // --- .symtab + .strtab: one STT_FUNC at 0x1000 named "outer".
    let mut strtab: Vec<u8> = vec![0];
    let name_off = strtab.len() as u32;
    strtab.extend_from_slice(b"outer\0");
    let mut symtab = Vec::new();
    symtab.extend_from_slice(&[0u8; 24]); // null symbol
    symtab.extend_from_slice(&name_off.to_le_bytes()); // st_name
    symtab.push(0x12); // st_info: STT_FUNC|GLOBAL
    symtab.push(0); // st_other
    symtab.extend_from_slice(&2u16.to_le_bytes()); // st_shndx (text placeholder)
    symtab.extend_from_slice(&0x1000u64.to_le_bytes()); // st_value
    symtab.extend_from_slice(&0x40u64.to_le_bytes()); // st_size

    // --- Assemble the ELF64: header + one contiguous blob per section.
    let mut body: Vec<u8> = Vec::new();
    let mut sections: Vec<(u32, u64, u64, Vec<u8>)> = Vec::new(); // (type, offset, size, data)
    let place = |sections: &mut Vec<(u32, u64, u64, Vec<u8>)>,
                 body: &mut Vec<u8>,
                 ty: u32,
                 data: &Vec<u8>| {
        // align 8; offsets are FILE offsets (64-byte ELF header first)
        while body.len() % 8 != 0 {
            body.push(0);
        }
        let off = 64u64 + body.len() as u64;
        body.extend_from_slice(data);
        sections.push((ty, off, data.len() as u64, Vec::new()));
    };
    place(&mut sections, &mut body, 2, &symtab); // SHT_SYMTAB
    place(&mut sections, &mut body, 3, &strtab); // SHT_STRTAB
    place(&mut sections, &mut body, 1, &abbrev); // .debug_abbrev (PROGBITS)
    place(&mut sections, &mut body, 1, &info); // .debug_info
    place(&mut sections, &mut body, 1, &line);
    if strx {
        // .debug_str_offsets: 8-byte header (unit_length, version, pad)
        // + u32 entries pointing into .debug_str.
        let mut so: Vec<u8> = Vec::new();
        so.extend_from_slice(&12u32.to_le_bytes()); // unit_length: ver(2)+pad(2)+entries(8)
        so.extend_from_slice(&5u16.to_le_bytes());
        so.extend_from_slice(&[0u8; 2]); // padding
        so.extend_from_slice(&0u32.to_le_bytes()); // entry 0 → "outer"
        so.extend_from_slice(&6u32.to_le_bytes()); // entry 1 → "inner"
        let mut dstr: Vec<u8> = Vec::new();
        dstr.extend_from_slice(b"outer\0");
        dstr.extend_from_slice(b"inner\0");
        place(&mut sections, &mut body, 1, &so); // .debug_str_offsets
        place(&mut sections, &mut body, 1, &dstr); // .debug_str
    } // .debug_line

    let shnum = 2 + sections.len() as u64; // NULL + .shstrtab + sections
    // .shstrtab — the name list must match the PLACEMENT ORDER of the
    // sections above (the strx variant adds two string sections).
    let mut shstr: Vec<u8> = vec![0];
    let mut names: Vec<u32> = Vec::new();
    let mut name_list = vec![
        ".symtab\0",
        ".strtab\0",
        ".debug_abbrev\0",
        ".debug_info\0",
        ".debug_line\0",
    ];
    if strx {
        name_list.insert(5, ".debug_str_offsets\0");
        name_list.insert(6, ".debug_str\0");
    }
    name_list.push(".shstrtab\0");
    for name in &name_list {
        names.push(shstr.len() as u32);
        shstr.extend_from_slice(name.as_bytes());
    }
    while body.len() % 8 != 0 {
        body.push(0);
    }
    let shstr_off = 64u64 + body.len() as u64;
    body.extend_from_slice(&shstr);
    // Section headers live AFTER the 64-byte ELF header + body.
    let shoff = 64u64 + body.len() as u64;
    let mut sh: Vec<u8> = Vec::new();
    let sh_header =
        |ty: u32, off: u64, size: u64, link: u32, name: u32, info: u32, entsize: u64| -> Vec<u8> {
            let mut s = Vec::new();
            s.extend_from_slice(&name.to_le_bytes());
            s.extend_from_slice(&ty.to_le_bytes());
            s.extend_from_slice(&0u64.to_le_bytes()); // flags
            s.extend_from_slice(&0u64.to_le_bytes()); // addr
            s.extend_from_slice(&off.to_le_bytes());
            s.extend_from_slice(&size.to_le_bytes());
            s.extend_from_slice(&link.to_le_bytes()); // sh_link
            s.extend_from_slice(&info.to_le_bytes()); // sh_info
            s.extend_from_slice(&8u64.to_le_bytes()); // addralign
            s.extend_from_slice(&entsize.to_le_bytes()); // entsize
            s
        };
    sh.extend_from_slice(&sh_header(0, 0, 0, 0, 0, 0, 0)); // NULL
    for (i, (ty, off, size, _)) in sections.iter().enumerate() {
        // The symtab is the first placed section: sh_link names the strtab
        // (sh index 2), sh_info = first non-local index, entsize 24.
        let (link, info, entsize) = if i == 0 {
            (2u32, 1u32, 24u64)
        } else {
            (0u32, 0u32, 0u64)
        };
        sh.extend_from_slice(&sh_header(*ty, *off, *size, link, names[i], info, entsize));
    }
    sh.extend_from_slice(&sh_header(
        3,
        shstr_off,
        shstr.len() as u64,
        0,
        names[name_list.len() - 1],
        0,
        0,
    ));

    let mut elf: Vec<u8> = Vec::new();
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]); // magic, 64-bit, LE, v1
    elf.extend_from_slice(&[0u8; 8]); // padding
    elf.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    elf.extend_from_slice(&0x3eu16.to_le_bytes()); // EM_X86_64
    elf.extend_from_slice(&1u32.to_le_bytes()); // version
    elf.extend_from_slice(&0u64.to_le_bytes()); // entry
    elf.extend_from_slice(&0u64.to_le_bytes()); // phoff
    elf.extend_from_slice(&shoff.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes()); // flags
    elf.extend_from_slice(&64u16.to_le_bytes()); // ehsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // phentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // phnum
    elf.extend_from_slice(&64u16.to_le_bytes()); // shentsize
    elf.extend_from_slice(&(shnum as u16).to_le_bytes());
    elf.extend_from_slice(&((shnum - 1) as u16).to_le_bytes()); // shstrndx = last
    assert_eq!(elf.len(), 64);
    elf.extend_from_slice(&body);
    elf.extend_from_slice(&sh);
    elf
}

/// Minimal structurally-valid ELF carrying a DWARF v2 `.debug_line`
/// unit — the OLDEST version the parser accepts, with a different
/// header (no max_ops_per_inst, no address_size/segment fields) and
/// v2-era file entries. Exercises the version-conditional paths the
/// v4/v5 seeds skip.
fn build_seed_v2_line_elf() -> Vec<u8> {
    let mut hdr = Vec::new();
    hdr.push(1); // min_inst_len (v2/v3: no max_ops_per_inst field)
    hdr.push(1); // default_is_stmt
    hdr.push(0xFB); // line_base (-5)
    hdr.push(14); // line_range
    hdr.push(13); // opcode_base
    hdr.extend_from_slice(&[0u8; 12]); // std_opcode_lengths
    hdr.push(0); // include_directories: empty
    // v2 file entry: name, dir, mtime, length; empty name ends the table.
    hdr.extend_from_slice(b"seed2.c ");
    hdr.push(0);
    hdr.push(0);
    hdr.push(0);
    hdr.push(0); // end of file_names

    let mut ops = Vec::new();
    ops.extend_from_slice(&[0x00, 0x09, 0x02]); // set_address
    ops.extend_from_slice(&0x2000u64.to_le_bytes());
    ops.extend_from_slice(&[0x03, 0x05]); // advance_line +5
    ops.extend_from_slice(&[0x02, 0x04]); // advance_pc +4 (min_inst × oper)
    ops.push(0x01); // copy → row
    ops.extend_from_slice(&[0x02, 0x04]); // advance_pc +4
    ops.push(0x01); // copy → row
    ops.extend_from_slice(&[0x00, 0x01, 0x01]); // end_sequence

    // v2 unit: unit_length, version, header_length, header, program.
    let unit_len = (2 + 4 + hdr.len() + ops.len()) as u32;
    let mut line: Vec<u8> = Vec::new();
    line.extend_from_slice(&unit_len.to_le_bytes());
    line.extend_from_slice(&2u16.to_le_bytes()); // version 2
    line.extend_from_slice(&(hdr.len() as u32).to_le_bytes());
    line.extend_from_slice(&hdr);
    line.extend_from_slice(&ops);

    // ELF skeleton: .debug_line only.
    let mut body: Vec<u8> = Vec::new();
    let mut sections: Vec<(u32, u64, u64)> = Vec::new();
    let place = |sections: &mut Vec<(u32, u64, u64)>, body: &mut Vec<u8>, ty: u32, data: &[u8]| {
        while body.len() % 8 != 0 {
            body.push(0);
        }
        let off = 64u64 + body.len() as u64;
        body.extend_from_slice(data);
        sections.push((ty, off, data.len() as u64));
    };
    let mut names: Vec<u8> = vec![0];
    let name_off = |names: &mut Vec<u8>, n: &str| -> u32 {
        let o = names.len() as u32;
        names.extend_from_slice(n.as_bytes());
        names.push(0);
        o
    };
    let n_line = name_off(&mut names, ".debug_line");
    let n_shstr = name_off(&mut names, ".shstrtab");
    place(&mut sections, &mut body, 1, &line);
    while body.len() % 8 != 0 {
        body.push(0);
    }
    let shstr_off = 64u64 + body.len() as u64;
    body.extend_from_slice(&names);
    let shoff = 64u64 + body.len() as u64;
    let mut sh: Vec<u8> = Vec::new();
    let sh_header = |ty: u32, off: u64, size: u64, name: u32| -> Vec<u8> {
        let mut x = Vec::new();
        x.extend_from_slice(&name.to_le_bytes());
        x.extend_from_slice(&ty.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        x.extend_from_slice(&off.to_le_bytes());
        x.extend_from_slice(&size.to_le_bytes());
        x.extend_from_slice(&0u32.to_le_bytes());
        x.extend_from_slice(&0u32.to_le_bytes());
        x.extend_from_slice(&8u64.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        x
    };
    let shnum = 2 + sections.len() as u64;
    sh.extend_from_slice(&sh_header(0, 0, 0, 0));
    for (i, (ty, off, size)) in sections.iter().enumerate() {
        let nm = [n_line][i];
        sh.extend_from_slice(&sh_header(*ty, *off, *size, nm));
    }
    sh.extend_from_slice(&sh_header(3, shstr_off, names.len() as u64, n_shstr));

    let mut elf: Vec<u8> = Vec::new();
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    elf.extend_from_slice(&[0u8; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&0x3eu16.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&shoff.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes());
    elf.extend_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&(shnum as u16).to_le_bytes());
    elf.extend_from_slice(&((shnum - 1) as u16).to_le_bytes());
    assert_eq!(elf.len(), 64);
    elf.extend_from_slice(&body);
    elf.extend_from_slice(&sh);
    elf
}

/// Minimal structurally-valid ELF carrying a DWARF v5 `.debug_line` unit
/// (format-described directory/file entries via `line_strp` + `udata`
/// forms, plus `.debug_line_str`/`.debug_str`) — the v5 parsing path that
/// real rustc binaries use, previously exercised only un-mutated.
fn build_seed_v5_line_elf() -> Vec<u8> {
    // String pools.
    let mut line_str: Vec<u8> = vec![0]; // offset 0 = empty
    let mut debug_str: Vec<u8> = vec![0];
    let dir_off = line_str.len() as u32;
    line_str.extend_from_slice(b"/usr/src\0");
    let file_off = line_str.len() as u32;
    line_str.extend_from_slice(b"seed5.c\0");
    // (debug_str carries a `strp`-form string in real units; this seed
    // uses line_strp for its entries, so debug_str stays minimal.)
    debug_str.extend_from_slice(b"strp-file.c\0");

    // v5 header + program.
    let mut hdr = Vec::new();
    hdr.push(1); // min_inst_len
    hdr.push(1); // max_ops_per_inst
    hdr.push(1); // default_is_stmt
    hdr.push(0xFB); // line_base (-5)
    hdr.push(14); // line_range
    hdr.push(13); // opcode_base
    hdr.extend_from_slice(&[0u8; 12]); // std_opcode_lengths
    // directory_entry_format: 1 pair — (DW_LNCT_path, DW_FORM_line_strp)
    hdr.push(1);
    uleb(1, &mut hdr);
    uleb(0x1f, &mut hdr);
    uleb(1, &mut hdr); // directories_count
    hdr.extend_from_slice(&dir_off.to_le_bytes());
    // file_name_entry_format: 2 pairs — (path, line_strp), (dir_index, udata)
    hdr.push(2);
    uleb(1, &mut hdr);
    uleb(0x1f, &mut hdr);
    uleb(2, &mut hdr);
    uleb(0x0f, &mut hdr);
    uleb(1, &mut hdr); // file_names_count
    hdr.extend_from_slice(&file_off.to_le_bytes());
    uleb(0, &mut hdr); // dir index 0

    let mut ops = Vec::new();
    ops.extend_from_slice(&[0x00, 0x09, 0x02]); // set_address
    ops.extend_from_slice(&0x1000u64.to_le_bytes());
    ops.extend_from_slice(&[0x03, 0x05]); // advance_line +5
    ops.extend_from_slice(&[0x02, 0x04]); // advance_pc +4
    ops.push(0x01); // copy → row
    ops.extend_from_slice(&[0x02, 0x04]); // advance_pc +4
    ops.push(0x01); // copy → row
    ops.extend_from_slice(&[0x00, 0x01, 0x01]); // end_sequence

    let unit_len = (2 + 1 + 1 + 4 + hdr.len() + ops.len()) as u32;
    let mut line: Vec<u8> = Vec::new();
    line.extend_from_slice(&unit_len.to_le_bytes());
    line.extend_from_slice(&5u16.to_le_bytes()); // version 5
    line.push(8); // address_size
    line.push(0); // segment_selector_size
    line.extend_from_slice(&(hdr.len() as u32).to_le_bytes()); // header_length
    line.extend_from_slice(&hdr);
    line.extend_from_slice(&ops);

    // ELF skeleton: .debug_line + .debug_line_str + .debug_str.
    let mut body: Vec<u8> = Vec::new();
    let mut sections: Vec<(u32, u64, u64)> = Vec::new();
    let place = |sections: &mut Vec<(u32, u64, u64)>, body: &mut Vec<u8>, ty: u32, data: &[u8]| {
        while body.len() % 8 != 0 {
            body.push(0);
        }
        let off = 64u64 + body.len() as u64;
        body.extend_from_slice(data);
        sections.push((ty, off, data.len() as u64));
    };
    let mut names: Vec<u8> = vec![0];
    let name_off = |names: &mut Vec<u8>, n: &str| -> u32 {
        let o = names.len() as u32;
        names.extend_from_slice(n.as_bytes());
        names.push(0);
        o
    };
    let n_line = name_off(&mut names, ".debug_line");
    let n_lstr = name_off(&mut names, ".debug_line_str");
    let n_dstr = name_off(&mut names, ".debug_str");
    let n_shstr = name_off(&mut names, ".shstrtab");
    place(&mut sections, &mut body, 1, &line);
    place(&mut sections, &mut body, 1, &line_str);
    place(&mut sections, &mut body, 1, &debug_str);
    while body.len() % 8 != 0 {
        body.push(0);
    }
    let shstr_off = 64u64 + body.len() as u64;
    body.extend_from_slice(&names);
    let shoff = 64u64 + body.len() as u64;
    let mut sh: Vec<u8> = Vec::new();
    let sh_header = |ty: u32, off: u64, size: u64, name: u32| -> Vec<u8> {
        let mut x = Vec::new();
        x.extend_from_slice(&name.to_le_bytes());
        x.extend_from_slice(&ty.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        x.extend_from_slice(&off.to_le_bytes());
        x.extend_from_slice(&size.to_le_bytes());
        x.extend_from_slice(&0u32.to_le_bytes());
        x.extend_from_slice(&0u32.to_le_bytes());
        x.extend_from_slice(&8u64.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        x
    };
    let shnum = 2 + sections.len() as u64;
    sh.extend_from_slice(&sh_header(0, 0, 0, 0));
    for (i, (ty, off, size)) in sections.iter().enumerate() {
        let nm = [n_line, n_lstr, n_dstr][i];
        sh.extend_from_slice(&sh_header(*ty, *off, *size, nm));
    }
    sh.extend_from_slice(&sh_header(3, shstr_off, names.len() as u64, n_shstr));

    let mut elf: Vec<u8> = Vec::new();
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    elf.extend_from_slice(&[0u8; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&0x3eu16.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&shoff.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes());
    elf.extend_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&(shnum as u16).to_le_bytes());
    elf.extend_from_slice(&((shnum - 1) as u16).to_le_bytes());
    assert_eq!(elf.len(), 64);
    elf.extend_from_slice(&body);
    elf.extend_from_slice(&sh);
    elf
}

/// Valid maps-text seed (same shape as /proc/<pid>/maps).
const MAPS_SEED: &str = "\
00400000-00452000 r-xp 00000000 08:01 1234567                    /usr/bin/workload
00452000-00453000 r--p 00052000 08:01 1234567                    /usr/bin/workload
7f0000000000-7f0000200000 r-xp 00000000 08:01 891011             /usr/lib/libsample.so.1
7f0000200000-7f0000210000 rw-p 00200000 08:01 891011             /usr/lib/libsample.so.1
7ffd00000000-7ffd00200000 rw-p 00000000 00:00 0                  [stack]
";

fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pf-fuzz-parsers-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Write `bytes` to the shared scratch file and run every file parser
/// against it. All parsers re-read from disk, so one write covers all.
fn poke_all_parsers(path: &std::path::Path) {
    let _ = elf_functions(path);
    let _ = inline_ranges(path);
    let _ = line_map(path);
}

#[test]
fn file_parsers_survive_mutations_of_a_valid_elf() {
    let dir = scratch_dir("mut");
    let path = dir.join("seed.elf");
    for version in [4u32, 5] {
        let base = build_seed_elf(version);
        std::fs::write(&path, &base).expect("write seed");
        // Sanity: the unmutated seed must actually parse for BOTH CU
        // versions (seed validity is part of the test — a dead seed
        // proves nothing).
        let funcs = elf_functions(&path).expect("seed symtab parses");
        assert_eq!(funcs.len(), 1, "seed has one STT_FUNC");
        assert_eq!(funcs[0].name, "outer");
        let inlines = inline_ranges(&path);
        eprintln!(
            "seed sanity: v{version} inlines={inlines:?} lines={:?}",
            line_map(&path).map(|m| m.rows())
        );
        assert!(
            !inlines.is_empty(),
            "v{version} debug_info yields inline ranges, got {inlines:?}"
        );
        // Multi-CU: CU2's cross-CU inline must resolve through table 1
        // and CU1's names — a second range at 0x3010..0x3018.
        assert_eq!(
            inlines.len(),
            2,
            "both CUs contribute an inline range (multi-CU + cross-CU ref)"
        );
        let cu2 = inlines
            .iter()
            .find(|r| r.low == 0x3010)
            .expect("CU2's cross-CU inline resolves");
        assert_eq!(cu2.chain, vec!["outer".to_string()], "cross-CU chain");
        assert!(line_map(&path).is_some(), "v{version} debug_line parses");

        for &seed in &SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..600 {
                let mutated = mutate(&mut rng, &base);
                std::fs::write(&path, &mutated).expect("write mutant");
                poke_all_parsers(&path);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_parsers_survive_mutations_of_a_valid_v5_line_table() {
    let dir = scratch_dir("v5");
    let path = dir.join("seed5.elf");
    let base = build_seed_v5_line_elf();
    std::fs::write(&path, &base).expect("write v5 seed");
    // Seed sanity: the v5 unit parses and the file resolves.
    let lm = line_map(&path).expect("v5 line table parses");
    // Same-line runs collapse (dedup) — the invariant is that the seeded
    // address resolves to the v5-declared file, via line_strp + dir idx.
    assert!(lm.rows() >= 1, "v5 seed yields rows: {}", lm.rows());
    // The v5 file entry resolves through line_strp + dir index: the row
    // at the seeded address names seed5.c via the /usr/src directory.
    for probe in [0x1000u64, 0x1004, 0x1008, 0x100C] {
        if let Some(loc) = lm.lookup(probe) {
            eprintln!(
                "probe {probe:#x} -> {}:{}, rows={}",
                loc.file,
                loc.line,
                lm.rows()
            );
        } else {
            eprintln!("probe {probe:#x} -> none, rows={}", lm.rows());
        }
    }
    let loc = lm
        .lookup(0x1000)
        .or_else(|| lm.lookup(0x1004))
        .expect("a row exists in the seeded range");
    assert!(
        loc.file.contains("seed5.c"),
        "v5 file resolves: {}",
        loc.file
    );

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..500 {
            let mutated = mutate(&mut rng, &base);
            std::fs::write(&path, &mutated).expect("write v5 mutant");
            let _ = line_map(&path); // None or a table — never a panic
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_parsers_survive_mutations_of_a_valid_v2_line_table() {
    let dir = scratch_dir("v2");
    let path = dir.join("seed2.elf");
    let base = build_seed_v2_line_elf();
    std::fs::write(&path, &base).expect("write v2 seed");
    // Seed sanity: the v2 unit parses and its seeded row resolves.
    let lm = line_map(&path).expect("v2 line table parses");
    assert!(lm.rows() >= 1, "v2 seed yields rows: {}", lm.rows());
    let loc = lm.lookup(0x2004).expect("a row exists in the v2 range");
    assert!(
        loc.file.contains("seed2.c"),
        "v2 file resolves: {}",
        loc.file
    );

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..500 {
            let mutated = mutate(&mut rng, &base);
            std::fs::write(&path, &mutated).expect("write v2 mutant");
            let _ = line_map(&path); // None or a table — never a panic
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_parsers_survive_mutations_of_a_valid_strx_cu() {
    // DWARF5 indexed strings: DIE names via DW_FORM_strx resolved through
    // .debug_str_offsets + DW_AT_str_offsets_base — the two-section
    // arithmetic that real producers emit and that previously resolved
    // to garbage (the offsets table was scanned for the string).
    let dir = scratch_dir("strx");
    let path = dir.join("strx.elf");
    let base = build_seed_elf_inner(5, true);
    std::fs::write(&path, &base).expect("write strx seed");
    let inlines = inline_ranges(&path);
    assert!(
        !inlines.is_empty(),
        "strx CU yields inline ranges, got {inlines:?}"
    );
    // The strx-resolved names must appear in the chain (the origin
    // points at the subprogram whose name came from strx index 0).
    let chain = inlines[0].chain.join("|");
    assert!(chain.contains("outer"), "strx names resolve: {chain}");

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..500 {
            let mutated = mutate(&mut rng, &base);
            std::fs::write(&path, &mutated).expect("write strx mutant");
            let _ = inline_ranges(&path); // None paths — never a panic
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_parsers_survive_random_byte_soups() {
    let dir = scratch_dir("soup");
    let path = dir.join("soup.elf");
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..400 * fuzz_scale() {
            let len = 1 + rng.below(1024);
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            std::fs::write(&path, &bytes).expect("write soup");
            poke_all_parsers(&path);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_parsers_survive_truncated_real_sections() {
    // Truncations of the seed at EVERY 64-byte boundary: header offsets
    // point past EOF is the most common real-world corruption shape.
    // Both CU versions are swept (v5's header field order differs).
    let dir = scratch_dir("trunc");
    let path = dir.join("trunc.elf");
    for version in [4u32, 5] {
        let base = build_seed_elf(version);
        for end in (1..base.len()).step_by(64) {
            std::fs::write(&path, &base[..end]).expect("write truncation");
            poke_all_parsers(&path);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parse_ring_survives_hostile_regions() {
    // A valid sample region, mutated everywhere: lying sizes, truncated
    // records, absurd callchain counts, LOST-record soups.
    let mut base: Vec<u8> = Vec::new();
    for i in 0..32u64 {
        base.extend_from_slice(&9u32.to_le_bytes()); // PERF_RECORD_SAMPLE
        base.extend_from_slice(&0u16.to_le_bytes());
        let size = 8u16 + 8 + 8 + 8 + 6 * 8;
        base.extend_from_slice(&size.to_le_bytes());
        base.extend_from_slice(&0x400000u64.to_le_bytes());
        base.extend_from_slice(&4242u32.to_le_bytes());
        base.extend_from_slice(&(i as u32).to_le_bytes());
        base.extend_from_slice(&(i * 1_000_000).to_le_bytes());
        base.extend_from_slice(&6u64.to_le_bytes());
        for d in 0..6u64 {
            base.extend_from_slice(&(0x1000 + d * 0x10 + i * 0x100).to_le_bytes());
        }
    }
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..8_000 * fuzz_scale() {
            let buf = mutate(&mut rng, &base);
            let mut out = Vec::new();
            let mut lost = 0u64;
            let mut kern = 0u64;
            parse_ring(&buf, &mut out, &mut lost, &mut kern, false);
            // Boundedness: a sample never carries more frames than the cap.
            for s in &out {
                assert!(s.ip_count <= 64, "callchain cap holds under mutation");
            }
            let _ = kern;
        }
    }
}

#[test]
fn parse_ring_survives_random_soups_and_lying_headers() {
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..6_000 * fuzz_scale() {
            let len = 1 + rng.below(512);
            let buf: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            let mut out = Vec::new();
            let mut lost = 0u64;
            let mut kern = 0u64;
            parse_ring(&buf, &mut out, &mut lost, &mut kern, false);
        }
    }
}

#[test]
fn parse_maps_survives_hostile_text() {
    let mut rng = Rng::new(42);
    // Mutations of a valid maps document.
    let base = MAPS_SEED.as_bytes();
    for _ in 0..20_000 * fuzz_scale() {
        let mutated = mutate(&mut rng, base);
        if let Ok(text) = String::from_utf8(mutated) {
            let entries = parse_maps(&text);
            // Parser invariants (NOT kernel plausibility — hostile text may
            // legitimately parse to backwards ranges, which downstream
            // consumers handle: a backwards range matches no IP):
            // every kept entry has a non-empty pathname and the entry
            // count is bounded by the line count.
            assert!(
                entries.len() <= text.lines().count(),
                "entry count bounded by lines"
            );
            for e in &entries {
                assert!(!e.pathname.is_empty(), "kept entries carry a pathname");
            }
        }
    }
    // Pure soups.
    for _ in 0..20_000 * fuzz_scale() {
        let len = 1 + rng.below(256);
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        if let Ok(text) = String::from_utf8(bytes) {
            let _ = parse_maps(&text);
        }
    }
}

#[test]
fn resolve_ip_chains_survives_hostile_map_sets() {
    // Overlapping, unsorted, zero-length and backwards maps against random
    // IPs: the resolver must never panic and never resolve into a range
    // that does not contain the IP.
    let mut rng = Rng::new(7);
    for _ in 0..3_000 * fuzz_scale() {
        let n = rng.below(16);
        let mut maps: Vec<MapsEntry> = Vec::new();
        for i in 0..n {
            let start = rng.next_u64() & 0x0000_ffff_ffff_f000;
            let len = (rng.next_u64() & 0xffff) * 0x10;
            maps.push(MapsEntry {
                start,
                end: start.saturating_add(len),
                pathname: if i % 3 == 0 {
                    String::new()
                } else {
                    format!("/nonexistent/lib{i}.so")
                },
            });
        }
        let ips: Vec<u64> = (0..16).map(|_| rng.next_u64()).collect();
        let mut symcache = std::collections::HashMap::new();
        let out = resolve_ip_chains(&ips, &maps, &mut symcache);
        for (ip, chain) in ips.iter().zip(out.iter()) {
            if let Some(chain) = chain {
                for fr in chain {
                    let e = maps
                        .iter()
                        .find(|m| *ip >= m.start && *ip < m.end)
                        .expect("resolved chain must come from a containing map");
                    assert_eq!(
                        fr.module, e.pathname,
                        "resolution must name the containing map"
                    );
                }
            }
        }
    }
}
