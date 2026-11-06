//! Seeded-mutation fuzz tests for the Windows binary parsers that face
//! files from disk (PF-1202 extension: PE identity incl. the debug
//! directory + RSDS CodeView blob, and MSF superblock/stream-directory
//! PDB matching). Dependency-free, same method as the pf-types/pf-linux
//! suites: a deterministic xorshift PRNG generates byte soups and
//! mutations of structurally-valid seeds; the property under test is
//! panic-freedom and bounded termination — every input either parses,
//! yields an honest `None`/`false`, or a typed error, never a panic and
//! never an unbounded loop.

// The parsers live in Windows-gated modules; the suite vanishes elsewhere.
#![cfg(windows)]
#![allow(clippy::arithmetic_side_effects)]

use std::path::PathBuf;

use pf_windows::modules::{PdbRef, pdb_matches, read_identity, resolve_pdb};

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
                if out.len() < 4096 {
                    let s = rng.below(out.len());
                    let e = (s + 1 + rng.below(64)).min(out.len());
                    let dup: Vec<u8> = out[s..e].to_vec();
                    out.extend_from_slice(&dup);
                }
            }
            _ => {
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

const SEED_GUID: [u8; 16] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
];
const SEED_AGE: u32 = 7;

/// Minimal PE32+ with one section carrying a debug directory whose single
/// entry is an RSDS CodeView blob referencing `seed.pdb`.
fn build_seed_pe() -> Vec<u8> {
    let mut pe: Vec<u8> = Vec::new();
    // DOS header: MZ + e_lfanew → PE at 0x80.
    pe.extend_from_slice(b"MZ");
    pe.extend_from_slice(&[0u8; 0x3A]);
    pe.extend_from_slice(&0x80u32.to_le_bytes());
    assert_eq!(pe.len(), 0x40);
    pe.extend_from_slice(&[0u8; 0x40]); // DOS stub space → PE at 0x80
    assert_eq!(pe.len(), 0x80);

    // "PE\0\0" + COFF (20 bytes) + optional PE32+ (240 bytes) + 1 section.
    pe.extend_from_slice(b"PE\0\0");
    pe.extend_from_slice(&0x8664u16.to_le_bytes()); // machine
    pe.extend_from_slice(&1u16.to_le_bytes()); // num sections
    pe.extend_from_slice(&0x67000000u32.to_le_bytes()); // timestamp
    pe.extend_from_slice(&[0u8; 8]); // symtab ptr/count
    pe.extend_from_slice(&240u16.to_le_bytes()); // SizeOfOptionalHeader
    pe.extend_from_slice(&0x22u16.to_le_bytes()); // characteristics
    let optional_off = pe.len();
    assert_eq!(optional_off, 0x98);
    pe.extend_from_slice(&0x20bu16.to_le_bytes()); // PE32+ magic
    pe.extend_from_slice(&[0u8; 62]); // rest of standard fields → +64
    pe.extend_from_slice(&0u32.to_le_bytes()); // CheckSum (+64)
    pe.extend_from_slice(&[0u8; 36]); // subsystem/dll/stacks/heaps/loader → +104
    pe.extend_from_slice(&16u64.to_le_bytes()); // NumberOfRvaAnds (+104)
    assert_eq!(pe.len() - optional_off, 112);
    // DataDirectory: 16 entries; entry 6 = debug directory.
    for i in 0..16u32 {
        if i == 6 {
            pe.extend_from_slice(&0x1000u32.to_le_bytes()); // RVA (in .text)
            pe.extend_from_slice(&28u32.to_le_bytes()); // size: one entry
        } else {
            pe.extend_from_slice(&[0u8; 8]);
        }
    }
    assert_eq!(pe.len() - optional_off, 240); // 112 std + 16×8 dirs
    // Section table (40 bytes): .text @ RVA 0x1000, raw at 0x400.
    let sect_off = pe.len();
    pe.extend_from_slice(b".text\0\0\0");
    pe.extend_from_slice(&0x400u32.to_le_bytes()); // VirtualSize
    pe.extend_from_slice(&0x1000u32.to_le_bytes()); // VirtualAddress
    pe.extend_from_slice(&0x400u32.to_le_bytes()); // SizeOfRawData
    pe.extend_from_slice(&0x400u32.to_le_bytes()); // PointerToRawData
    pe.extend_from_slice(&[0u8; 12]); // relocs/linenums/nums
    pe.extend_from_slice(&0x60000020u32.to_le_bytes()); // characteristics
    assert_eq!(pe.len() - sect_off, 40);

    // Body at 0x400: debug directory entry + CodeView blob.
    while pe.len() < 0x400 {
        pe.push(0);
    }
    // Debug directory entry (28 bytes): Characteristics, TimeDateStamp,
    // Major/MinorVersion, Type, SizeOfData, AddressOfRawData, PointerToRawData.
    let cv_rva: u32 = 0x1000 + 0x40; // blob RVA (0x440 file)
    pe.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    pe.extend_from_slice(&0x67000000u32.to_le_bytes()); // timestamp
    pe.extend_from_slice(&0u16.to_le_bytes()); // major
    pe.extend_from_slice(&0u16.to_le_bytes()); // minor
    pe.extend_from_slice(&2u32.to_le_bytes()); // type = CODEVIEW
    pe.extend_from_slice(&((24 + 10) as u32).to_le_bytes()); // size of data
    pe.extend_from_slice(&cv_rva.to_le_bytes()); // AddressOfRawData (RVA)
    pe.extend_from_slice(&0x440u32.to_le_bytes()); // PointerToRawData (file)
    assert_eq!(pe.len(), 0x41C);
    while pe.len() < 0x440 {
        pe.push(0);
    }
    // RSDS CodeView: magic + guid + age + path.
    pe.extend_from_slice(b"RSDS");
    pe.extend_from_slice(&SEED_GUID);
    pe.extend_from_slice(&SEED_AGE.to_le_bytes());
    pe.extend_from_slice(b"seed.pdb\0");
    pe
}

