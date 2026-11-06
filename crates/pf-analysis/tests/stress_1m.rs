//! PF-910 bounded slice: stress the stack-aggregation engine with 1M+
//! synthetic sampled stacks covering the roadmap's named shapes — many
//! modules, no symbols, broken references, and recursive call stacks —
//! while asserting the honesty invariants (exact weights, counted
//! recursion cuts, node-cap honesty) and recording query performance.
//!
//! The full 1M run is the default; `PF_STRESS_STACKS` overrides the count
//! (e.g. 100_000 for quick local loops). Numbers are printed for the
//! ledger, not asserted against (measurement, not a performance claim).

#![allow(clippy::arithmetic_side_effects)]

use std::time::Instant;

use pf_analysis::stacks::{SampledStack, StackFrame, StackTrie};

fn stress_count() -> usize {
    std::env::var("PF_STRESS_STACKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000)
}

/// Generate the corpus: interleaved shapes per the roadmap's named
/// categories, outermost-frame-first.
fn generate(n: usize) -> Vec<SampledStack> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let kind = i % 5;
        let frames: Vec<StackFrame> = match kind {
            // Deep recursion: A -> B -> A -> B ... (cycle-cut exercise).
            0 => (0..12)
                .map(|d| {
                    let key = if d % 2 == 0 { "recurse_a" } else { "recurse_b" };
                    StackFrame {
                        key: key.into(),
                        module: "app".into(),
                        resolved: true,
                        file: None,
                        line: None,
                        offset: 0,
                    }
                })
                .collect(),
            // Wide fan-out: 10 siblings under one root (node-cap pressure).
            1 => {
                let mut v = vec![StackFrame {
                    key: "wide_root".into(),
                    module: "app".into(),
                    resolved: true,
                    file: None,
                    line: None,
                    offset: 0,
                }];
                for d in 0..10 {
                    v.push(StackFrame {
                        key: format!("wide_{d}"),
                        module: "app".into(),
                        resolved: true,
                        file: None,
                        line: None,
                        offset: 0,
                    });
                }
                v
            }
            // No symbols: unresolved leaves on a broken module.
            2 => vec![
                StackFrame {
                    key: "known_caller".into(),
                    module: "app".into(),
                    resolved: true,
                    file: None,
                    line: None,
                    offset: 0,
                },
                StackFrame {
                    key: "<unresolved>".into(),
                    module: String::new(),
                    resolved: false,
                    file: None,
                    line: None,
                    offset: 0,
                },
            ],
            // Many modules: 8 frames across 8 distinct module names.
            3 => (0..8)
                .map(|d| StackFrame {
                    key: format!("mod_fn_{d}"),
                    module: format!("lib{d}.so"),
                    resolved: true,
                    file: None,
                    line: None,
                    offset: 0,
                })
                .collect(),
            // Broken references: empty module + garbage keys.
            _ => vec![
                StackFrame {
                    key: String::new(),
                    module: String::new(),
                    resolved: false,
                    file: None,
                    line: None,
                    offset: 0,
                },
                StackFrame {
                    key: "after_broken".into(),
                    module: "app".into(),
                    resolved: true,
                    file: None,
                    line: None,
                    offset: 0,
                },
            ],
        };
        out.push(SampledStack { frames, weight: 1 });
    }
    out
}

#[test]
fn one_million_stacks_aggregate_with_honest_invariants() {
    let n = stress_count();
    println!("PF910 generating {n} synthetic stacks (5 shapes)…");
    let t_gen = Instant::now();
    let samples = generate(n);
    let gen_ms = t_gen.elapsed().as_millis();

    println!("PF910 aggregating…");
    let t_agg = Instant::now();
    let mut trie = StackTrie::new();
    trie.aggregate(&samples);
    let agg_ms = t_agg.elapsed().as_millis();

    // Honesty invariants (exact, not statistical).
    assert_eq!(trie.total_weight(), n as u64, "every sample counted once");
    assert_eq!(trie.sample_count(), n);
    // The recursion shape cuts every one of its samples at the cycle.
    let expected_cuts = n.div_ceil(5);
    assert_eq!(
        trie.recursion_cuts(),
        expected_cuts,
        "recursion cuts counted exactly"
    );
    // Coverage: the unresolved shapes are the no-symbol (1/5) and broken
    // (1/5) shapes' leaf frames.
    assert!(trie.unresolved_weight() > 0, "unresolved weight tracked");
    let top = trie.top_by_exclusive(8);
    assert!(!top.is_empty(), "top functions exist");
    let json = trie.to_json(16);
    assert!(json.get("recursion_cuts").is_some());
    assert!(json.get("call_tree").is_some());

    println!(
        "PF910 n={n} shapes=5 generate={gen_ms}ms aggregate={agg_ms}ms \
         cuts={} unresolved_weight={} top_exclusive={:?}",
        trie.recursion_cuts(),
        trie.unresolved_weight(),
        top.iter()
            .take(3)
            .map(|(k, _, w, _)| (k.to_string(), *w))
            .collect::<Vec<_>>(),
    );
}
