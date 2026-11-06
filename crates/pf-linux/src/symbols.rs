//! Linux user-space symbolization (roadmap P10 bounded slice: PF-1004
//! module mapping + PF-90x-adjacent symbol resolution for sampled IPs).
//!
//! Scope (honest):
//! - `/proc/<pid>/maps` parsing → module ranges;
//! - ELF symtab (` .symtab` + `.dynsym`) function symbols → name table;
//! - IP → (module, symbol, offset) resolution.
//!
//! DWARF inline-frame expansion (PF-1005): sampled IPs inside inlined
//! code expand to their inlinee chain (innermost-first) via the minimal
//! `.debug_info` parser in [`crate::dwarf`]. Still out of scope: kernel
//! symbols (filtered upstream) and stripped-binary recovery via debug
//! files — a stripped binary resolves to `module+offset` honestly, never
//! an invented name.

// ELF parsing: bounded reader arithmetic (offsets validated vs file len).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use pf_types::{ErrorCode, PfError};

/// One /proc/<pid>/maps range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapsEntry {
    pub start: u64,
    pub end: u64,
    pub pathname: String,
}

/// Parse /proc/<pid>/maps lines: `start-end perms offset dev inode pathname`.
/// Anonymous ranges (no pathname) are skipped. Bounded line length.
pub fn parse_maps(text: &str) -> Vec<MapsEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.len() > 4096 {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(range), Some(_perms), Some(_off), Some(_dev), Some(_inode)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        // Pathname: everything after the 5th field (may contain spaces).
        let rest = line
            .splitn(6, ' ')
            .nth(5)
            .unwrap_or_default()
            .trim()
            .to_string();
        if rest.is_empty() || rest.starts_with('[') {
            continue; // anonymous / [stack] / [heap] / [vdso]
        }
        out.push(MapsEntry {
            start,
            end,
            pathname: rest,
        });
    }
    out
}

/// Read /proc/<pid>/maps.
pub fn read_maps(pid: u32) -> Result<Vec<MapsEntry>, PfError> {
    let text = fs::read_to_string(format!("/proc/{pid}/maps")).map_err(|e| {
        PfError::new(
            ErrorCode::PermissionDenied,
            format!("/proc/{pid}/maps: {e}"),
        )
    })?;
    Ok(parse_maps(&text))
}

/// One STT_FUNC symbol from an ELF symtab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfFunc {
    pub value: u64,
    pub size: u64,
    pub name: String,
}

/// Per-module resolution data, cached by path: the symtab function table,
/// the DWARF inline-range table (empty without debug info) and the
/// `.debug_line` source map (None without line info) — degradation is
/// graceful, not silent. `symbols_file` records the file the data came
/// from (the binary itself, or its split-debug companion).
#[derive(Clone, Debug, Default)]
pub struct ModuleSymbols {
    pub funcs: Vec<ElfFunc>,
    pub inline: Vec<crate::dwarf::InlineRange>,
    /// Bucketed lookup over [`ModuleSymbols::inline`] (built at load).
    pub inline_index: InlineIndex,
    pub lines: Option<crate::line::LineMap>,
    pub symbols_file: Option<String>,
}

/// Bucketed lookup over the module's inline ranges: ranges are indexed
/// into 64 KiB address-space buckets so a per-IP probe touches only the
/// candidates that can contain it. A real dev-profile library carries
/// tens of thousands of ranges, and the previous per-IP linear scan over
/// all of them was the last quadratic in symbolization. Ranges wider
/// than the bucket cap live on a linear overflow list — honest
/// boundedness instead of a pretend O(1); the lookup result is EXACTLY
/// the old scan's (deepest containing range, ties → the range earliest
/// in the original `inline` order, enforced by the explicit tie-break).
#[derive(Clone, Debug, Default)]
pub struct InlineIndex {
    base: u64,
    bucket_size: u64,
    /// Range indices registered per 64 KiB bucket of the module span.
    buckets: Vec<Vec<u32>>,
    /// Range indices too wide to bucket; probed for every IP.
    overflow: Vec<u32>,
}

impl InlineIndex {
    const BUCKET_SIZE: u64 = 64 * 1024;
    /// A range may span at most this many buckets (16k × 64 KiB = 1 GiB)
    /// before it goes to the overflow list.
    const BUCKET_CAP: u32 = 16 * 1024;
    /// The bucket table itself is capped (1M buckets = 64 GiB of span):
    /// hostile headers claiming absurd spans must not allocate the table.
    const TABLE_CAP: u64 = 1_048_576;

