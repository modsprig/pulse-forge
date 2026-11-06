//! # pf-linux
//!
//! Linux backend: `/proc` counters, boot-id identity, platform
//! capabilities. Runs only on Linux (`cfg(target_os = "linux")`).

#[cfg(target_os = "linux")]
pub mod dwarf;
#[cfg(target_os = "linux")]
pub mod elfio;
#[cfg(target_os = "linux")]
pub mod line;
pub mod perf;
#[cfg(target_os = "linux")]
pub mod procparser;
#[cfg(target_os = "linux")]
pub mod procsource;
#[cfg(target_os = "linux")]
pub mod symbols;
#[cfg(target_os = "linux")]
pub mod uds;

#[cfg(target_os = "linux")]
pub use perf::{PerfSample, PerfSampler};
#[cfg(target_os = "linux")]
pub use procsource::{LinuxCounterSource, LinuxPlatform};
#[cfg(target_os = "linux")]
pub use symbols::{
    ElfFunc, MapsEntry, ResolvedFrame, elf_functions, parse_maps, read_maps, resolve_ips,
};
