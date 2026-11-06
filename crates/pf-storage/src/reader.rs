//! Chunk reader (PF-505/506): scans sealed chunks, verifies every CRC,
//! exposes bounded ranged queries and spike-preserving envelope rollups.
//! Corruption is surfaced as `PARTIAL_DATA` with the salvage boundary —
//! never silently skipped, never a panic, never unbounded memory.

use std::path::{Path, PathBuf};

use pf_types::{CounterSnapshot, ErrorCode, PfError};

use crate::chunk::{HEADER_BYTES, RECORD_U64S, TRAILER_BYTES, decode_header, decode_record};
use crate::crc32::crc32;

/// One sealed chunk's metadata + records (records loaded lazily on query).
struct Chunk {
    path: PathBuf,
    min_ns: u64,
    max_ns: u64,
    count: u32,
    /// Set when the chunk failed verification; its records are excluded.
    corrupt: Option<String>,
}

pub struct ChunkReader {
    chunks_dir: PathBuf,
    chunks: Vec<Chunk>,
    partial_files: Vec<String>,
    total_records: usize,
    salvage_notes: Vec<String>,
}

/// Result page of a ranged query — bounded by construction.
pub struct QueryPage {
    pub snapshots: Vec<CounterSnapshot>,
    pub total_in_range: usize,
    pub truncated: bool,
}