    /// Build the index for `inline` (order-preserving: bucket entries
    /// reference the original vec positions).
    // Bucket arithmetic is bounded by the table cap and range bounds.
    #[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
    pub fn of(inline: &[crate::dwarf::InlineRange]) -> InlineIndex {
        if inline.is_empty() {
            return InlineIndex::default();
        }
        let base = inline.iter().map(|r| r.low).min().unwrap_or(0);
        let top = inline.iter().map(|r| r.high).max().unwrap_or(base);
        let nbuckets = top.saturating_sub(base) / Self::BUCKET_SIZE + 1;
        if nbuckets > Self::TABLE_CAP {
            // Overflow-only mode: correct for every IP, linear per probe.
            return InlineIndex {
                base,
                bucket_size: Self::BUCKET_SIZE,
                buckets: Vec::new(),
                overflow: (0..inline.len() as u32).collect(),
            };
        }
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); nbuckets as usize];
        let mut overflow = Vec::new();
        for (i, r) in inline.iter().enumerate() {
            if r.high <= r.low {
                continue; // empty range: never matches, never registered
            }
            let b0 = (r.low.saturating_sub(base) / Self::BUCKET_SIZE) as usize;
            let b1 = ((r.high - 1).saturating_sub(base) / Self::BUCKET_SIZE) as usize;
            if (b1 - b0) as u32 >= Self::BUCKET_CAP {
                overflow.push(i as u32);
                continue;
            }
            for b in buckets[b0..=b1].iter_mut() {
                b.push(i as u32);
            }
        }
        InlineIndex {
            base,
            bucket_size: Self::BUCKET_SIZE,
            buckets,
            overflow,
        }
    }

    /// Deepest range containing `rel`, ties broken toward the earliest
    /// original index — the exact result of the previous linear scan.
    // Probe arithmetic is bounded by the bucket count and range bounds.
    #[allow(clippy::arithmetic_side_effects)]
    fn deepest<'a>(
        &self,
        inline: &'a [crate::dwarf::InlineRange],
        rel: u64,
    ) -> Option<&'a crate::dwarf::InlineRange> {
        let mut best: Option<(u32, &'a crate::dwarf::InlineRange)> = None;
        let probe = |i: u32, best: &mut Option<(u32, &'a crate::dwarf::InlineRange)>| {
            let r = &inline[i as usize];
            if rel < r.low || rel >= r.high {
                return;
            }
            let better = match best {
                None => true,
                Some((bi, br)) => {
                    r.chain.len() > br.chain.len() || (r.chain.len() == br.chain.len() && i < *bi)
                }
            };
            if better {
                *best = Some((i, r));
            }
        };
        if !self.buckets.is_empty() && rel >= self.base {
            let b = ((rel - self.base) / self.bucket_size) as usize;
            if let Some(bucket) = self.buckets.get(b) {
                for &i in bucket {
                    probe(i, &mut best);
                }
            }
        }
        for &i in &self.overflow {
            probe(i, &mut best);
        }
        best.map(|(_, r)| r)
    }
}

fn load_module_symbols(path: &Path) -> ModuleSymbols {
    // Direct parse: the binary carries its own symtab/DWARF.
    let funcs = elf_functions(path).unwrap_or_default();
    let inline = crate::dwarf::inline_ranges(path);
    let lines = crate::line::line_map(path);
    // Recovery adds value only when debug info is missing; a binary with
    // symbols AND sources is already complete.
    if !funcs.is_empty() && lines.is_some() && !inline.is_empty() {
        let inline_index = InlineIndex::of(&inline);
        return ModuleSymbols {
            inline,
            inline_index,
            lines,
            symbols_file: Some(path.display().to_string()),
            funcs,
        };
    }
    // Split-debug recovery (PF-1005): build-id first (note equality
    // verified), then .gnu_debuglink (CRC verified). A failed or mismatched
    // recovery degrades to whatever the binary itself offered — never to a
    // guessed name.
    if let Some(recovered) = recover_by_build_id(path) {
        return recovered;
    }
    if let Some(recovered) = recover_by_debuglink(path) {
        return recovered;
    }
    let inline_index = InlineIndex::of(&inline);
    ModuleSymbols {
        inline,
        inline_index,
        lines,
        symbols_file: Some(path.display().to_string()),
        funcs,
    }
}

