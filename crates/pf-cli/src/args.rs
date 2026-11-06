//! Small, tested argument parser (no dependencies; clap is not justified for
//! this surface). Returns typed errors; never panics on user input.

// Manual cursor stepping is bounded by argv length on every path.
#![allow(clippy::arithmetic_side_effects)]

use pf_types::{ErrorCode, PfError};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Capabilities,
    Daemon,
    Agent(AgentCommand),
    List {
        limit: usize,
    },
    Record(RecordArgs),
    Report {
        session: String,
    },
    /// Speedscope export of a session's sampled stacks (PF-909).
    ExportStacks {
        session: String,
        out: Option<String>,
    },
    /// pprof (profile.proto) export of a session's sampled stacks (PF-909).
    ExportPprof {
        session: String,
        out: String,
    },
    /// Stack aggregation over a recorded session's sampled stacks (PF-904).
    Stacks {
        session: String,
        limit: usize,
        /// PF-1105: diff this session's top functions against another's.
        compare_session: Option<String>,
        /// PF-907: aggregate bottom-up (callee-first) instead of top-down.
        bottom_up: bool,
    },
    ProbeCounters,
    ProbeEtw,
    Compare {
        baseline: String,
        candidate: String,
        force: bool,
        markdown: bool,
    },
    Doctor,
    BenchSpin {
        ms: u64,
    },
    /// Deep link: open a recorded session in the GUI (PF-708).
    Ui {
        session: String,
    },
    /// Repeated-run benchmark: compare two workload specs (PF-1101..1104).
    Bench {
        baseline: String,
        candidate: String,
        out_dir: Option<String>,
    },
    /// Export a recorded session (PF-1209): JSON file, optional redaction.
    Export {
        session: String,
        out: String,
        redact: bool,
    },
    Mcp,
    Version,
}