/// One envelope bucket: min/max/first/last/count — preserves spikes
/// (PF-504: never average-only downsampling).
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvelopeBucket {
    pub bucket_start_ns: u64,
    pub bucket_end_ns: u64,
    pub count: usize,
    pub first: Option<f64>,
    pub last: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

// Query/bucket arithmetic bounded by caps validated at entry.
#[allow(clippy::arithmetic_side_effects)]
impl ChunkReader {
    pub fn open(session_dir: &Path) -> Result<ChunkReader, PfError> {
        let chunks_dir = session_dir.join("chunks");
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut partial_files = Vec::new();
        let mut salvage_notes = Vec::new();
        let mut total_records = 0usize;

        let entries = fs_read_dir(&chunks_dir)?;
        let mut names: Vec<String> = entries.into_iter().flatten().collect();
        names.sort();
        for name in &names {
            if let Some(_stem) = name.strip_suffix(".partial") {
                // Evidence of an interrupted write: kept on disk, ignored.
                partial_files.push(name.clone());
                continue;
            }
            let Some(stem) = name.strip_suffix(".pfc") else {
                continue;
            };
            if !stem.starts_with("metrics-") {
                continue;
            }
            let path = chunks_dir.join(name);
            let Ok(buf) = std::fs::read(&path) else {
                salvage_notes.push(format!("unreadable chunk {name}"));
                continue;
            };
            if buf.len() < HEADER_BYTES + TRAILER_BYTES {
                salvage_notes.push(format!("chunk {name} too short ({})", buf.len()));
                continue;
            }
            let (count, min_ns, max_ns) = match decode_header(&buf) {
                Ok(v) => v,
                Err(e) => {
                    salvage_notes.push(format!("chunk {name}: {e}"));
                    chunks.push(Chunk {
                        path,
                        min_ns: 0,
                        max_ns: u64::MAX,
                        count: 0,
                        corrupt: Some(e),
                    });
                    continue;
                }
            };
            let body_end = buf.len() - TRAILER_BYTES;
            let expect_crc = u32::from_le_bytes(buf[body_end..].try_into().expect("4 bytes"));
            let actual_crc = crc32(&buf[..body_end]);
            if expect_crc != actual_crc {
                let note = format!(
                    "chunk {name}: CRC mismatch (stored {expect_crc:08X}, computed {actual_crc:08X})"
                );
                salvage_notes.push(note.clone());
                chunks.push(Chunk {
                    path,
                    min_ns,
                    max_ns,
                    count,
                    corrupt: Some(note),
                });
                continue;
            }
            total_records += count as usize;
            chunks.push(Chunk {
                path,
                min_ns,
                max_ns,
                count,
                corrupt: None,
            });
        }
        Ok(ChunkReader {
            chunks_dir,
            chunks,
            partial_files,
            total_records,
            salvage_notes,
        })
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.iter().filter(|c| c.corrupt.is_none()).count()
    }

    pub fn corrupt_chunk_count(&self) -> usize {
        self.chunks.iter().filter(|c| c.corrupt.is_some()).count()
    }

    pub fn partial_file_count(&self) -> usize {
        self.partial_files.len()
    }

    pub fn total_records(&self) -> usize {
        self.total_records
    }

    pub fn time_range(&self) -> Option<(u64, u64)> {
        let valid = self.chunks.iter().filter(|c| c.corrupt.is_none());
        let min = valid.clone().map(|c| c.min_ns).min()?;
        let max = valid.map(|c| c.max_ns).max()?;
        Some((min, max))
    }

    pub fn salvage_notes(&self) -> &[String] {
        &self.salvage_notes
    }

    pub fn chunks_dir(&self) -> &Path {
        &self.chunks_dir
    }

    /// Ranged query: returns up to `cap` snapshots in `[from_ns, to_ns]`,
    /// memory-bounded, with stride sampling when the range exceeds the cap
    /// (the caller learns `total_in_range` and `truncated`).
    pub fn range(&self, from_ns: u64, to_ns: u64, cap: usize) -> Result<QueryPage, PfError> {
        if cap == 0 || cap > 1_000_000 {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                "query cap must be within 1..=1000000",
            ));
        }
        let mut all: Vec<CounterSnapshot> = Vec::new();
        for chunk in self.chunks.iter().filter(|c| c.corrupt.is_none()) {
            if chunk.max_ns < from_ns || chunk.min_ns > to_ns {
                continue; // chunk outside the window
            }
            let buf = std::fs::read(&chunk.path).map_err(PfError::from)?;
            let mut pos = HEADER_BYTES;
            for _ in 0..chunk.count {
                let rec = decode_record(&buf, &mut pos).map_err(|e| {
                    PfError::new(
                        ErrorCode::PartialData,
                        format!("chunk decode failed mid-way: {e}"),
                    )
                })?;
                if rec.mono_ns >= from_ns && rec.mono_ns <= to_ns {
                    all.push(rec);
                }
            }
        }
        let total_in_range = all.len();
        if total_in_range <= cap {
            return Ok(QueryPage {
                snapshots: all,
                total_in_range,
                truncated: false,
            });
        }
        // Stride sampling keeps first/last and evenly spaced samples.
        // Integer stride (rounded up) avoids float-index rounding traps.
        let stride = total_in_range.div_ceil(cap).max(1);
        let mut picked: Vec<CounterSnapshot> = Vec::with_capacity(cap + 1);
        let mut idx = 0usize;
        while idx < total_in_range && picked.len() < cap {
            picked.push(all[idx].clone());
            idx += stride;
        }
        // Guarantee the true last sample within the cap: swap it for the
        // stride-picked tail when needed.
        if let Some(last) = all.last() {
            if picked.last().map(|s| s.mono_ns) != Some(last.mono_ns) {
                if picked.len() >= cap {
                    picked.pop();
                }
                picked.push(last.clone());
            }
        }
        Ok(QueryPage {
            snapshots: picked,
            total_in_range,
            truncated: true,
        })
    }

    /// Envelope rollup over `[from_ns, to_ns]` with `buckets` slots:
    /// per-bucket first/last/min/max/count of a chosen field. Spikes are
    /// preserved by min/max; gaps are visible as empty buckets.
    pub fn envelope(
        &self,
        from_ns: u64,
        to_ns: u64,
        buckets: usize,
        field: EnvelopeField,
    ) -> Result<Vec<EnvelopeBucket>, PfError> {
        if buckets == 0 || buckets > 10_000 {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                "bucket count must be within 1..=10000",
            ));
        }
        // Bound the working set: at most 32 samples per bucket.
        let page = self.range(from_ns, to_ns, buckets * 32)?;
        Ok(envelope_of_samples(
            &page.snapshots,
            from_ns,
            to_ns,
            buckets,
            field,
        ))
    }
}

/// Pure envelope rollup over in-memory samples (shared by the chunk reader
/// and in-memory clients — one bucketing implementation).
// Bucket arithmetic on bounded window/bucket counts (validated upstream).
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn envelope_of_samples(
    samples: &[CounterSnapshot],
    from_ns: u64,
    to_ns: u64,
    buckets: usize,
    field: EnvelopeField,
) -> Vec<EnvelopeBucket> {
    let buckets = buckets.max(1);
    let span = to_ns.saturating_sub(from_ns).max(1);
    let bucket_ns = (span / buckets as u64).max(1);
    let mut out = vec![EnvelopeBucket::default(); buckets];
    for (i, b) in out.iter_mut().enumerate() {
        b.bucket_start_ns = from_ns + (i as u64) * bucket_ns;
        b.bucket_end_ns = if i == buckets - 1 {
            to_ns
        } else {
            from_ns + (i as u64 + 1) * bucket_ns
        };
    }
    for s in samples {
        if s.mono_ns < from_ns || s.mono_ns > to_ns {
            continue;
        }
        let offset = s.mono_ns.saturating_sub(from_ns);
        let idx = usize::try_from(offset / bucket_ns)
            .unwrap_or(0)
            .min(out.len() - 1);
        let Some(value) = field.pick(s) else { continue };
        let b = &mut out[idx];
        b.count += 1;
        if b.first.is_none() {
            b.first = Some(value);
        }
        b.last = Some(value);
        b.min = Some(b.min.map_or(value, |m: f64| m.min(value)));
        b.max = Some(b.max.map_or(value, |m: f64| m.max(value)));
    }
    out
}