/// Follow `.note.gnu.build-id`: sibling `.build-id/xx/rest.debug` then the
/// system `/usr/lib/debug/.build-id/…`; a candidate's own build-id (when
/// present) must equal the binary's.
fn recover_by_build_id(path: &Path) -> Option<ModuleSymbols> {
    let id = crate::elfio::build_id(path)?;
    let mut recovered = None;
    for candidate in crate::elfio::build_id_candidates(path, &id) {
        // A candidate carrying a build-id must match; note-less candidates
        // still need a usable symtab (checked by parse_recovered).
        if let Some(candidate_id) = crate::elfio::build_id(&candidate) {
            if candidate_id != id {
                continue; // mismatched debug file: never trusted
            }
        }
        if let Some(m) = parse_recovered(&candidate) {
            recovered = Some(m);
            break;
        }
    }
    recovered
}

/// Follow `.gnu_debuglink` (sibling or `.debug/`), CRC32-verified.
fn recover_by_debuglink(path: &Path) -> Option<ModuleSymbols> {
    let (link_name, crc) = crate::elfio::debuglink(path)?;
    let parent = path.parent();
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = parent {
        candidates.push(p.join(&link_name));
        candidates.push(p.join(".debug").join(&link_name));
    }
    for candidate in candidates {
        if std::fs::read(&candidate)
            .map(|b| crate::elfio::crc32(&b) == crc)
            .unwrap_or(false)
        {
            if let Some(m) = parse_recovered(&candidate) {
                return Some(m);
            }
        }
    }
    None
}

/// Parse a candidate debug file; usable only when it offers something the
/// stripped binary could not provide on its own.
fn parse_recovered(candidate: &Path) -> Option<ModuleSymbols> {
    let funcs = elf_functions(candidate).unwrap_or_default();
    if funcs.is_empty() {
        return None;
    }
    let inline = crate::dwarf::inline_ranges(candidate);
    let inline_index = InlineIndex::of(&inline);
    Some(ModuleSymbols {
        inline,
        inline_index,
        lines: crate::line::line_map(candidate),
        symbols_file: Some(candidate.display().to_string()),
        funcs,
    })
}

/// Resolved symbol for one sampled IP. `file`/`line` come from the
/// `.debug_line` map when the binary carries line info (the location of
/// the instruction — for inlined code, the inlinee's source line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFrame {
    pub module: String,
    pub symbol: String,
    pub offset: u64,
    pub file: Option<String>,
    pub line: Option<u32>,
}

