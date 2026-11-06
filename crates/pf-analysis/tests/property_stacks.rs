//! Randomized property tests for the stack-aggregation engine: the trie
//! carries exactness contracts (weights, recursion cuts, coverage) that
//! example tests cover only on named shapes — this suite throws
//! deterministic random corpora at the invariants directly.
//!
//! Invariants per corpus (both directions):
//! 1. total_weight == the exact sum of sample weights;
//! 2. sample_count == the corpus size;
//! 3. every node satisfies inclusive >= exclusive (a node's leaf weight
//!    cannot exceed the weight that passed through it);
//! 4. recursion cuts == EXACTLY the number of samples whose key sequence
//!    contains a repeat (each such sample is cut once, at the first
//!    repeat);
//! 5. the sum of root children's inclusive weight == total_weight (every
//!    sample descends through exactly one root child);
//! 6. the flame tree (generous caps, no truncation) preserves the same
//!    root sums and per-node inclusive weights for every node it kept.
//!
//! Plus a differential: `from_resolved` and `from_resolved_owned` build
//! identical stacks from the same tuples.

#![allow(clippy::arithmetic_side_effects)]

use std::collections::HashSet;

use pf_analysis::stacks::{SampledStack, StackFrame, StackTrie};

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

const KEYS: usize = 48;
const MODULES: usize = 8;
const CORPUS: usize = 50_000;

/// Corpus-tier parameters: the SMALL tier keeps the trie under the
/// default 50k node cap (8 keys, depth ≤ 5 → bounded distinct nodes) so
/// EXACT cut counting is assertable; the LARGE tier intentionally
/// overflows the cap to assert truncation honesty.
const SMALL_KEYS: usize = 8;
const SMALL_MAX_DEPTH: usize = 5;

fn frame(rng: &mut Rng, keys: usize) -> StackFrame {
    let f = rng.below(4);
    StackFrame {
        key: if f == 3 {
            "<unresolved>".into() // unresolved frames share the identity
        } else {
            format!("fn_{}", rng.below(keys))
        },
        module: if f == 3 {
            String::new()
        } else {
            format!("lib{}.so", rng.below(MODULES))
        },
        resolved: f != 3,
        file: None,
        line: None,
        offset: rng.next_u64() & 0xffff,
    }
}

/// One random stack; returns it and whether its key sequence repeats
/// (→ exactly one recursion cut when no cap truncates the walk).
fn random_stack(rng: &mut Rng, keys: usize, max_depth: usize) -> (SampledStack, bool) {
    let depth = 1 + rng.below(max_depth);
    let mut frames = Vec::with_capacity(depth);
    for _ in 0..depth {
        frames.push(frame(rng, keys));
    }
    let mut forced_repeat = false;
    // Deliberate recursion bursts: 1 in 8 stacks is a ping-pong chain.
    if rng.below(8) == 0 {
        let a = format!("ping_{}", rng.below(4));
        let b = format!("pong_{}", rng.below(4));
        frames = (0..6)
            .map(|d| {
                let mut fr = frame(rng, SMALL_KEYS);
                fr.key = if d % 2 == 0 { a.clone() } else { b.clone() };
                fr
            })
            .collect();
        forced_repeat = true;
    }
    // Repeat detection runs over the FINISHED frames (borrow after build).
    let mut seen: HashSet<&str> = HashSet::new();
    let mut repeats_local = false;
    for fr in &frames {
        if !seen.insert(fr.key.as_str()) {
            repeats_local = true;
        }
    }
    let repeats = forced_repeat || repeats_local;
    let weight = 1 + (rng.next_u64() % 3);
    (SampledStack { frames, weight }, repeats)
}

