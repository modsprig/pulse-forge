//! # pf-storage
//!
//! Durable session storage (roadmap P5, ADR-005): append-only **binary
//! chunks** with per-chunk checksums, atomic publish, crash recovery, and
//! bounded ranged queries with spike-preserving envelopes.
//!
//! Format (little-endian, version 1):
//!
//! ```text
//! chunk file  chunks/metrics-000001.pfc
//!   u32  magic "PFRB"          (0x5046_4252)
//!   u16  format version        (1)
//!   u16  flags                 (0)
//!   u32  record count
//!   u64  min mono_ns, max mono_ns
//!   record_count × fixed-layout snapshot (14×u64 + 2×u32 packed as u64s,
//!        exactly the JSONL field order of schema-0)
//!   u32  CRC-32 (IEEE) of every preceding byte
//! ```
//!
//! Durability rules: chunks are written to `<name>.partial`, flushed, then
//! atomically renamed; a `.partial` file is evidence of an interrupted
//! write and is never promoted. Readers verify the CRC of every chunk and
//! report corruption as `PARTIAL_DATA` with the salvage boundary — never a
//! silent skip, never a panic.
//!
//! This replaces JSONL as the high-frequency storage for sessions recorded
//! through the daemon (the JSONL files remain readable as schema-0
//! interchange).

mod chunk;
mod crc32;
mod reader;
mod writer;

pub use chunk::{CHUNK_MAGIC, CHUNK_VERSION, MAX_RECORDS_PER_CHUNK, RECORD_U64S};
pub use reader::{
    ChunkReader, EnvelopeBucket, EnvelopeField, QueryPage, delete_session_dir, envelope_of_samples,
};
pub use writer::ChunkWriter;
