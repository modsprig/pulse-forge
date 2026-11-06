//! # pulseforge CLI
//!
//! v0.1 vertical slice (roadmap P2): attach to or launch a permitted local
//! process, record resource counters, and produce a typed, honest report.
//!
//! Rules encoded here:
//! - `--run` launches the given executable **directly** with an argument
//!   vector — never through a shell;
//! - recording a target never kills it: attached targets are untouched, and
//!   a child PulseForge launched is terminated after the recording only
//!   because we are its direct parent (opt out with `--leave-running`);
//! - every number in the report carries a unit and an availability state;
//! - durations, intervals and sample counts are bounded.

mod args;
mod bench;
mod commands;
mod daemon_cmds;
mod mcp_bridge;
mod report;

use std::process::ExitCode;

use pf_types::{ErrorCode, PfError};

const USAGE: &str = "\
pulseforge — local-first process profiler (v0.1 counters slice + daemon)

USAGE:
  pulseforge <COMMAND> [OPTIONS]

COMMANDS:
  capabilities                    Report what this machine can do right now (JSON)
  daemon                          Run the per-user recording daemon (single instance)
  agent <op>                      One-shot daemon client:
      capabilities | list [--limit N] | sessions
      start --pid N | --run <exe> [-- <args…>] [--duration-ms] [--interval-ms] [--out]
      status|stop|metrics --session <id>
  list [--limit N]                List visible processes (bounded, default 100)
  record --pid <N> [options]      Record an attached process (never touched)
  record --run <exe> [-- <args…>] Launch a workload directly (no shell) and record it
      --duration-ms <ms>          Recording budget (default 10000, max 3600000)
      --interval-ms <ms>          Poll interval (default 250, range 100..5000)
      --out <dir>                 Session root (default: <user data>/sessions)
      --label <text>              Free-form label stored in the manifest
      --leave-running             Do not terminate a child launched by --run
  report --session <dir>          Re-render a recorded session as a report (JSON)
  stacks --session <dir> [--limit N] [--bottom-up]  Aggregate stacks (PF-904/907)
  export-stacks --session <dir> [--out f]  Export sampled stacks as speedscope JSON (PF-909)
  export-pprof --session <dir> --out file  Export sampled stacks as pprof protobuf (PF-909)
  probe counters                  Counters feasibility probe against this process
  probe etw                       ETW session-creation feasibility probe (read-only)
  doctor                          Environment sanity check (JSON)
  bench-spin --ms <ms>            Busy-spin utility used for overhead measurement
  version                         Print version
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&argv) {
        Ok(cmd) => match commands::dispatch(cmd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                print_error(&e);
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn print_error(e: &PfError) {
    eprintln!("error [{}]: {}", e.code, e.message);
    if let Some(r) = &e.remediation {
        eprintln!("hint: {r}");
    }
    if e.code == ErrorCode::UnsupportedPlatform {
        eprintln!("note: this build was compiled without a collector backend for this OS");
    }
}