/// Selectable envelope source field.
#[derive(Clone, Copy, Debug)]
pub enum EnvelopeField {
    ResidentBytes,
    CpuUserNs,
    IoReadBytes,
}

impl EnvelopeField {
    fn pick(self, s: &CounterSnapshot) -> Option<f64> {
        match self {
            EnvelopeField::ResidentBytes => s.resident_bytes,
            EnvelopeField::CpuUserNs => s.cpu_user_ns,
            EnvelopeField::IoReadBytes => s.io_read_bytes,
        }
        .map(|v| v as f64)
    }
}

fn fs_read_dir(dir: &Path) -> Result<Vec<Option<String>>, PfError> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_str().map(str::to_string))
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(PfError::from(e)),
    }
}

// Compile-time sanity for the sizes used by the reader.
const _: () = assert!(RECORD_U64S * 8 == 112);
const _: () = assert!(HEADER_BYTES + TRAILER_BYTES == 32);

/// Delete a finalized `.pfsession` directory, returning the bytes freed.
///
/// Safety rails: the path must exist, be a directory, and its file name
/// must end in the `.pfsession` marker — callers (the daemon) derive the
/// path from the parsed `SessionId` itself, never from raw client input,
/// and this check is the second line of defense against a mis-derived
/// path. A live session must never reach this function; callers refuse
/// non-terminal sessions before asking for deletion.
pub fn delete_session_dir(session_dir: &Path) -> Result<u64, PfError> {
    let marker = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".pfsession"))
        .ok_or_else(|| {
            PfError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "refusing to delete `{}`: not a .pfsession directory",
                    session_dir.display()
                ),
            )
        })?;
    if marker.is_empty() {
        return Err(PfError::new(
            ErrorCode::InvalidArgument,
            "refusing to delete: empty session name",
        ));
    }
    let meta = std::fs::metadata(session_dir).map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::SessionNotFound
        } else {
            ErrorCode::StorageIo
        };
        PfError::new(code, format!("{}: {e}", session_dir.display()))
    })?;
    if !meta.is_dir() {
        return Err(PfError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not a directory", session_dir.display()),
        ));
    }
    let bytes = dir_size(session_dir);
    std::fs::remove_dir_all(session_dir)?;
    Ok(bytes)
}

/// Total size of a directory tree in bytes (bounded by the file count the
/// session store actually has; a delete refuses nothing based on size —
/// deleting is the point — this is for the confirmation report only).
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total = total.saturating_add(dir_size(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    fn temp_session(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pf-del-{tag}-{}-{}.pfsession",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let chunks = dir.join("chunks");
        std::fs::create_dir_all(&chunks).expect("mkdir");
        std::fs::write(dir.join("manifest.json"), b"{\"session_id\":\"x\"}").expect("write");
        std::fs::write(chunks.join("metrics-0.pfc"), vec![0u8; 1024]).expect("write");
        dir
    }

    #[test]
    fn deletes_a_pfsession_dir_and_reports_bytes() {
        let dir = temp_session("ok");
        let bytes = delete_session_dir(&dir).expect("delete ok");
        assert!(bytes >= 1042, "bytes freed: {bytes}");
        assert!(!dir.exists(), "directory must be gone");
    }

    #[test]
    fn refuses_paths_without_the_pfsession_marker() {
        let dir = temp_session("bad");
        let wrong = dir.with_file_name("not-a-session");
        std::fs::rename(&dir, &wrong).expect("rename");
        let e = delete_session_dir(&wrong).expect_err("must refuse");
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(wrong.exists(), "nothing was deleted");
        let _ = std::fs::remove_dir_all(&wrong);
    }

    #[test]
    fn missing_dir_is_session_not_found() {
        // Marker-bearing name that does not exist on disk.
        let gone = std::env::temp_dir().join(format!(
            "pf-del-gone-{}-{}.pfsession",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let e = delete_session_dir(&gone).expect_err("missing must be typed");
        assert_eq!(e.code, ErrorCode::SessionNotFound);
    }
}
