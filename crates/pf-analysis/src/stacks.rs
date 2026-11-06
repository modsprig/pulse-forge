//! Stack aggregation core (roadmap PF-904): a canonical stack trie keyed by
//! interned frame keys, computing inclusive/exclusive sample weights with
//! cycle-cut recursion safety, unresolved-frame buckets and a bounded call
//! tree — the shared engine beneath top-functions, call trees and
//! flamegraphs (PF-905/906/907, PF-1007).
//!
//! This module is deliberately OS-neutral and testable with synthetic
//! stacks; the recorder pipeline (P8/P10) feeds it real sampled stacks.
//!
//! Honesty contract: unresolved frames are never silently merged into a
//! "known" parent bucket; recursion cuts are counted, not hidden; every
//! output list is bounded and reports what was omitted.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use pf_types::json::JsonValue;

/// Zero-dep FxHasher (the rustc `fxhash` algorithm: rotate-xor-multiply
/// over usize chunks) — several times faster than the default SipHash for
/// the short string keys the trie hashes per sample-frame (function names).
/// Correctness never depends on hash quality: HashMap/HashSet semantics
/// are unchanged, only the hash function is swapped.
#[derive(Default)]
pub(crate) struct FxHasher {
    hash: u64,
}

const FX_MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(FX_MULTIPLIER);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in chunks.by_ref() {
            // chunks_exact yields exactly 8-byte slices.
            let arr: [u8; 8] = chunk.try_into().expect("chunks_exact guarantees 8 bytes");
            self.add_to_hash(u64::from_le_bytes(arr));
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            self.add_to_hash(u64::from_le_bytes(buf));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }
}

/// BuildHasher for trie-internal maps/sets keyed by interned frame strings.
type FxBuild = BuildHasherDefault<FxHasher>;

/// One resolved frame as callers hand it over: (module, symbol, offset,
/// file, line) — the tuple shape stacks.jsonl carries per frame entry.
pub type ResolvedInput = (String, String, u64, Option<String>, Option<u32>);

/// One frame of a sampled stack, already symbol-resolved by the caller.
/// `key` is the aggregation identity (symbol name when resolved, else
/// `module+0xoffset`, else `<unresolved>`); `module` feeds the unresolved
/// buckets; `resolved` is honest per-frame provenance; `file`/`line` carry
/// the `.debug_line` attribution when known (PF-1005).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    pub key: String,
    pub module: String,
    pub resolved: bool,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// Module-relative address of the frame (from the sampled IP).
    pub offset: u64,
}

/// One sampled stack. Callers provide frames OUTERMOST first (the trie's
/// natural order); [`SampledStack::flip`] converts callee-first storage.
#[derive(Clone, Debug)]
pub struct SampledStack {
    /// Outermost frame first.
    pub frames: Vec<StackFrame>,
    pub weight: u64,
}

impl SampledStack {
    pub fn flip(&self) -> SampledStack {
        let mut frames = self.frames.clone();
        frames.reverse();
        SampledStack {
            frames,
            weight: self.weight,
        }
    }

    /// Convenience for resolved frames: identity = symbol (or module+offset
    /// fallback); empty module ⇒ unresolved. `file`/`line` pass through
    /// when the caller has them (PF-1005 line attribution).
    pub fn from_resolved(frames: &[ResolvedInput], weight: u64) -> SampledStack {
        let frames = frames
            .iter()
            .map(|(module, symbol, offset, file, line)| {
                let (key, resolved) = if !symbol.is_empty() {
                    (symbol.clone(), true)
                } else if !module.is_empty() {
                    (format!("{module}+0x{offset:x}"), true)
                } else {
                    ("<unresolved>".to_string(), false)
                };
                StackFrame {
                    key,
                    module: module.clone(),
                    resolved,
                    file: file.clone(),
                    line: *line,
                    offset: *offset,
                }
            })
            .collect();
        SampledStack { frames, weight }
    }

