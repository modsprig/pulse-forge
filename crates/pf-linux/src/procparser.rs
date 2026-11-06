//! `/proc/<pid>/stat` parsing (roadmap PF-301): robust to `(comm)`
//! containing spaces, parentheses, digits, and newlines-by-escaping — the
//! documented trap is that field splitting must happen on the **last `)`**,
//! never naive whitespace.

/// Parsed fields of one stat line (only what PulseForge consumes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatFields {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    /// utime + stime in clock ticks (fields 14+15).
    pub utime_ticks: u64,
    pub stime_ticks: u64,
    /// Major + minor faults (fields 10, 12 — note field 11 is minflt's
    /// neighbor; majflt=10, majflt is what we keep, minflt skipped).
    pub minflt: u64,
    pub majflt: u64,
    pub num_threads: u64,
    /// starttime in clock ticks since boot (field 22) — identity anchor.
    pub starttime_ticks: u64,
    /// RSS in pages (field 24).
    pub rss_pages: u64,
    /// vsize in bytes (field 23).
    pub vsize: u64,
}

/// Parse one stat line. Returns None when the line is empty or truncated.
// Field-index arithmetic bounded by the whitespace split length checks.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn parse_stat(line: &str) -> Option<StatFields> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }
    let pid: u32 = line[..open].trim().parse().ok()?;
    let comm = line[open + 1..close].to_string();
    let rest = line[close + 1..].trim_start();
    let f: Vec<&str> = rest.split_whitespace().collect();
    // rest[0] is field 3 (state). Field N (1-based) is rest[N-3].
    let get = |n: usize| -> Option<&str> { f.get(n.saturating_sub(3)).copied() };
    let num = |n: usize| -> Option<u64> { get(n).and_then(|s| s.parse().ok()) };
    Some(StatFields {
        pid,
        comm,
        state: get(3)?.chars().next()?,
        ppid: num(4).unwrap_or(0) as u32,
        minflt: num(10)?,
        majflt: num(12)?,
        utime_ticks: num(14)?,
        stime_ticks: num(15)?,
        num_threads: num(20)?,
        starttime_ticks: num(22)?,
        vsize: num(23)?,
        rss_pages: num(24)?,
    })
}

/// Ticks (CLK_TCK) → nanoseconds.
// tick math on u64 is bounded (ticks << 1e9/100 fits u64 for any uptime).
#[allow(clippy::arithmetic_side_effects)]
pub fn ticks_to_ns(ticks: u64, clk_tck: u32) -> u64 {
    if clk_tck == 0 {
        return 0;
    }
    ticks.saturating_mul(1_000_000_000 / u64::from(clk_tck))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The canonical hostile fixture: comm containing spaces and parens.
    const TRICKY: &str = "1234 (evil (kworker) #1 v2.6) S 1 2 0 0 -1 4194560 123 456 7 8 100 50 3 2 20 0 4 0 98765 123456789 512 18446744073709551615 1 2 3 4 5 6 7 8 9 10 11 12 13 14 0 0";

    #[test]
    fn parses_comm_with_spaces_and_parens() {
        let f = parse_stat(TRICKY).expect("tricky line parses");
        assert_eq!(f.pid, 1234);
        assert_eq!(f.comm, "evil (kworker) #1 v2.6");
        assert_eq!(f.state, 'S');
        assert_eq!(f.ppid, 1);
        // fields 14/15 (utime/stime) = 100/50
        assert_eq!(f.utime_ticks, 100);
        assert_eq!(f.stime_ticks, 50);
        assert_eq!(f.num_threads, 4);
        assert_eq!(f.starttime_ticks, 98765);
    }

    #[test]
    fn real_kernel_shape_parses() {
        // Captured from a real Ubuntu /proc/self/stat (fields beyond 24 vary).
        let line = "293 (head) R 280 280 280 34816 280 4194304 103 0 0 0 0 0 0 0 20 0 1 0 221 3211264 272 18446744073709551615 1 2 3 4 5 6 7 8 9 10 11 12 13 14 0 0";
        let f = parse_stat(line).expect("parses");
        assert_eq!(f.comm, "head");
        assert_eq!(f.state, 'R');
        // Verified field indices on this exact line: minflt(10)=103,
        // utime(14)=0, threads(20)=1, starttime(22)=221, rss(24)=272.
        assert_eq!(f.minflt, 103);
        assert_eq!(f.utime_ticks, 0);
        assert_eq!(f.num_threads, 1);
        assert_eq!(f.starttime_ticks, 221);
        assert_eq!(f.rss_pages, 272);
    }

    #[test]
    fn ticks_conversion_uses_clk_tck() {
        // 100 ticks/s (documented Ubuntu default): 250 ticks = 2.5 s = 2.5e9 ns
        assert_eq!(ticks_to_ns(250, 100), 2_500_000_000);
        assert_eq!(ticks_to_ns(0, 100), 0);
        assert_eq!(ticks_to_ns(100, 0), 0); // degenerate clk_tck -> 0, not panic
    }

    #[test]
    fn truncated_and_garbage_return_none() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("1234 (no close").is_none());
        assert!(parse_stat("1234 )reversed( S 1 2").is_none()); // last-')' before '('
        assert!(parse_stat("abc (x) S").is_none()); // non-numeric pid
        assert!(parse_stat("1234 (x)").is_none()); // no fields after comm
    }
}
