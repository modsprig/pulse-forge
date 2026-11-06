//! Append-only chunk writer (PF-502): buffers records, seals a chunk on the
//! rotation threshold or on `seal()`, publishes atomically via
//! `.partial` → rename, and fsyncs before publishing.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use pf_types::{CounterSnapshot, ErrorCode, PfError};

use crate::chunk::{
    HEADER_BYTES, MAX_RECORDS_PER_CHUNK, RECORD_U64S, encode_header, encode_record,
};
use crate::crc32::crc32;

pub struct ChunkWriter {
    chunks_dir: PathBuf,
    buffer: Vec<CounterSnapshot>,
    next_index: u32,
    chunks_written: u32,
    records_written: u64,
    sealed: bool,
    /// Optional wall-time flush period for incremental durability.
    flush_period: Option<std::time::Duration>,
    last_flush: std::time::Instant,
}

// Counters bounded by the rotation threshold and index space checks.
#[allow(clippy::arithmetic_side_effects)]
impl ChunkWriter {
    /// Create (or continue) a chunk store under `<session_dir>/chunks`.
    /// Continuing an existing store appends after the highest sealed index.
    pub fn open(session_dir: &Path) -> Result<ChunkWriter, PfError> {
        let chunks_dir = session_dir.join("chunks");
        fs::create_dir_all(&chunks_dir).map_err(PfError::from)?;
        let next_index = highest_sealed_index(&chunks_dir)? + 1;
        Ok(ChunkWriter {
            chunks_dir,
            buffer: Vec::with_capacity(1024),
            next_index,
            chunks_written: 0,
            records_written: 0,
            sealed: false,
            flush_period: None,
            last_flush: std::time::Instant::now(),
        })
    }

    /// Enable time-based flush: buffered samples are sealed into a chunk at
    /// least every `period`, so a crash loses at most one period of data.
    pub fn with_flush_period(mut self, period: std::time::Duration) -> Self {
        self.flush_period = Some(period.max(std::time::Duration::from_millis(250)));
        self
    }

    /// Buffer one snapshot. Flushes a chunk when the rotation threshold is
    /// reached — the incremental durability point (PF-506).
    pub fn push(&mut self, s: &CounterSnapshot) -> Result<(), PfError> {
        if self.sealed {
            return Err(PfError::new(ErrorCode::InvalidArgument, "writer is sealed"));
        }
        self.buffer.push(s.clone());
        let due_by_count = self.buffer.len() >= MAX_RECORDS_PER_CHUNK;
        let due_by_time = self
            .flush_period
            .is_some_and(|p| self.buffer.len() >= 4 && self.last_flush.elapsed() >= p);
        if due_by_count || due_by_time {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Flush any buffered records and seal the writer.
    pub fn seal(&mut self) -> Result<(), PfError> {
        if !self.buffer.is_empty() {
            self.flush_chunk()?;
        }
        self.sealed = true;
        Ok(())
    }

    pub fn records_written(&self) -> u64 {
        self.records_written + self.buffer.len() as u64
    }

    pub fn chunks_written(&self) -> u32 {
        self.chunks_written
    }

    fn flush_chunk(&mut self) -> Result<(), PfError> {
        let count = self.buffer.len();
        if count == 0 {
            return Ok(());
        }
        let min_ns = self.buffer.first().map(|s| s.mono_ns).unwrap_or(0);
        let max_ns = self.buffer.last().map(|s| s.mono_ns).unwrap_or(0);
        if min_ns > max_ns {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                "snapshots must be pushed in mono_ns order",
            ));
        }

        let mut body = Vec::with_capacity(HEADER_BYTES + count * RECORD_U64S * 8);
        encode_header(
            u32::try_from(count).unwrap_or(u32::MAX),
            min_ns,
            max_ns,
            &mut body,
        );
        for s in &self.buffer {
            encode_record(s, &mut body);
        }
        let crc = crc32(&body);
        body.extend_from_slice(&crc.to_le_bytes());

        let name = format!("metrics-{:010}.pfc", self.next_index);
        let tmp = self.chunks_dir.join(format!("{name}.partial"));
        let final_path = self.chunks_dir.join(&name);
        {
            let mut f = fs::File::create(&tmp).map_err(PfError::from)?;
            f.write_all(&body).map_err(PfError::from)?;
            f.sync_all().map_err(PfError::from)?;
        }
        fs::rename(&tmp, &final_path).map_err(PfError::from)?;

        self.records_written += count as u64;
        self.buffer.clear();
        self.last_flush = std::time::Instant::now();
        self.next_index = self.next_index.checked_add(1).ok_or_else(|| {
            PfError::new(ErrorCode::ResourceExhausted, "chunk index space exhausted")
        })?;
        self.chunks_written += 1;
        Ok(())
    }
}

fn highest_sealed_index(dir: &Path) -> Result<u32, PfError> {
    let mut highest: Option<u32> = None;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // "metrics-%010u.pfc"
            if let Some(stem) = name.strip_suffix(".pfc") {
                if let Some(dec) = stem.strip_prefix("metrics-") {
                    if let Ok(v) = dec.parse::<u32>() {
                        highest = Some(highest.map_or(v, |h: u32| h.max(v)));
                    }
                }
            }
        }
    }
    Ok(highest.unwrap_or(0))
}