/// One-shot daemon client operations (`pulseforge agent <op>`).
#[derive(Clone, Debug, PartialEq)]
pub enum AgentCommand {
    Capabilities,
    List {
        limit: u32,
    },
    Sessions,
    Start {
        pid: Option<u32>,
        run: Option<String>,
        run_args: Vec<String>,
        duration_ms: u64,
        interval_ms: u64,
        out_dir: Option<String>,
    },
    Status {
        session: String,
    },
    Stop {
        session: String,
    },
    Metrics {
        session: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordArgs {
    pub pid: Option<u32>,
    pub run: Option<String>,
    pub run_args: Vec<String>,
    pub duration_ms: u64,
    pub interval_ms: u64,
    pub out_dir: Option<String>,
    pub label: Option<String>,
    pub leave_running: bool,
}

impl Default for RecordArgs {
    fn default() -> Self {
        RecordArgs {
            pid: None,
            run: None,
            run_args: Vec::new(),
            duration_ms: 10_000,
            interval_ms: 250,
            out_dir: None,
            label: None,
            leave_running: false,
        }
    }
}

fn invalid(msg: String) -> PfError {
    PfError::new(ErrorCode::InvalidArgument, msg)
}

fn next_value<'a>(argv: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str, PfError> {
    *i += 1;
    argv.get(*i)
        .map(String::as_str)
        .ok_or_else(|| invalid(format!("{flag} requires a value")))
}

pub fn parse(argv: &[String]) -> Result<Command, PfError> {
    let Some(first) = argv.first() else {
        return Err(invalid("missing command".into()));
    };
    let mut i = 0;
    match first.as_str() {
        "capabilities" => Ok(Command::Capabilities),
        "daemon" => Ok(Command::Daemon),
        "mcp" => Ok(Command::Mcp),
        "export" => {
            let mut session = None;
            let mut out = None;
            let mut redact = false;
            let mut i = 0;
            while {
                i += 1;
                i < argv.len()
            } {
                match argv[i].as_str() {
                    "--session" => {
                        i += 1;
                        session = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--session requires a dir".into()))?,
                        );
                    }
                    "--out" => {
                        i += 1;
                        out = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--out requires a path".into()))?,
                        );
                    }
                    "--redact" => redact = true,
                    other => return Err(invalid(format!("unknown export option `{other}`"))),
                }
            }
            Ok(Command::Export {
                session: session.ok_or_else(|| invalid("--session is required".into()))?,
                out: out.ok_or_else(|| invalid("--out is required".into()))?,
                redact,
            })
        }
        "bench" => {
            let mut baseline = None;
            let mut candidate = None;
            let mut out_dir = None;
            let mut i = 0;
            while {
                i += 1;
                i < argv.len()
            } {
                match argv[i].as_str() {
                    "--a" => {
                        i += 1;
                        baseline = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--a requires a spec path".into()))?,
                        );
                    }
                    "--b" => {
                        i += 1;
                        candidate = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--b requires a spec path".into()))?,
                        );
                    }
                    "--out" => {
                        i += 1;
                        out_dir = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--out requires a dir".into()))?,
                        );
                    }
                    other => return Err(invalid(format!("unknown bench option `{other}`"))),
                }
            }
            Ok(Command::Bench {
                baseline: baseline.ok_or_else(|| invalid("--a is required".into()))?,
                candidate: candidate.ok_or_else(|| invalid("--b is required".into()))?,
                out_dir,
            })
        }
        "ui" => {
            let session = argv
                .get(1)
                .cloned()
                .ok_or_else(|| invalid("ui requires a session directory".into()))?;
            Ok(Command::Ui { session })
        }
        "agent" => parse_agent(argv),
        "version" => Ok(Command::Version),
        "doctor" => Ok(Command::Doctor),
        "probe" => {
            let sub = next_value(argv, &mut i, "probe")?;
            match sub {
                "counters" => Ok(Command::ProbeCounters),
                "etw" => Ok(Command::ProbeEtw),
                other => Err(invalid(format!("unknown probe `{other}` (counters|etw)"))),
            }
        }
        "list" => {
            let mut limit = 100usize;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--limit" {
                    limit = next_value(argv, &mut i, "--limit")?
                        .parse()
                        .map_err(|_| invalid("--limit must be a number".into()))?;
                } else {
                    return Err(invalid(format!("unknown list option `{}`", argv[i])));
                }
            }
            if limit == 0 || limit > 4_096 {
                return Err(invalid("--limit must be within 1..=4096".into()));
            }
            Ok(Command::List { limit })
        }
        "bench-spin" => {
            let mut ms: u64 = 1_000;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--ms" {
                    ms = next_value(argv, &mut i, "--ms")?
                        .parse()
                        .map_err(|_| invalid("--ms must be a number".into()))?;
                } else {
                    return Err(invalid(format!("unknown bench-spin option `{}`", argv[i])));
                }
            }
            if ms == 0 || ms > 600_000 {
                return Err(invalid("--ms must be within 1..=600000".into()));
            }
            Ok(Command::BenchSpin { ms })
        }
        "stacks" => {
            let mut session = None;
            let mut limit = 32usize;
            let mut compare_session = None;
            let mut bottom_up = false;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--session" {
                    session = Some(next_value(argv, &mut i, "--session")?.to_string());
                } else if argv[i] == "--limit" {
                    i += 1;
                    limit = argv
                        .get(i)
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|n| (1..=256).contains(n))
                        .ok_or_else(|| invalid("--limit must be 1..=256".into()))?;
                } else if argv[i] == "--compare-session" {
                    compare_session =
                        Some(next_value(argv, &mut i, "--compare-session")?.to_string());
                } else if argv[i] == "--bottom-up" {
                    bottom_up = true;
                } else {
                    return Err(invalid(format!("unknown stacks option `{}`", argv[i])));
                }
            }
            Ok(Command::Stacks {
                session: session.ok_or_else(|| invalid("--session is required".into()))?,
                limit,
                compare_session,
                bottom_up,
            })
        }
        "export-pprof" => {
            let mut session = None;
            let mut out = None;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--session" {
                    session = Some(next_value(argv, &mut i, "--session")?.to_string());
                } else if argv[i] == "--out" {
                    out = Some(next_value(argv, &mut i, "--out")?.to_string());
                } else {
                    return Err(invalid(format!(
                        "unknown export-pprof option `{}`",
                        argv[i]
                    )));
                }
            }
            Ok(Command::ExportPprof {
                session: session.ok_or_else(|| invalid("--session is required".into()))?,
                // Binary output goes to a FILE only: stdout stays
                // text/JSON for protocol purity.
                out: out.ok_or_else(|| invalid("--out is required (binary output)".into()))?,
            })
        }
        "export-stacks" => {
            let mut session = None;
            let mut out = None;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--session" {
                    session = Some(next_value(argv, &mut i, "--session")?.to_string());
                } else if argv[i] == "--out" {
                    out = Some(next_value(argv, &mut i, "--out")?.to_string());
                } else {
                    return Err(invalid(format!(
                        "unknown export-stacks option `{}`",
                        argv[i]
                    )));
                }
            }
            Ok(Command::ExportStacks {
                session: session.ok_or_else(|| invalid("--session is required".into()))?,
                out,
            })
        }
        "report" => {
            let mut session = None;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--session" {
                    session = Some(next_value(argv, &mut i, "--session")?.to_string());
                } else {
                    return Err(invalid(format!("unknown report option `{}`", argv[i])));
                }
            }
            Ok(Command::Report {
                session: session.ok_or_else(|| invalid("--session is required".into()))?,
            })
        }
        "compare" => parse_compare(argv),
        "record" => parse_record(argv),
        other => Err(invalid(format!("unknown command `{other}`"))),
    }
}