    /// Consuming variant of [`SampledStack::from_resolved`]: takes
    /// ownership of freshly parsed tuples instead of cloning every string
    /// (the stacks.jsonl reader hands these over, so the load path pays
    /// zero extra allocations). Frames are consumed IN THE ORDER GIVEN —
    /// pass them OUTERMOST first (reverse the callee-first disk order in
    /// place before calling; [`SampledStack::flip`] is not needed).
    pub fn from_resolved_owned(frames: Vec<ResolvedInput>, weight: u64) -> SampledStack {
        let frames = frames
            .into_iter()
            .map(|(module, symbol, offset, file, line)| {
                let (key, resolved) = if !symbol.is_empty() {
                    (symbol, true)
                } else if !module.is_empty() {
                    (format!("{module}+0x{offset:x}"), true)
                } else {
                    ("<unresolved>".to_string(), false)
                };
                StackFrame {
                    key,
                    module,
                    resolved,
                    file,
                    line,
                    offset,
                }
            })
            .collect();
        SampledStack { frames, weight }
    }
}

/// Child container: most trie nodes hold 1–3 children (deep chains), so a
/// short linear scan beats hashing there; wide fan-outs (a bottom-up
/// trie's root) promote to a hash map past the cap. Lookups, insertion
/// and iteration are order-independent for consumers — every renderer
/// sorts by inclusive weight afterward.
enum Children {
    Small(Vec<(String, usize)>),
    Large(HashMap<String, usize, FxBuild>),
}

/// Linear-scan cap before promotion to the hash map.
const SMALL_CHILDREN_CAP: usize = 8;

impl Default for Children {
    fn default() -> Self {
        Children::Small(Vec::new())
    }
}

impl Children {
    fn get(&self, key: &str) -> Option<usize> {
        match self {
            Children::Small(v) => v.iter().find(|(k, _)| k == key).map(|(_, i)| *i),
            Children::Large(m) => m.get(key).copied(),
        }
    }