/// Minimal ELF64 section/symbol table reader (zero-dep). Reads the file,
/// finds `.symtab` (fallback `.dynsym`) + its string table, returns
/// STT_FUNC symbols sorted by value. Bounded: refuses files > 512 MiB.
pub fn elf_functions(path: &Path) -> Result<Vec<ElfFunc>, PfError> {
    let bytes = fs::read(path)
        .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("{}: {e}", path.display())))?;
    if bytes.len() > 512 * 1024 * 1024 {
        return Err(PfError::new(
            ErrorCode::ResourceExhausted,
            format!("{} exceeds the 512 MiB symbolization cap", path.display()),
        ));
    }
    // ELF64 magic + class check.
    if bytes.len() < 64 || bytes[0..4] != [0x7f, b'E', b'L', b'F'] || bytes[4] != 2 {
        return Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            format!("{} is not an ELF64 object", path.display()),
        ));
    }
    let rd_u16 = |off: usize| -> u64 {
        u64::from(u16::from_le_bytes([
            bytes[off],
            bytes.get(off + 1).copied().unwrap_or(0),
        ]))
    };
    let rd_u32 = |off: usize| -> u64 {
        u64::from(u32::from_le_bytes([
            bytes[off],
            bytes.get(off + 1).copied().unwrap_or(0),
            bytes.get(off + 2).copied().unwrap_or(0),
            bytes.get(off + 3).copied().unwrap_or(0),
        ]))
    };
    let rd_u64 = |off: usize| -> u64 {
        u64::from_le_bytes([
            bytes[off],
            bytes.get(off + 1).copied().unwrap_or(0),
            bytes.get(off + 2).copied().unwrap_or(0),
            bytes.get(off + 3).copied().unwrap_or(0),
            bytes.get(off + 4).copied().unwrap_or(0),
            bytes.get(off + 5).copied().unwrap_or(0),
            bytes.get(off + 6).copied().unwrap_or(0),
            bytes.get(off + 7).copied().unwrap_or(0),
        ])
    };

    let shoff = rd_u64(0x28) as usize; // e_shoff
    let shentsize = rd_u16(0x3A) as usize; // e_shentsize
    let shnum = rd_u16(0x3C) as usize; // e_shnum
    if shoff == 0 || shentsize < 64 || shnum == 0 || shoff + shnum * shentsize > bytes.len() {
        return Err(PfError::new(
            ErrorCode::StorageIo,
            format!("{} has an unusable section header table", path.display()),
        ));
    }

    // Section headers: find .symtab/.dynsym and their linked strtabs.
    let sh = |i: usize| -> (u32, u64, u64) {
        let off = shoff + i * shentsize;
        (
            // SAFETY: shoff+shentsize bounds were validated above.
            u32::from_le_bytes([
                bytes[off + 4],
                bytes[off + 5],
                bytes[off + 6],
                bytes[off + 7],
            ]),
            u64::from_le_bytes([
                bytes[off + 24],
                bytes[off + 25],
                bytes[off + 26],
                bytes[off + 27],
                bytes[off + 28],
                bytes[off + 29],
                bytes[off + 30],
                bytes[off + 31],
            ]),
            u64::from_le_bytes([
                bytes[off + 32],
                bytes[off + 33],
                bytes[off + 34],
                bytes[off + 35],
                bytes[off + 36],
                bytes[off + 37],
                bytes[off + 38],
                bytes[off + 39],
            ]),
        )
    };
    let cstr = |table_off: usize, table_size: usize, name_off: usize| -> String {
        let start = table_off + name_off;
        if start >= table_off + table_size || start >= bytes.len() {
            return String::new();
        }
        let end = bytes[start..(table_off + table_size).min(bytes.len())]
            .iter()
            .position(|&b| b == 0)
            .map(|p| start + p)
            .unwrap_or(start);
        String::from_utf8_lossy(&bytes[start..end]).into_owned()
    };

    // Single pass over section headers: find the symbol table (.symtab
    // preferred, .dynsym fallback) and remember its sh_link (the strtab).
    let mut sym: Option<(usize, usize, usize)> = None; // (off, size, strtab_index)
    let mut dynsym: Option<(usize, usize, usize)> = None;
    let mut strtabs: HashMap<usize, (usize, usize)> = HashMap::new(); // index -> (off, size)
    for i in 0..shnum {
        let (sh_type, off, size) = sh(i);
        let (off, size) = (off as usize, size as usize);
        if off + size > bytes.len() {
            continue; // malformed section: bounded skip
        }
        // SHT_SYMTAB=2, SHT_DYNSYM=6, SHT_STRTAB=3
        if sh_type == 3 {
            strtabs.insert(i, (off, size));
        }
        let link = rd_u32(shoff + i * shentsize + 40) as usize; // sh_link
        match sh_type {
            2 => sym = Some((off, size, link)),
            6 if sym.is_none() => dynsym = Some((off, size, link)),
            _ => {}
        }
    }
    // Prefer .symtab (full symbols); fall back to .dynsym (exported only).
    let Some((sym_off, sym_size, str_index)) = sym.or(dynsym) else {
        return Ok(Vec::new()); // stripped without exports: honest emptiness
    };
    let (str_off, str_size) = strtabs.get(&str_index).copied().unwrap_or((0, 0));

    let entry_size = 24usize; // Elf64_Sym
    let count = if sym_size >= entry_size {
        sym_size / entry_size
    } else {
        0
    };
    let mut out = Vec::with_capacity(count.min(1_000_000));
    for i in 0..count {
        let base = sym_off + i * entry_size;
        if base + entry_size > bytes.len() {
            break;
        }
        let st_name = rd_u32(base) as usize;
        let st_info = bytes[base + 4];
        let st_value = rd_u64(base + 8);
        let st_size = rd_u64(base + 16);
        // STT_FUNC = (st_info & 0xf) == 2
        if st_info & 0xf != 2 || st_value == 0 {
            continue;
        }
        let name = cstr(str_off, str_size, st_name);
        if name.is_empty() {
            continue;
        }
        out.push(ElfFunc {
            value: st_value,
            size: st_size,
            name,
        });
    }
    out.sort_by_key(|f| f.value);
    Ok(out)
}

/// Resolve a batch of user-space IPs against the module map + per-module
/// symbol tables. Results are per-input (None = unresolved, kept honest).
/// Modules are cached by path (parsed once per module).
pub fn resolve_ips(
    ips: &[u64],
    maps: &[MapsEntry],
    symcache: &mut HashMap<String, ModuleSymbols>,
) -> Vec<Option<ResolvedFrame>> {
    resolve_ip_chains(ips, maps, symcache)
        .into_iter()
        .map(|c| c.and_then(|chain| chain.into_iter().next_back()))
        .collect()
}