/// Parse `pulseforge agent <op> …` arguments.
fn parse_agent(argv: &[String]) -> Result<Command, PfError> {
    let sub = argv.get(1).map(String::as_str).unwrap_or("");
    let cmd = match sub {
        "capabilities" => AgentCommand::Capabilities,
        "sessions" => AgentCommand::Sessions,
        "list" => {
            let mut limit: u32 = 100;
            let mut i = 1;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--limit" {
                    i += 1;
                    limit = argv
                        .get(i)
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| invalid("--limit must be a number".into()))?;
                }
            }
            AgentCommand::List { limit }
        }
        "start" => {
            let mut pid = None;
            let mut run: Option<String> = None;
            let mut run_args = Vec::new();
            let mut duration_ms = 10_000u64;
            let mut interval_ms = 250u64;
            let mut out_dir = None;
            let mut i = 1;
            while {
                i += 1;
                i < argv.len()
            } {
                match argv[i].as_str() {
                    "--pid" => {
                        i += 1;
                        pid = Some(
                            argv.get(i)
                                .and_then(|v| v.parse().ok())
                                .ok_or_else(|| invalid("--pid must be a number".into()))?,
                        );
                    }
                    "--run" => {
                        i += 1;
                        run = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--run requires a path".into()))?,
                        );
                    }
                    "--duration-ms" => {
                        i += 1;
                        duration_ms = argv
                            .get(i)
                            .and_then(|v| v.parse().ok())
                            .ok_or_else(|| invalid("--duration-ms must be a number".into()))?;
                    }
                    "--interval-ms" => {
                        i += 1;
                        interval_ms = argv
                            .get(i)
                            .and_then(|v| v.parse().ok())
                            .ok_or_else(|| invalid("--interval-ms must be a number".into()))?;
                    }
                    "--out" => {
                        i += 1;
                        out_dir = Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| invalid("--out requires a path".into()))?,
                        );
                    }
                    "--" => {
                        run_args = argv[i + 1..].to_vec();
                        break;
                    }
                    other => return Err(invalid(format!("unknown agent start option `{other}`"))),
                }
            }
            if pid.is_none() && run.is_none() {
                return Err(invalid("agent start needs --pid or --run".into()));
            }
            AgentCommand::Start {
                pid,
                run,
                run_args,
                duration_ms,
                interval_ms,
                out_dir,
            }
        }
        "status" | "stop" | "metrics" => {
            let mut session = None;
            let mut i = 1;
            while {
                i += 1;
                i < argv.len()
            } {
                if argv[i] == "--session" {
                    i += 1;
                    session = Some(
                        argv.get(i)
                            .cloned()
                            .ok_or_else(|| invalid("--session requires an id".into()))?,
                    );
                }
            }
            let session = session.ok_or_else(|| invalid("--session is required".into()))?;
            match sub {
                "status" => AgentCommand::Status { session },
                "stop" => AgentCommand::Stop { session },
                _ => AgentCommand::Metrics { session },
            }
        }
        other => {
            return Err(invalid(format!(
                "unknown agent op `{other}` (capabilities|list|sessions|start|status|stop|metrics)"
            )));
        }
    };
    Ok(Command::Agent(cmd))
}