    fn insert(&mut self, key: String, idx: usize) {
        match self {
            Children::Small(v) => {
                if v.len() < SMALL_CHILDREN_CAP {
                    v.push((key, idx));
                } else {
                    let mut m: HashMap<String, usize, FxBuild> = HashMap::default();
                    for (k, i) in std::mem::take(v) {
                        m.insert(k, i);
                    }
                    m.insert(key, idx);
                    *self = Children::Large(m);
                }
            }
            Children::Large(m) => {
                m.insert(key, idx);
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        match self {
            Children::Small(v) => v.iter().map(|(_, i)| *i).collect::<Vec<_>>().into_iter(),
            Children::Large(m) => m.values().copied().collect::<Vec<_>>().into_iter(),
        }
    }
}

#[derive(Default)]
struct Node {
    children: Children,
    /// Total weight of samples whose stack passes through this frame
    /// (inclusive).
    inclusive: u64,
    /// Weight of samples whose stack ENDS here — the true leaf, or the
    /// cycle-cut point for recursive stacks (counted in `recursion_cuts`).
    exclusive: u64,
}

/// Default node budget: aggregation is bounded before it is exposed.
pub const DEFAULT_NODE_CAP: usize = 50_000;

/// Aggregated stack trie. Bounded by the node cap; deeper frames of samples
/// that would exceed it are dropped and reported (`nodes_truncated`).
#[derive(Default)]
pub struct StackTrie {
    nodes: Vec<Node>,
    key_of: Vec<String>,
    module_of: Vec<String>,
    total_weight: u64,
    sample_count: usize,
    unresolved_weight: u64,
    /// module → weight of frames that failed symbol resolution.
    unresolved_buckets: HashMap<String, u64, FxBuild>,
    recursion_cuts: usize,
    node_cap: usize,
    nodes_truncated: bool,
}

// Aggregation counters are bounded by sample and node counts; caps make
// overflow unreachable in practice and saturating ops keep it honest anyway.
#[allow(clippy::arithmetic_side_effects)]
impl StackTrie {
    pub fn new() -> StackTrie {
        StackTrie::with_node_cap(DEFAULT_NODE_CAP)
    }

    pub fn with_node_cap(node_cap: usize) -> StackTrie {
        StackTrie {
            nodes: vec![Node::default()], // index 0 = synthetic root
            key_of: Vec::new(),
            module_of: Vec::new(),
            total_weight: 0,
            sample_count: 0,
            unresolved_weight: 0,
            unresolved_buckets: HashMap::default(),
            recursion_cuts: 0,
            node_cap,
            nodes_truncated: false,
        }
    }

    /// Aggregate `samples` (outermost-frame-first order). The per-sample
    /// path set is allocated ONCE for the whole batch (clear() keeps its
    /// capacity) — the hot loop allocates no per-sample containers.
    pub fn aggregate(&mut self, samples: &[SampledStack]) {
        self.aggregate_iter(samples.iter());
    }

    /// Streaming variant of [`StackTrie::aggregate`]: consumes an iterator
    /// of borrowed stacks so callers (the GUI's window/thread
    /// re-aggregation) never need a clone-collect of the selected
    /// sub-batch — at recorder stress scale that clone is a deep copy of
    /// every selected stack on every pan/zoom.
    pub fn aggregate_iter<'a, I>(&mut self, samples: I)
    where
        I: Iterator<Item = &'a SampledStack>,
    {
        let mut path: HashSet<&str, FxBuild> = HashSet::default();
        for s in samples {
            path.clear();
            self.insert_walk(s.frames.iter(), s.weight, &mut path);
        }
    }

    /// Aggregate `samples` BOTTOM-UP (PF-907): frames are walked in reverse
    /// (no per-sample clone through [`SampledStack::flip`]), so the trie's
    /// "root" is the LEAF and exclusive weight lands on the OUTERMOST frame
    /// (e.g. `main`). Top-exclusive on a bottom-up trie answers "which
    /// entry points ran this code", and the tree walks callees-first. Cycle
    /// cuts and node caps behave exactly as in the top-down trie (reversed
    /// recursion is still recursion).
    pub fn aggregate_bottom_up(&mut self, samples: &[SampledStack]) {
        self.aggregate_bottom_up_iter(samples.iter());
    }

    /// Streaming variant of [`StackTrie::aggregate_bottom_up`]; the
    /// iterator must be double-ended (frames are walked in reverse).
    pub fn aggregate_bottom_up_iter<'a, I>(&mut self, samples: I)
    where
        I: DoubleEndedIterator<Item = &'a SampledStack>,
    {
        let mut path: HashSet<&str, FxBuild> = HashSet::default();
        for s in samples {
            path.clear();
            self.insert_walk(s.frames.iter().rev(), s.weight, &mut path);
        }
    }

    pub fn add(&mut self, sample: &SampledStack) {
        let mut path: HashSet<&str, FxBuild> = HashSet::default();
        self.insert_walk(sample.frames.iter(), sample.weight, &mut path);
    }

    /// One sample's walk from the root, over frames in the given order.
    /// `path` holds the keys already on the current root→node path (the
    /// caller reuses one set across a batch). Recursion safety: a frame
    /// already on the path is a cycle — cut there, count the cut, never
    /// hidden. Node caps truncate honestly (`nodes_truncated`).
    fn insert_walk<'a, I>(&mut self, frames: I, weight: u64, path: &mut HashSet<&'a str, FxBuild>)
    where
        I: Iterator<Item = &'a StackFrame>,
    {
        let mut node = 0usize; // root
        for frame in frames {
            if path.contains(frame.key.as_str()) {
                self.recursion_cuts = self.recursion_cuts.saturating_add(1);
                break;
            }
            let next = match self.nodes[node].children.get(frame.key.as_str()) {
                Some(idx) => idx,
                None => {
                    if self.nodes.len() >= self.node_cap {
                        self.nodes_truncated = true;
                        break;
                    }
                    let idx = self.nodes.len();
                    self.nodes.push(Node::default());
                    self.key_of.push(frame.key.clone());
                    self.module_of.push(frame.module.clone());
                    self.nodes[node].children.insert(frame.key.clone(), idx);
                    idx
                }
            };
            self.nodes[next].inclusive = self.nodes[next].inclusive.saturating_add(weight);
            if !frame.resolved {
                self.unresolved_weight = self.unresolved_weight.saturating_add(weight);
                if !frame.module.is_empty() {
                    let b = self
                        .unresolved_buckets
                        .entry(frame.module.clone())
                        .or_insert(0);
                    *b = b.saturating_add(weight);
                }
            }
            path.insert(frame.key.as_str());
            node = next;
        }
        // The deepest reached frame carries the exclusive weight.
        self.nodes[node].exclusive = self.nodes[node].exclusive.saturating_add(weight);
        self.total_weight = self.total_weight.saturating_add(weight);
        self.sample_count += 1;
    }

    pub fn total_weight(&self) -> u64 {
        self.total_weight
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    /// Weight attributable to frames with no module identity.
    pub fn unresolved_weight(&self) -> u64 {
        self.unresolved_weight
    }

    pub fn recursion_cuts(&self) -> usize {
        self.recursion_cuts
    }

    pub fn nodes_truncated(&self) -> bool {
        self.nodes_truncated
    }

    fn pct(&self, w: u64) -> f64 {
        if self.total_weight > 0 {
            w as f64 / self.total_weight as f64 * 100.0
        } else {
            0.0
        }
    }

    /// Top functions by inclusive weight (bounded by `limit`).
    pub fn top_by_inclusive(&self, limit: usize) -> Vec<(&str, &str, u64, f64)> {
        let mut rows: Vec<(&str, &str, u64, f64)> = self
            .nodes
            .iter()
            .enumerate()
            .skip(1) // skip the synthetic root
            .filter(|(i, _)| self.nodes[*i].inclusive > 0)
            .map(|(i, _)| {
                let inclusive = self.nodes[i].inclusive;
                (
                    self.key_of[i - 1].as_str(),
                    self.module_of[i - 1].as_str(),
                    inclusive,
                    self.pct(inclusive),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        rows.truncate(limit);
        rows
    }

    /// Top functions by exclusive weight (bounded).
    pub fn top_by_exclusive(&self, limit: usize) -> Vec<(&str, &str, u64, f64)> {
        let mut rows: Vec<(&str, &str, u64, f64)> = self
            .nodes
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(i, _)| self.nodes[*i].exclusive > 0)
            .map(|(i, _)| {
                let exclusive = self.nodes[i].exclusive;
                (
                    self.key_of[i - 1].as_str(),
                    self.module_of[i - 1].as_str(),
                    exclusive,
                    self.pct(exclusive),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        rows.truncate(limit);
        rows
    }

    /// Bounded call-tree render: DFS from the root, children sorted by
    /// inclusive weight, capped at `max_nodes` rendered nodes and `max_depth`
    /// levels; omitted children/depth are counted per node, never silent.
    pub fn call_tree_json(&self, max_nodes: usize, max_depth: usize) -> JsonValue {
        use pf_types::json::JsonValue as J;
        let mut rendered = 0usize;
        // Recursive renderer with the same caps (node 0 is the root and has
        // no key; its children are the real tree).
        fn render_child(
            trie: &StackTrie,
            node: usize,
            depth: usize,
            max_nodes: usize,
            max_depth: usize,
            rendered: &mut usize,
        ) -> JsonValue {
            let mut child_pairs: Vec<(usize, u64)> = trie.nodes[node]
                .children
                .iter()
                .map(|c| (c, trie.nodes[c].inclusive))
                .collect();
            child_pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let total_children = child_pairs.len();
            let shown = if depth >= max_depth {
                0
            } else {
                child_pairs.len().min(max_nodes.saturating_sub(*rendered))
            };
            let children: Vec<JsonValue> = child_pairs[..shown]
                .iter()
                .map(|&(c, _)| {
                    *rendered += 1;
                    render_child(trie, c, depth + 1, max_nodes, max_depth, rendered)
                })
                .collect();
            let omitted = total_children - shown;
            let mut pairs = vec![
                ("key".to_string(), J::Str(trie.key_of[node - 1].clone())),
                (
                    "module".to_string(),
                    J::Str(trie.module_of[node - 1].clone()),
                ),
                ("inclusive".to_string(), J::UInt(trie.nodes[node].inclusive)),
                (
                    "inclusive_pct".to_string(),
                    J::Float(trie.pct(trie.nodes[node].inclusive)),
                ),
                ("exclusive".to_string(), J::UInt(trie.nodes[node].exclusive)),
                ("children".to_string(), J::Array(children)),
            ];
            if omitted > 0 {
                pairs.push(("omitted_children".to_string(), J::UInt(omitted as u64)));
            }
            J::Object(pairs)
        }
        let mut child_pairs: Vec<(usize, u64)> = self.nodes[0]
            .children
            .iter()
            .map(|c| (c, self.nodes[c].inclusive))
            .collect();
        child_pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total_roots = child_pairs.len();
        let shown = child_pairs.len().min(max_nodes.saturating_sub(rendered));
        let children: Vec<JsonValue> = child_pairs[..shown]
            .iter()
            .map(|&(c, _)| {
                rendered += 1;
                render_child(self, c, 1, max_nodes, max_depth, &mut rendered)
            })
            .collect();
        let omitted_roots = total_roots - shown;
        let mut pairs = vec![("children".to_string(), J::Array(children))];
        if omitted_roots > 0 {
            pairs.push((
                "omitted_children".to_string(),
                J::UInt(omitted_roots as u64),
            ));
        }
        J::Object(pairs)
    }

    /// Render as JSON: totals, coverage, both top lists, recursion and
    /// unresolved-bucket honesty, plus the bounded call tree.
    pub fn to_json(&self, limit: usize) -> JsonValue {
        self.to_json_with_tree(limit, 2_000, 48)
    }

    pub fn to_json_with_tree(
        &self,
        limit: usize,
        tree_nodes: usize,
        tree_depth: usize,
    ) -> JsonValue {
        use pf_types::json::JsonValue as J;
        let row = |(key, module, w, pct): &(&str, &str, u64, f64)| {
            J::Object(vec![
                ("function".to_string(), J::Str((*key).to_string())),
                ("module".to_string(), J::Str((*module).to_string())),
                ("weight".to_string(), J::UInt(*w)),
                ("weight_pct".to_string(), J::Float(*pct)),
            ])
        };
        let coverage = if self.total_weight > 0 {
            1.0 - self.unresolved_weight as f64 / self.total_weight as f64
        } else {
            0.0
        };
        let mut buckets: Vec<(&String, &u64)> = self.unresolved_buckets.iter().collect();
        buckets.sort_by(|a, b| b.1.cmp(a.1));
        let buckets: Vec<JsonValue> = buckets
            .iter()
            .take(16)
            .map(|(m, w)| {
                J::Object(vec![
                    ("module".to_string(), J::Str((*m).clone())),
                    ("weight".to_string(), J::UInt(**w)),
                ])
            })
            .collect();
        J::Object(vec![
            ("total_weight".to_string(), J::UInt(self.total_weight)),
            ("samples".to_string(), J::UInt(self.sample_count as u64)),
            (
                "frame_identity_coverage".to_string(),
                J::Float((coverage * 10_000.0).round() / 10_000.0),
            ),
            (
                "recursion_cuts".to_string(),
                J::UInt(self.recursion_cuts as u64),
            ),
            ("nodes_truncated".to_string(), J::Bool(self.nodes_truncated)),
            ("unresolved_buckets".to_string(), J::Array(buckets)),
            (
                "top_inclusive".to_string(),
                J::Array(self.top_by_inclusive(limit).iter().map(row).collect()),
            ),
            (
                "top_exclusive".to_string(),
                J::Array(self.top_by_exclusive(limit).iter().map(row).collect()),
            ),
            (
                "call_tree".to_string(),
                self.call_tree_json(tree_nodes, tree_depth),
            ),
        ])
    }
}

/// A bounded node of the call tree for native consumers (GUI flamegraph).
#[derive(Clone, Debug)]
pub struct FlameNode {
    pub key: String,
    pub module: String,
    pub inclusive: u64,
    pub exclusive: u64,
    /// Children sorted by inclusive weight (desc), already bounded.
    pub children: Vec<FlameNode>,
}

/// Bounded call tree as Rust values — same caps and omission accounting
/// as [`StackTrie::call_tree_json`], for the GUI renderer.
#[derive(Clone, Debug, Default)]
pub struct FlameTree {
    pub root_children: Vec<FlameNode>,
    pub omitted_children: usize,
    /// True when any node's children were cut by the depth cap.
    pub depth_capped: bool,
}

impl StackTrie {
    /// Bounded call tree as owned values (children sorted by inclusive
    /// weight desc). Rendering stops at `max_nodes` nodes or `max_depth`
    /// levels; omissions are counted, never silent.
    // Tree construction walks a node-indexed vec; caps bound every index
    // and count, so plain arithmetic is safe here.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn flame_tree(&self, max_nodes: usize, max_depth: usize) -> FlameTree {
        let mut budget = max_nodes;
        let mut depth_capped = false;
        let mut child_pairs: Vec<(usize, u64)> = self.nodes[0]
            .children
            .iter()
            .map(|c| (c, self.nodes[c].inclusive))
            .collect();
        child_pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total_roots = child_pairs.len();
        let shown = child_pairs.len().min(budget);
        budget = budget.saturating_sub(shown);
        let root_children: Vec<FlameNode> = child_pairs[..shown]
            .iter()
            .map(|&(c, _)| self.build_node(c, 1, max_depth, &mut budget, &mut depth_capped))
            .collect();
        FlameTree {
            omitted_children: total_roots.saturating_sub(shown),
            root_children,
            depth_capped,
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn build_node(
        &self,
        node: usize,
        depth: usize,
        max_depth: usize,
        budget: &mut usize,
        depth_capped: &mut bool,
    ) -> FlameNode {
        let mut child_pairs: Vec<(usize, u64)> = self.nodes[node]
            .children
            .iter()
            .map(|c| (c, self.nodes[c].inclusive))
            .collect();
        child_pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let shown = if depth >= max_depth {
            *depth_capped = *depth_capped || !child_pairs.is_empty();
            0
        } else {
            child_pairs.len().min(*budget)
        };
        *budget -= shown;
        let children: Vec<FlameNode> = child_pairs[..shown]
            .iter()
            .map(|&(c, _)| self.build_node(c, depth + 1, max_depth, budget, depth_capped))
            .collect();
        FlameNode {
            key: self.key_of[node - 1].clone(),
            module: self.module_of[node - 1].clone(),
            inclusive: self.nodes[node].inclusive,
            exclusive: self.nodes[node].exclusive,
            children,
        }
    }
}

/// Convenience: aggregate and render in one step.
pub fn aggregate_stacks(samples: &[SampledStack]) -> StackTrie {
    let mut trie = StackTrie::new();
    trie.aggregate(samples);
    trie
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(key: &str, module: &str, resolved: bool) -> StackFrame {
        StackFrame {
            key: key.into(),
            module: module.into(),
            resolved,
            file: None,
            line: None,
            offset: 0,
        }
    }

    fn stack(weight: u64, frames: &[(&str, &str, bool)]) -> SampledStack {
        SampledStack {
            frames: frames.iter().map(|(k, m, r)| frame(k, m, *r)).collect(),
            weight,
        }
    }

    #[test]
    fn inclusive_and_exclusive_weights_are_correct() {
        // 10 samples: main -> work -> burn ; 5 samples: main -> work
        let samples = vec![
            stack(
                10,
                &[
                    ("main", "app", true),
                    ("work", "app", true),
                    ("burn", "app", true),
                ],
            ),
            stack(5, &[("main", "app", true), ("work", "app", true)]),
        ];
        let trie = aggregate_stacks(&samples);
        assert_eq!(trie.total_weight(), 15);
        assert_eq!(trie.sample_count(), 2);
        let inc: Vec<_> = trie
            .top_by_inclusive(10)
            .into_iter()
            .map(|(k, _, w, _)| (k.to_string(), w))
            .collect();
        assert!(inc.contains(&("main".to_string(), 15)), "{inc:?}");
        assert!(inc.contains(&("work".to_string(), 15)), "{inc:?}");
        assert!(inc.contains(&("burn".to_string(), 10)), "{inc:?}");
        // exclusive: only leaf frames
        let exc: Vec<_> = trie
            .top_by_exclusive(10)
            .into_iter()
            .map(|(k, _, w, _)| (k.to_string(), w))
            .collect();
        assert!(exc.contains(&("burn".to_string(), 10)), "{exc:?}");
        assert!(exc.contains(&("work".to_string(), 5)), "{exc:?}");
        assert!(!exc.contains(&("main".to_string(), 15)), "{exc:?}");
    }

    #[test]
    fn recursion_is_cut_not_infinite_and_counted() {
        // Mutually recursive A -> B -> A -> B … 10 samples.
        let samples = vec![stack(
            10,
            &[
                ("a", "app", true),
                ("b", "app", true),
                ("a", "app", true),
                ("b", "app", true),
            ],
        )];
        let trie = aggregate_stacks(&samples);
        assert_eq!(trie.total_weight(), 10);
        assert_eq!(trie.recursion_cuts(), 1, "one cut per sample path");
        // Tree stays finite: a and b, one node each.
        let inc = trie.top_by_inclusive(10);
        assert_eq!(inc.len(), 2, "{inc:?}");
        assert!(inc.iter().all(|(_, _, w, _)| *w == 10));
        // The cut node (b) carries the exclusive weight.
        let exc = trie.top_by_exclusive(10);
        assert_eq!(exc.len(), 1);
        assert_eq!(exc[0].0, "b");
        assert_eq!(exc[0].2, 10);
    }

    #[test]
    fn node_cap_truncates_honestly() {
        let mut samples = Vec::new();
        for i in 0..10 {
            samples.push(stack(1, &[(&format!("f{i}"), "app", true)]));
        }
        let mut trie = StackTrie::with_node_cap(4);
        trie.aggregate(&samples);
        assert!(trie.nodes_truncated());
        // Every node that exists has exact weights for the samples it saw.
        assert_eq!(trie.total_weight(), 10);
        assert!(trie.top_by_inclusive(100).len() < 10);
    }

    #[test]
    fn unresolved_frames_lower_coverage_and_bucket_by_module() {
        let samples = vec![
            stack(8, &[("app", "app", true), ("lib", "libfoo.so", false)]),
            stack(2, &[("app", "app", true), ("work", "app", true)]),
        ];
        let trie = aggregate_stacks(&samples);
        let json = trie.to_json(10);
        let coverage = json
            .get("frame_identity_coverage")
            .and_then(|v| v.as_f64())
            .unwrap();
        // Coverage is weight of stacks with no unresolved frame link: the
        // 8-weight sample carries one unresolved frame → 1 - 8/10 = 0.2.
        assert!((coverage - 0.2).abs() < 1e-9, "{coverage}");
        let buckets = json
            .get("unresolved_buckets")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(
            buckets[0].get("module").and_then(|m| m.as_str()),
            Some("libfoo.so")
        );
        assert_eq!(buckets[0].get("weight").and_then(|w| w.as_u64()), Some(8));
    }

    #[test]
    fn bounded_top_lists() {
        let samples: Vec<_> = (0..50)
            .map(|i| stack(1, &[(&format!("f{i}"), "app", true)]))
            .collect();
        let trie = aggregate_stacks(&samples);
        assert_eq!(trie.top_by_inclusive(5).len(), 5);
        let json = trie.to_json(5);
        assert_eq!(
            json.get("top_inclusive")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(5)
        );
    }

    #[test]
    fn call_tree_is_bounded_and_reports_omissions() {
        // A wide tree: 40 root-level branches.
        let samples: Vec<_> = (0..40)
            .map(|i| stack(1, &[(&format!("f{i}"), "app", true)]))
            .collect();
        let trie = aggregate_stacks(&samples);
        let tree = trie.call_tree_json(10, 48);
        let children = tree.get("children").and_then(|c| c.as_array()).unwrap();
        assert_eq!(children.len(), 10, "tree render bounded to max_nodes");
        assert_eq!(
            tree.get("omitted_children").and_then(|o| o.as_u64()),
            Some(30),
            "omissions are counted, not silent"
        );
        // Depth cap: 100-deep chain, depth 5.
        let chain: SampledStack = SampledStack {
            frames: (0..100)
                .map(|i| frame(&format!("d{i}"), "app", true))
                .collect(),
            weight: 1,
        };
        let mut trie2 = StackTrie::new();
        trie2.aggregate(&[chain]);
        let tree2 = trie2.call_tree_json(2_000, 5);
        // Walk down: depth must stop at 5 levels of children.
        let mut depth = 0;
        let mut cur = &tree2;
        while let Some(kids) = cur.get("children").and_then(|c| c.as_array()) {
            if kids.is_empty() {
                break;
            }
            depth += 1;
            cur = &kids[0];
        }
        assert_eq!(depth, 5, "depth cap respected");
    }

    #[test]
    fn flip_converts_callee_first_to_root_first() {
        let callee_first = stack(1, &[("leaf", "app", true), ("root", "app", true)]);
        let flipped = callee_first.flip();
        assert_eq!(flipped.frames[0].key, "root");
    }

    #[test]
    fn from_resolved_uses_symbol_keys_and_honest_unresolved() {
        let s = SampledStack::from_resolved(
            &[
                ("app".into(), "main".into(), 0x10, None, None),
                ("libfoo.so".into(), "".into(), 0x40, None, None),
                ("".into(), "".into(), 0x0, None, None),
            ],
            1,
        );
        assert_eq!(s.frames[0].key, "main");
        assert!(s.frames[0].resolved);
        assert_eq!(s.frames[1].key, "libfoo.so+0x40");
        assert!(s.frames[1].resolved);
        assert_eq!(s.frames[2].key, "<unresolved>");
        assert!(!s.frames[2].resolved);
    }
}

/// One function's shift between two aggregated profiles (PF-1105): weight
/// deltas with honest presence flags (a function absent from one side has
/// weight 0 there — reported as gained/lost, never averaged away).
#[derive(Clone, Debug)]
pub struct FunctionShift {
    pub function: String,
    pub baseline_weight: u64,
    pub candidate_weight: u64,
    /// candidate − baseline (saturating on both sides).
    pub delta: i64,
}

/// Compare two aggregated profiles by exclusive (leaf) weight: every
/// function appearing in either top-set is reported with both weights and
/// the delta, sorted by |delta| descending then name. Bounded to the top
/// `limit` functions of each side before diffing.
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_wrap)]
pub fn compare_top_functions(
    baseline: &StackTrie,
    candidate: &StackTrie,
    limit: usize,
) -> Vec<FunctionShift> {
    let collect = |trie: &StackTrie| -> HashMap<String, u64> {
        trie.top_by_exclusive(limit)
            .iter()
            .map(|(k, _, w, _)| (k.to_string(), *w))
            .collect()
    };
    let base = collect(baseline);
    let cand = collect(candidate);
    let mut names: Vec<String> = base.keys().chain(cand.keys()).cloned().collect();
    names.sort();
    names.dedup();
    let mut out: Vec<FunctionShift> = names
        .into_iter()
        .map(|name| {
            let b = base.get(&name).copied().unwrap_or(0);
            let c = cand.get(&name).copied().unwrap_or(0);
            FunctionShift {
                function: name,
                baseline_weight: b,
                candidate_weight: c,
                delta: c as i64 - b as i64,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.delta
            .abs()
            .cmp(&a.delta.abs())
            .then_with(|| a.function.cmp(&b.function))
    });
    out
}

/// Render shifts as JSON (bounded by the diff length).
pub fn shifts_to_json(shifts: &[FunctionShift]) -> JsonValue {
    use pf_types::json::JsonValue as J;
    J::Array(
        shifts
            .iter()
            .map(|s| {
                J::Object(vec![
                    ("function".to_string(), J::Str(s.function.clone())),
                    ("baseline".to_string(), J::UInt(s.baseline_weight)),
                    ("candidate".to_string(), J::UInt(s.candidate_weight)),
                    ("delta".to_string(), J::Int(s.delta)),
                ])
            })
            .collect(),
    )
}