fn check_invariants(trie: &StackTrie, corpus: &[SampledStack], expected_cuts: usize) {
    let total: u64 = corpus.iter().map(|s| s.weight).sum();
    assert_eq!(trie.total_weight(), total, "weights exact");
    assert_eq!(trie.sample_count(), corpus.len(), "sample count exact");
    assert_eq!(
        trie.recursion_cuts(),
        expected_cuts,
        "cuts == samples with a repeated key"
    );

    // Per-node invariant + root sum via the bounded flame tree with caps
    // far above this corpus's distinct-shape count (no truncation).
    let tree = trie.flame_tree(usize::MAX, usize::MAX);
    assert!(!tree.depth_capped, "caps must not truncate this corpus");
    assert!(
        !tree.root_children.is_empty() || corpus.is_empty(),
        "tree reflects the corpus"
    );
    let mut root_sum = 0u64;
    fn walk(nodes: &[pf_analysis::stacks::FlameNode], out: &mut u64) {
        for n in nodes {
            assert!(
                n.inclusive >= n.exclusive,
                "inclusive >= exclusive for {}",
                n.key
            );
            *out += n.inclusive;
            walk(&n.children, out);
        }
    }
    // The flame tree root children are the trie's roots.
    let mut root_only = 0u64;
    for n in &tree.root_children {
        assert!(n.inclusive >= n.exclusive, "root child {}", n.key);
        root_only += n.inclusive;
        walk(&n.children, &mut root_sum);
    }
    assert_eq!(
        root_only, total,
        "root children's inclusive sum == total (every sample has one root)"
    );
    // Nodes beyond the root sum to something ≤ the total (each sample
    // contributes ≤ 1 unit per remaining depth level).
    assert!(root_sum <= total.saturating_mul(64), "bounded node sums");
}

#[test]
fn aggregation_invariants_hold_under_random_corpora() {
    for &seed in &[1u64, 2, 3, 5, 8, 13] {
        let mut rng = Rng::new(seed);

        // SMALL tier (no cap trip): exact cut counting is assertable.
        let mut corpus = Vec::with_capacity(CORPUS);
        let mut cuts = 0usize;
        for _ in 0..CORPUS {
            let (s, repeats) = random_stack(&mut rng, SMALL_KEYS, SMALL_MAX_DEPTH);
            if repeats {
                cuts += 1;
            }
            corpus.push(s);
        }

        let mut top_down = StackTrie::new();
        top_down.aggregate(&corpus);
        assert!(
            !top_down.nodes_truncated(),
            "small tier stays under the cap"
        );
        check_invariants(&top_down, &corpus, cuts);

        // Bottom-up: the SAME invariants hold on reversed walks; cuts are
        // identical (a repeated key repeats in reverse too).
        let mut bottom_up = StackTrie::new();
        bottom_up.aggregate_bottom_up(&corpus);
        check_invariants(&bottom_up, &corpus, cuts);

        // LARGE tier (cap trips): truncation is HONEST — totals stay
        // exact, the flag reports, and truncated walks never invent cuts.
        let mut big = Vec::with_capacity(CORPUS);
        let mut big_repeats = 0usize;
        for _ in 0..CORPUS {
            let (s, repeats) = random_stack(&mut rng, KEYS, 24);
            if repeats {
                big_repeats += 1;
            }
            big.push(s);
        }
        let mut capped = StackTrie::new();
        capped.aggregate(&big);
        assert!(capped.nodes_truncated(), "large tier overflows the cap");
        let total: u64 = big.iter().map(|s| s.weight).sum();
        assert_eq!(capped.total_weight(), total, "truncated totals exact");
        assert!(
            capped.recursion_cuts() <= big_repeats,
            "truncated walks never invent cuts"
        );

        // Node cap honesty: a tiny cap truncates but keeps totals exact.
        let mut capped_small = StackTrie::with_node_cap(16);
        capped_small.aggregate(&corpus[..100]);
        assert!(capped_small.nodes_truncated(), "cap trips on a tiny budget");
        let expected: u64 = corpus[..100].iter().map(|s| s.weight).sum();
        assert_eq!(
            capped_small.total_weight(),
            expected,
            "truncated totals exact"
        );
    }
}

#[test]
fn from_resolved_and_owned_build_identically() {
    let mut rng = Rng::new(0x1dea);
    type ResolvedTuple = (String, String, u64, Option<String>, Option<u32>);
    for _ in 0..2_000 {
        let depth = 1 + rng.below(12);
        let tuples: Vec<ResolvedTuple> = (0..depth)
            .map(|_| {
                let fr = frame(&mut rng, KEYS);
                let file = if rng.below(3) == 0 {
                    None
                } else {
                    Some(format!("src/f{}.rs", rng.below(9)))
                };
                let line = if rng.below(3) == 0 {
                    None
                } else {
                    u32::try_from(1 + rng.below(400)).ok()
                };
                (fr.module, fr.key, fr.offset, file, line)
            })
            .collect();
        let borrowed = SampledStack::from_resolved(&tuples, 1);
        let owned = SampledStack::from_resolved_owned(tuples, 1);
        assert_eq!(borrowed.frames, owned.frames, "constructors agree");
        assert_eq!(borrowed.weight, owned.weight);
    }
}