// Manual index stepping is bounded by argv length on every path.
#[allow(clippy::arithmetic_side_effects)]
fn parse_record(argv: &[String]) -> Result<Command, PfError> {
    let mut args = RecordArgs::default();
    let mut i = 0;
    while {
        i += 1;
        i < argv.len()
    } {
        let flag = argv[i].as_str();
        match flag {
            "--pid" => {
                let v = next_value(argv, &mut i, "--pid")?;
                args.pid = Some(
                    v.parse()
                        .map_err(|_| invalid(format!("--pid must be a number, got `{v}`")))?,
                );
            }
            "--run" => {
                args.run = Some(next_value(argv, &mut i, "--run")?.to_string());
            }
            "--duration-ms" => {
                args.duration_ms = next_value(argv, &mut i, "--duration-ms")?
                    .parse()
                    .map_err(|_| invalid("--duration-ms must be a number".into()))?;
            }
            "--interval-ms" => {
                args.interval_ms = next_value(argv, &mut i, "--interval-ms")?
                    .parse()
                    .map_err(|_| invalid("--interval-ms must be a number".into()))?;
            }
            "--out" => {
                args.out_dir = Some(next_value(argv, &mut i, "--out")?.to_string());
            }
            "--label" => {
                args.label = Some(next_value(argv, &mut i, "--label")?.to_string());
            }
            "--leave-running" => args.leave_running = true,
            "--" => {
                // Everything after -- is the child's argv (no shell, ever).
                args.run_args = argv[i + 1..].to_vec();
                break;
            }
            other => return Err(invalid(format!("unknown record option `{other}`"))),
        }
    }
    match (args.pid, &args.run) {
        (Some(_), Some(_)) => return Err(invalid("--pid and --run are mutually exclusive".into())),
        (None, None) => return Err(invalid("one of --pid or --run is required".into())),
        _ => {}
    }
    if args.duration_ms == 0 || args.duration_ms > 3_600_000 {
        return Err(invalid("--duration-ms must be within 1..=3600000".into()));
    }
    if !(100..=5_000).contains(&args.interval_ms) {
        return Err(invalid("--interval-ms must be within 100..=5000".into()));
    }
    Ok(Command::Record(args))
}