/// Resolve IPs to their FULL call-site chains: when the IP sits inside
/// inlined code (DWARF `.debug_info`), the chain is
/// `[inlinee innermost, …, enclosing symbol]`; otherwise it is the single
/// enclosing symbol. Chain entries share the module and the IP's
/// module-relative offset.
pub fn resolve_ip_chains(
    ips: &[u64],
    maps: &[MapsEntry],
    symcache: &mut HashMap<String, ModuleSymbols>,
) -> Vec<Option<Vec<ResolvedFrame>>> {
    // Load base per module: the LOWEST segment start for that pathname.
    // ELF symbol values (st_value) are relative to the load base — for PIE
    // binaries with separate r--/r-x segments, subtracting the text
    // segment's start alone would short every lookup by the segment offset.
    let mut load_bases: HashMap<String, u64> = HashMap::new();
    for m in maps {
        let e = load_bases.entry(m.pathname.clone()).or_insert(m.start);
        *e = (*e).min(m.start);
    }
    let mut out = Vec::with_capacity(ips.len());
    // /proc/<pid>/maps is disjoint and sorted by start, so the per-IP
    // module lookup can binary-search. Synthetic callers may pass
    // overlapping or unsorted maps: the fast path is guarded by an O(n)
    // disjointness check with the original first-match linear scan as the
    // fallback — identical results either way.
    let disjoint_sorted = maps.windows(2).all(|w| w[0].end <= w[1].start);
    for &ip in ips {
        let entry = if disjoint_sorted {
            // partition_point: first map whose start is > ip; the candidate
            // is the one before it. Disjointness means no earlier map can
            // contain ip either (its end <= candidate.start <= ip).
            let idx = maps.partition_point(|m| m.start <= ip);
            if idx > 0 && ip < maps[idx - 1].end {
                Some(&maps[idx - 1])
            } else {
                None
            }
        } else {
            maps.iter().find(|m| ip >= m.start && ip < m.end)
        };
        let Some(entry) = entry else {
            out.push(None);
            continue;
        };
        let load_base = load_bases
            .get(&entry.pathname)
            .copied()
            .unwrap_or(entry.start);
        let rel = ip - load_base;
        let syms = if let Some(cached) = symcache.get(&entry.pathname) {
            cached
        } else {
            let loaded = load_module_symbols(Path::new(&entry.pathname));
            symcache.insert(entry.pathname.clone(), loaded);
            symcache.get(&entry.pathname).unwrap()
        };
        // The symbol containing rel: the last func with value <= rel.
        let funcs = &syms.funcs;
        let idx = funcs.partition_point(|f| f.value <= rel);
        let symbol = if idx > 0 {
            let f = &funcs[idx - 1];
            // Inside the function when size is known and nonzero, or when
            // size is unknown (0) accept the nearest preceding symbol.
            let within = f.size == 0 || rel < f.value + f.size;
            if within {
                f.name.clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let module = entry.pathname.clone();
        // DWARF inline expansion (PF-1005): the deepest containing range
        // wins; its chain is already innermost-first. Absent debug info
        // leaves the single enclosing frame; a stripped binary keeps its
        // honest empty-symbol frame (consumers render module+offset).
        let mut chain = Vec::new();
        // Bucketed probe (deepest containing range, ties → earliest
        // original index — identical to the previous full linear scan).
        let best = syms.inline_index.deepest(&syms.inline, rel);
        // Source location of the instruction (shared by the whole chain —
        // it is the location of the sampled PC, i.e. the innermost frame's
        // code, which is what the line table describes).
        let loc = syms.lines.as_ref().and_then(|m| m.lookup(rel));
        if let Some(r) = best {
            chain.extend(r.chain.iter().map(|name| ResolvedFrame {
                module: module.clone(),
                symbol: name.clone(),
                offset: rel,
                file: loc.as_ref().map(|l| l.file.clone()),
                line: loc.as_ref().map(|l| l.line),
            }));
        }
        chain.push(ResolvedFrame {
            module,
            symbol,
            offset: rel,
            file: loc.as_ref().map(|l| l.file.clone()),
            line: loc.as_ref().map(|l| l.line),
        });
        out.push(Some(chain));
    }
    out
}