/// Minimal MSF 7.00 PDB: superblock (block 0), block map (block 1), info
/// stream content (block 2: version, signature, age, guid), stream
/// directory (block 3): 2 streams, sizes [0, 32], stream 1 → block 2.
fn build_seed_pdb() -> Vec<u8> {
    let block = 512usize;
    let mut pdb = vec![0u8; block * 4];
    pdb[0..26].copy_from_slice(b"Microsoft C/C++ MSF 7.00\r\n");
    pdb[0x20..0x24].copy_from_slice(&u32::try_from(block).unwrap_or(0).to_le_bytes()); // BlockSize
    pdb[0x24..0x28].copy_from_slice(&1u32.to_le_bytes()); // FreeBlockMapBlock
    pdb[0x28..0x2C].copy_from_slice(&4u32.to_le_bytes()); // NumBlocks
    pdb[0x2C..0x30].copy_from_slice(&16u32.to_le_bytes()); // NumDirectoryBytes
    pdb[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // Unknown0
    pdb[0x34..0x38].copy_from_slice(&1u32.to_le_bytes()); // BlockMapAddr
    // Block map (block 1): directory block index = 3.
    pdb[block..block + 4].copy_from_slice(&3u32.to_le_bytes());
    // Directory (block 3): num_streams=2, sizes=[0, 32], stream1 → block 2.
    let dir: [u8; 16] = [
        2, 0, 0, 0, // num streams
        0, 0, 0, 0, // stream 0 size
        32, 0, 0, 0, // stream 1 size
        2, 0, 0, 0, // stream 1 first block
    ];
    pdb[block * 3..block * 3 + 16].copy_from_slice(&dir);
    // Info stream (block 2): version, signature, age, guid.
    pdb[block * 2..block * 2 + 4].copy_from_slice(&20_000_404u32.to_le_bytes());
    pdb[block * 2 + 4..block * 2 + 8].copy_from_slice(&0x67000000u32.to_le_bytes());
    pdb[block * 2 + 8..block * 2 + 12].copy_from_slice(&SEED_AGE.to_le_bytes());
    pdb[block * 2 + 12..block * 2 + 28].copy_from_slice(&SEED_GUID);
    pdb
}

fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pf-fuzz-pe-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[test]
fn pe_identity_survives_mutations_of_a_valid_pe() {
    let dir = scratch_dir("mut");
    let path = dir.join("seed.exe");
    let base = build_seed_pe();
    std::fs::write(&path, &base).expect("write seed");
    // Seed sanity: the unmutated seed must parse to the full identity.
    let (ts, _ck, r) = read_identity(&path).expect("seed parses");
    assert_eq!(ts, 0x6700_0000);
    let r = r.expect("seed carries an RSDS reference");
    assert_eq!(r.guid, SEED_GUID);
    assert_eq!(r.age, SEED_AGE);
    assert_eq!(r.path, "seed.pdb");

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..600 * fuzz_scale() {
            let mutated = mutate(&mut rng, &base);
            std::fs::write(&path, &mutated).expect("write mutant");
            let _ = read_identity(&path); // None or Some — never a panic
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pe_identity_survives_soups_and_truncations() {
    let dir = scratch_dir("soup");
    let path = dir.join("soup.exe");
    let mut rng = Rng::new(42);
    for _ in 0..1_500 * fuzz_scale() {
        let len = 1 + rng.below(1024);
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        std::fs::write(&path, &bytes).expect("write soup");
        let _ = read_identity(&path);
    }
    // Truncations of the seed at every 64-byte boundary.
    let base = build_seed_pe();
    for end in (1..base.len()).step_by(64) {
        std::fs::write(&path, &base[..end]).expect("write truncation");
        let _ = read_identity(&path);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pdb_matches_survives_mutations_of_a_valid_pdb() {
    let dir = scratch_dir("pdb");
    let path = dir.join("seed.pdb");
    let base = build_seed_pdb();
    std::fs::write(&path, &base).expect("write seed");
    let r = PdbRef {
        guid: SEED_GUID,
        age: SEED_AGE,
        path: "seed.pdb".into(),
    };
    // Seed sanity: the reference matches its own PDB.
    assert!(pdb_matches(&path, &r).expect("seed matches"));
    let wrong_guid = PdbRef {
        guid: [0u8; 16],
        age: SEED_AGE,
        path: "seed.pdb".into(),
    };
    assert!(!pdb_matches(&path, &wrong_guid).expect("parseable"));
    let wrong_age = PdbRef {
        guid: SEED_GUID,
        age: SEED_AGE + 1,
        path: "seed.pdb".into(),
    };
    assert!(!pdb_matches(&path, &wrong_age).expect("parseable"));

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..600 * fuzz_scale() {
            let mutated = mutate(&mut rng, &base);
            std::fs::write(&path, &mutated).expect("write mutant");
            // Ok(bool) or a typed StorageIo error — never a panic.
            let _ = pdb_matches(&path, &r);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_pdb_survives_hostile_references() {
    // Mutated RSDS references (GUID, age, path) against a directory
    // holding only the valid seed PDB: resolution must never accept a
    // non-matching file and never panic — hostile path bytes included.
    let dir = scratch_dir("resolve");
    let pdb_path = dir.join("seed.pdb");
    std::fs::write(&pdb_path, build_seed_pdb()).expect("write seed pdb");
    let module = dir.join("mod.exe");
    std::fs::write(&module, build_seed_pe()).expect("write seed pe");
    let mut rng = Rng::new(99);
    for _ in 0..2_000 * fuzz_scale() {
        let mut guid = SEED_GUID;
        if rng.below(2) == 0 {
            let i = rng.below(16);
            guid[i] ^= 0xFF;
        }
        let age = if rng.below(2) == 0 {
            SEED_AGE ^ u32::try_from(rng.next_u64()).unwrap_or(1).max(1)
        } else {
            SEED_AGE
        };
        let path = if rng.below(3) == 0 {
            // hostile-ish path bytes: unicode lossy, spaces, dots
            let mut p = String::new();
            for _ in 0..rng.below(24) {
                let c = (rng.next_u64() & 0xFF) as u8;
                p.push(c as char);
            }
            p
        } else {
            "seed.pdb".into()
        };
        let r = PdbRef { guid, age, path };
        // Never a panic; a hit must be the verified seed PDB.
        if let Ok(Some(hit)) = resolve_pdb(&module, &r) {
            assert!(hit.ends_with("seed.pdb"), "hit is the verified file");
            assert!(pdb_matches(&hit, &r).unwrap_or(false));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