fn parse_compare(argv: &[String]) -> Result<Command, PfError> {
    let mut baseline = None;
    let mut candidate = None;
    let mut force = false;
    let mut markdown = false;
    let mut i = 0;
    while {
        i += 1;
        i < argv.len()
    } {
        match argv[i].as_str() {
            "--a" => {
                i += 1;
                baseline = Some(
                    argv.get(i)
                        .cloned()
                        .ok_or_else(|| invalid("--a requires a session dir".into()))?,
                );
            }
            "--b" => {
                i += 1;
                candidate = Some(
                    argv.get(i)
                        .cloned()
                        .ok_or_else(|| invalid("--b requires a session dir".into()))?,
                );
            }
            "--force" => force = true,
            "--markdown" => markdown = true,
            other => return Err(invalid(format!("unknown compare option `{other}`"))),
        }
    }
    Ok(Command::Compare {
        baseline: baseline.ok_or_else(|| invalid("--a is required".into()))?,
        candidate: candidate.ok_or_else(|| invalid("--b is required".into()))?,
        force,
        markdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_attach_record_with_defaults() {
        let c = parse(&argv(&["record", "--pid", "1234"])).unwrap();
        match c {
            Command::Record(r) => {
                assert_eq!(r.pid, Some(1234));
                assert_eq!(r.duration_ms, 10_000);
                assert_eq!(r.interval_ms, 250);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_run_record_with_child_argv() {
        let c = parse(&argv(&[
            "record",
            "--run",
            "C:/tool/tool.exe",
            "--duration-ms",
            "2000",
            "--",
            "-v",
            "--input",
            "my file.txt",
        ]))
        .unwrap();
        match c {
            Command::Record(r) => {
                assert_eq!(r.run.as_deref(), Some("C:/tool/tool.exe"));
                assert_eq!(r.run_args, vec!["-v", "--input", "my file.txt"]);
                assert_eq!(r.duration_ms, 2_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pid_and_run_are_exclusive() {
        let err = parse(&argv(&["record", "--pid", "1", "--run", "x.exe"])).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn bounds_are_enforced() {
        assert_eq!(
            parse(&argv(&["record", "--pid", "1", "--interval-ms", "50"]))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            parse(&argv(&["record", "--pid", "1", "--interval-ms", "6000"]))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            parse(&argv(&[
                "record",
                "--pid",
                "1",
                "--duration-ms",
                "999999999"
            ]))
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            parse(&argv(&["bench-spin", "--ms", "0"])).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn missing_values_are_typed_errors() {
        assert_eq!(
            parse(&argv(&["record", "--pid"])).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            parse(&argv(&["report"])).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            parse(&argv(&[])).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            parse(&argv(&["probe", "nonsense"])).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn compare_parses_with_flags() {
        assert!(matches!(
            parse(&argv(&["compare"])).unwrap_err().code,
            ErrorCode::InvalidArgument
        ));
        match parse(&argv(&["compare", "--a", "dir1", "--b", "dir2"])).unwrap() {
            Command::Compare {
                baseline,
                candidate,
                force,
                markdown,
            } => {
                assert_eq!(baseline, "dir1");
                assert_eq!(candidate, "dir2");
                assert!(!force && !markdown);
            }
            other => panic!("{other:?}"),
        }
        match parse(&argv(&[
            "compare",
            "--a",
            "x",
            "--b",
            "y",
            "--force",
            "--markdown",
        ]))
        .unwrap()
        {
            Command::Compare {
                force, markdown, ..
            } => assert!(force && markdown),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn agent_ops_parse() {
        assert_eq!(
            parse(&argv(&["agent", "capabilities"])).unwrap(),
            Command::Agent(AgentCommand::Capabilities)
        );
        assert_eq!(
            parse(&argv(&["agent", "sessions"])).unwrap(),
            Command::Agent(AgentCommand::Sessions)
        );
        assert_eq!(
            parse(&argv(&["agent", "list", "--limit", "5"])).unwrap(),
            Command::Agent(AgentCommand::List { limit: 5 })
        );
        match parse(&argv(&["agent", "start", "--pid", "42"])).unwrap() {
            Command::Agent(AgentCommand::Start {
                pid,
                run,
                duration_ms,
                ..
            }) => {
                assert_eq!(pid, Some(42));
                assert!(run.is_none());
                assert_eq!(duration_ms, 10_000);
            }
            other => panic!("{other:?}"),
        }
        match parse(&argv(&[
            "agent",
            "start",
            "--run",
            "C:/x.exe",
            "--duration-ms",
            "900",
            "--",
            "-v",
        ]))
        .unwrap()
        {
            Command::Agent(AgentCommand::Start {
                run,
                run_args,
                duration_ms,
                ..
            }) => {
                assert_eq!(run.as_deref(), Some("C:/x.exe"));
                assert_eq!(run_args, vec!["-v"]);
                assert_eq!(duration_ms, 900);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            parse(&argv(&["agent", "stop", "--session", "abc"])).unwrap(),
            Command::Agent(AgentCommand::Stop {
                session: "abc".into()
            })
        );
        assert!(parse(&argv(&["agent", "start"])).is_err());
        assert!(parse(&argv(&["agent"])).is_err());
    }

    #[test]
    fn daemon_command_parses() {
        assert_eq!(parse(&argv(&["daemon"])).unwrap(), Command::Daemon);
        assert_eq!(parse(&argv(&["mcp"])).unwrap(), Command::Mcp);
        assert_eq!(
            parse(&argv(&["ui", "some/dir"])).unwrap(),
            Command::Ui {
                session: "some/dir".into()
            }
        );
        assert!(parse(&argv(&["ui"])).is_err());
    }

    #[test]
    fn subcommands_parse() {
        assert_eq!(
            parse(&argv(&["capabilities"])).unwrap(),
            Command::Capabilities
        );
        assert_eq!(
            parse(&argv(&["list", "--limit", "7"])).unwrap(),
            Command::List { limit: 7 }
        );
        assert_eq!(parse(&argv(&["probe", "etw"])).unwrap(), Command::ProbeEtw);
        assert_eq!(parse(&argv(&["doctor"])).unwrap(), Command::Doctor);
        assert_eq!(
            parse(&argv(&["bench-spin", "--ms", "250"])).unwrap(),
            Command::BenchSpin { ms: 250 }
        );
    }
}
