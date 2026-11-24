//! Expert-major copies: one expert, one read.
//!
//! In a GGUF, an expert's slices of a layer (`ffn_gate_exps`, `ffn_up_exps`,
//! `ffn_down_exps`, or a fused `ffn_gate_up_exps` plus down) sit in different tensors far
//! apart, so fetching one expert costs one seek per slice. On a hard disk that is most of
//! the time spent. A copy with this layout stores each expert's slices side by side, and
//! orders the experts of a layer so the ones that fire together are neighbours, so the
//! experts one token picks often merge into a single read.
//!
//! The mount still shows the original file. Only the copy is rearranged: the copy is the
//! original's pages in another order, and this map turns an original offset into a copy
//! offset. Every slice is stored as the whole pages it touches, so a page shared by two
//! slices is stored twice (once with each) and any original page can be read from the
//! copy in one piece, page aligned, ready for direct reads and for the page cache.
//!
//! ```text
//! copy space:  [L0 e17: gate up down][L0 e3: gate up down] ... [L47 ...][everything else]
//! ```

use std::fs;
use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::trace::Trace;

pub const PAGE: u64 = 4096;

fn floor_page(x: u64) -> u64 {
    x / PAGE * PAGE
}

fn ceil_page(x: u64) -> u64 {
    x.div_ceil(PAGE) * PAGE
}

// ---------------------------------------------------------------------------
// Where the experts are in the model file

#[derive(Clone, Debug, PartialEq)]
pub struct ExpertTensor {
    /// File offset of expert 0's slice.
    pub offs: u64,
    /// Bytes per expert.
    pub stride: u64,
    pub n_expert: u64,
}

#[derive(Clone, Debug)]
pub struct ExpertMap {
    pub file_size: u64,
    /// Per layer, its expert tensors in file order (gate, up, down or gate_up, down).
    pub layers: Vec<Vec<ExpertTensor>>,
}

impl ExpertMap {
    pub fn experts(&self) -> usize {
        self.layers.iter().filter_map(|l| l.first()).map(|t| t.n_expert as usize).max().unwrap_or(0)
    }

    /// Bytes of one expert in one layer (all its slices).
    pub fn expert_bytes(&self, layer: usize) -> u64 {
        self.layers.get(layer).map_or(0, |l| l.iter().map(|t| t.stride).sum())
    }

    /// The page-rounded byte ranges of expert `e` in `layer`, one per slice.
    pub fn slices(&self, layer: usize, e: u32) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.layers[layer].iter().filter(move |t| (e as u64) < t.n_expert).map(move |t| {
            let a = t.offs + e as u64 * t.stride;
            (floor_page(a), ceil_page(a + t.stride))
        })
    }

    /// A stable fingerprint of where every expert slice lives, so a copy laid out for one
    /// file is never read as another.
    pub fn fingerprint(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |v: u64| {
            for b in v.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        };
        eat(self.file_size);
        for (l, ts) in self.layers.iter().enumerate() {
            eat(l as u64);
            for t in ts {
                eat(t.offs);
                eat(t.stride);
                eat(t.n_expert);
            }
        }
        h
    }

    /// Reads the expert tensors' offsets from a GGUF file's header (version 2 or 3).
    pub fn from_gguf(path: &Path) -> Result<ExpertMap, String> {
        let file_size = fs::metadata(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?.len();
        let f = fs::File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
        let mut r = Gguf { r: BufReader::with_capacity(1 << 20, f), pos: 0 };
        let bad = |what: &str| format!("{} is not a GGUF file this version understands ({what}).", path.display());
        let magic = r.u32().map_err(|_| bad("too short"))?;
        if magic != 0x4655_4747 {
            return Err(bad("no GGUF magic"));
        }
        let version = r.u32().map_err(|_| bad("too short"))?;
        if !(2..=3).contains(&version) {
            return Err(bad(&format!("version {version}")));
        }
        let n_tensors = r.u64().map_err(|_| bad("too short"))?;
        let n_kv = r.u64().map_err(|_| bad("too short"))?;
        let mut alignment = 32u64;
        for _ in 0..n_kv {
            let key = r.string().map_err(|_| bad("truncated metadata"))?;
            let ty = r.u32().map_err(|_| bad("truncated metadata"))?;
            if key == "general.alignment" && ty == 4 {
                alignment = r.u32().map_err(|_| bad("truncated metadata"))? as u64;
            } else {
                r.skip_value(ty).map_err(|e| bad(&e.to_string()))?;
            }
        }
        if alignment == 0 || alignment % 8 != 0 {
            return Err(bad("odd alignment"));
        }
        struct Info {
            name: String,
            dims: Vec<u64>,
            ty: u32,
            offs: u64,
        }
        let mut infos = Vec::with_capacity(n_tensors.min(1 << 20) as usize);
        for _ in 0..n_tensors {
            let name = r.string().map_err(|_| bad("truncated tensor list"))?;
            let nd = r.u32().map_err(|_| bad("truncated tensor list"))?;
            if nd > 8 {
                return Err(bad("tensor with more than 8 dimensions"));
            }
            let mut dims = Vec::with_capacity(nd as usize);
            for _ in 0..nd {
                dims.push(r.u64().map_err(|_| bad("truncated tensor list"))?);
            }
            let ty = r.u32().map_err(|_| bad("truncated tensor list"))?;
            let offs = r.u64().map_err(|_| bad("truncated tensor list"))?;
            infos.push(Info { name, dims, ty, offs });
        }
        let data = r.pos.div_ceil(alignment) * alignment;
        let mut layers: Vec<Vec<ExpertTensor>> = Vec::new();
        for t in &infos {
            let Some(rest) = t.name.strip_prefix("blk.") else { continue };
            let Some((il, kind)) = rest.split_once('.') else { continue };
            let Ok(il) = il.parse::<usize>() else { continue };
            if !matches!(kind, "ffn_gate_exps.weight" | "ffn_up_exps.weight" | "ffn_down_exps.weight" | "ffn_gate_up_exps.weight") {
                continue;
            }
            if t.dims.len() < 3 || t.dims[2] < 2 {
                continue;
            }
            let (blck, tsize) = type_size(t.ty).ok_or_else(|| bad(&format!("tensor {} has type {} that Corsia does not know", t.name, t.ty)))?;
            if t.dims[0] % blck != 0 {
                return Err(bad(&format!("tensor {} rows do not fill whole blocks", t.name)));
            }
            let row = t.dims[0] / blck * tsize;
            let per_expert = row * t.dims[1] * t.dims[3..].iter().product::<u64>();
            let offs = data + t.offs;
            if offs + per_expert * t.dims[2] > file_size {
                return Err(bad(&format!("tensor {} runs past the end of the file", t.name)));
            }
            if il >= layers.len() {
                layers.resize(il + 1, Vec::new());
            }
            layers[il].push(ExpertTensor { offs, stride: per_expert, n_expert: t.dims[2] });
        }
        for l in &mut layers {
            l.sort_by_key(|t| t.offs);
        }
        if layers.iter().all(|l| l.is_empty()) {
            return Err(format!("{} has no expert tensors (not a mixture-of-experts model?).", path.display()));
        }
        Ok(ExpertMap { file_size, layers })
    }

    /// A made-up model shaped like a GGUF, for tests and for replays without a model file:
    /// a header, then per layer some other weights and three expert tensors. Offsets are
    /// 32-byte aligned and slices are not whole pages, like real files.
    pub fn synthetic(layers: usize, experts: u64, slice_bytes: u64) -> ExpertMap {
        let align = |x: u64| x.div_ceil(32) * 32;
        let mut pos = align(5 << 20 | 1234);
        let mut out = Vec::with_capacity(layers);
        for _ in 0..layers {
            pos = align(pos + (3 << 20) + 352);
            let mut ts = Vec::new();
            for _ in 0..3 {
                ts.push(ExpertTensor { offs: pos, stride: slice_bytes, n_expert: experts });
                pos = align(pos + slice_bytes * experts);
            }
            out.push(ts);
        }
        ExpertMap { file_size: pos + (300 << 20) + 77, layers: out }
    }
}

/// (elements per block, bytes per block) for ggml tensor types.
fn type_size(ty: u32) -> Option<(u64, u64)> {
    Some(match ty {
        0 => (1, 4),     // F32
        1 => (1, 2),     // F16
        2 => (32, 18),   // Q4_0
        3 => (32, 20),   // Q4_1
        6 => (32, 22),   // Q5_0
        7 => (32, 24),   // Q5_1
        8 => (32, 34),   // Q8_0
        9 => (32, 36),   // Q8_1
        10 => (256, 84), // Q2_K
        11 => (256, 110),
        12 => (256, 144),
        13 => (256, 176),
        14 => (256, 210),
        15 => (256, 292),
        16 => (256, 66), // IQ2_XXS
        17 => (256, 74),
        18 => (256, 98),
        19 => (256, 50),
        20 => (32, 18), // IQ4_NL
        21 => (256, 110),
        22 => (256, 82),
        23 => (256, 136),
        24 => (1, 1),
        25 => (1, 2),
        26 => (1, 4),
        27 => (1, 8),
        28 => (1, 8),
        29 => (256, 56), // IQ1_M
        30 => (1, 2),    // BF16
        34 => (256, 54), // TQ1_0
        35 => (256, 66), // TQ2_0
        39 => (32, 17),  // MXFP4
        _ => return None,
    })
}

struct Gguf<R: Read> {
    r: R,
    pos: u64,
}

impl<R: Read> Gguf<R> {
    fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut b = [0u8; N];
        self.r.read_exact(&mut b)?;
        self.pos += N as u64;
        Ok(b)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn skip(&mut self, n: u64) -> io::Result<()> {
        let got = io::copy(&mut (&mut self.r).take(n), &mut io::sink())?;
        self.pos += got;
        if got != n {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated"));
        }
        Ok(())
    }
    fn string(&mut self) -> io::Result<String> {
        let n = self.u64()?;
        if n > 1 << 24 {
            return Err(io::Error::other("string too long"));
        }
        let mut b = vec![0u8; n as usize];
        self.r.read_exact(&mut b)?;
        self.pos += n;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }
    fn skip_value(&mut self, ty: u32) -> io::Result<()> {
        match ty {
            0 | 1 | 7 => self.skip(1),
            2 | 3 => self.skip(2),
            4..=6 => self.skip(4),
            10..=12 => self.skip(8),
            8 => {
                let n = self.u64()?;
                self.skip(n)
            }
            9 => {
                let inner = self.u32()?;
                let n = self.u64()?;
                let fixed = match inner {
                    0 | 1 | 7 => Some(1),
                    2 | 3 => Some(2),
                    4..=6 => Some(4),
                    10..=12 => Some(8),
                    _ => None,
                };
                match fixed {
                    Some(w) => self.skip(n * w),
                    None => {
                        for _ in 0..n {
                            self.skip_value(inner)?;
                        }
                        Ok(())
                    }
                }
            }
            _ => Err(io::Error::other(format!("unknown metadata type {ty}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Which experts sit next to each other

/// Orders each layer's experts so the ones that fire together are neighbours, learned from
/// `tokens` of a routing trace. The number of reads a layer costs is the number of runs
/// its wanted experts form, which is the count minus the adjacent pairs among them, so
/// each order maximises the co-firing weight of neighbouring pairs: greedy path building
/// (heaviest pairs first), then 2-opt until nothing improves.
pub fn cofire_order(trace: &Trace, tokens: std::ops::Range<usize>, experts: usize) -> Vec<Vec<u32>> {
    let mut orders = Vec::with_capacity(trace.layers);
    for l in 0..trace.layers {
        let mut w = vec![0u32; experts * experts];
        let mut pop = vec![0u32; experts];
        for tok in trace.tokens.get(tokens.clone()).unwrap_or(&[]) {
            let Some(group) = tok.get(l) else { continue };
            for (i, &a) in group.iter().enumerate() {
                if (a as usize) >= experts {
                    continue;
                }
                pop[a as usize] += 1;
                for &b in &group[i + 1..] {
                    if (b as usize) < experts && a != b {
                        w[a as usize * experts + b as usize] += 1;
                        w[b as usize * experts + a as usize] += 1;
                    }
                }
            }
        }
        orders.push(order_one(&w, &pop, experts));
    }
    orders
}

fn order_one(w: &[u32], pop: &[u32], n: usize) -> Vec<u32> {
    // Greedy: heaviest pairs first, joining path ends of different paths.
    let mut pairs: Vec<(u32, usize, usize)> = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            if w[a * n + b] > 0 {
                pairs.push((w[a * n + b], a, b));
            }
        }
    }
    pairs.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
    let mut deg = vec![0u8; n];
    let mut link: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut root: Vec<usize> = (0..n).collect();
    fn find(root: &mut [usize], mut x: usize) -> usize {
        while root[x] != x {
            root[x] = root[root[x]];
            x = root[x];
        }
        x
    }
    for (_, a, b) in pairs {
        if deg[a] >= 2 || deg[b] >= 2 {
            continue;
        }
        let (ra, rb) = (find(&mut root, a), find(&mut root, b));
        if ra == rb {
            continue;
        }
        root[ra] = rb;
        deg[a] += 1;
        deg[b] += 1;
        link[a].push(b);
        link[b].push(a);
    }
    // Walk each path from an end; busiest paths first.
    let mut seen = vec![false; n];
    let mut paths: Vec<(u64, Vec<u32>)> = Vec::new();
    for start in 0..n {
        if seen[start] || deg[start] > 1 {
            continue;
        }
        let mut path = Vec::new();
        let (mut prev, mut cur) = (usize::MAX, start);
        loop {
            seen[cur] = true;
            path.push(cur as u32);
            let Some(&next) = link[cur].iter().find(|&&x| x != prev && !seen[x]) else { break };
            prev = cur;
            cur = next;
        }
        let heat = path.iter().map(|&e| pop[e as usize] as u64).sum();
        paths.push((heat, path));
    }
    paths.sort_by(|a, b| b.0.cmp(&a.0).then(a.1[0].cmp(&b.1[0])));
    let mut order: Vec<u32> = paths.into_iter().flat_map(|p| p.1).collect();
    debug_assert_eq!(order.len(), n);
    // 2-opt: reversing order[i..=j] swaps edges (i-1,i),(j,j+1) for (i-1,j),(i,j+1).
    let wt = |a: u32, b: u32| w[a as usize * n + b as usize] as i64;
    let mut better = true;
    let mut rounds = 0;
    while better && rounds < 50 {
        better = false;
        rounds += 1;
        for i in 1..n {
            for j in i + 1..n {
                let before = wt(order[i - 1], order[i]) + if j + 1 < n { wt(order[j], order[j + 1]) } else { 0 };
                let after = wt(order[i - 1], order[j]) + if j + 1 < n { wt(order[i], order[j + 1]) } else { 0 };
                if after > before {
                    order[i..=j].reverse();
                    better = true;
                }
            }
        }
    }
    order
}

/// Expected adjacent pairs per token: the order's co-firing weight over the tokens.
pub fn adjacency(trace: &Trace, tokens: std::ops::Range<usize>, orders: &[Vec<u32>]) -> f64 {
    let mut pos: Vec<Vec<usize>> = orders.iter().map(|o| vec![0; o.len()]).collect();
    for (l, o) in orders.iter().enumerate() {
        for (i, &e) in o.iter().enumerate() {
            pos[l][e as usize] = i;
        }
    }
    let (mut pairs, mut n) = (0u64, 0u64);
    for tok in trace.tokens.get(tokens).unwrap_or(&[]) {
        n += 1;
        for (l, group) in tok.iter().enumerate().take(orders.len()) {
            let mut p: Vec<usize> = group.iter().filter_map(|&e| pos[l].get(e as usize).copied()).collect();
            p.sort_unstable();
            pairs += p.windows(2).filter(|w| w[1] == w[0] + 1).count() as u64;
        }
    }
    pairs as f64 / n.max(1) as f64
}

pub fn identity_order(map: &ExpertMap) -> Vec<Vec<u32>> {
    map.layers.iter().map(|l| (0..l.first().map_or(0, |t| t.n_expert as u32)).collect()).collect()
}

// ---------------------------------------------------------------------------
// The remap

/// A run of original pages stored contiguously in the copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ext {
    pub orig: u64,
    pub copy: u64,
    pub len: u64,
}

/// A piece of a request: `len` bytes at original offset `orig` live at copy offset `copy`.
pub type Seg = Ext;

pub struct ExpertLayout {
    /// Bytes of the copy (page multiple): the original plus the pages stored twice.
    pub space: u64,
    pub fingerprint: u64,
    pub orders: Vec<Vec<u32>>,
    /// Sorted by (orig, end). Expert extents may share their first or last page with a
    /// neighbour; the rest of the file is split into extents that overlap nothing.
    by_orig: Vec<Ext>,
    /// max end over by_orig[..=i], to find every extent holding a page.
    max_end: Vec<u64>,
    /// Sorted by copy offset; extents tile [0, space) without gaps or overlaps.
    by_copy: Vec<Ext>,
}

impl ExpertLayout {
    pub fn new(map: &ExpertMap, orders: Vec<Vec<u32>>) -> Result<ExpertLayout, String> {
        if orders.len() != map.layers.len() {
            return Err(format!("the order names {} layers, the model has {}", orders.len(), map.layers.len()));
        }
        for (l, (o, ts)) in orders.iter().zip(&map.layers).enumerate() {
            let n = ts.first().map_or(0, |t| t.n_expert) as usize;
            let mut seen = vec![false; n];
            if o.len() != n || o.iter().any(|&e| (e as usize) >= n || std::mem::replace(&mut seen[e as usize], true)) {
                return Err(format!("layer {l}'s order is not each of its {n} experts once"));
            }
        }
        let mut by_copy = Vec::new();
        let mut cursor = 0u64;
        for (l, o) in orders.iter().enumerate() {
            for &e in o {
                for (a, b) in map.slices(l, e) {
                    by_copy.push(Ext { orig: a, copy: cursor, len: b - a });
                    cursor += b - a;
                }
            }
        }
        // Everything no expert slice touches, in file order.
        let mut covered: Vec<(u64, u64)> = by_copy.iter().map(|x| (x.orig, x.orig + x.len)).collect();
        covered.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for r in covered {
            match merged.last_mut() {
                Some(m) if r.0 <= m.1 => m.1 = m.1.max(r.1),
                _ => merged.push(r),
            }
        }
        let end = ceil_page(map.file_size);
        let mut p = 0u64;
        for (a, b) in merged.iter().copied().chain(std::iter::once((end, end))) {
            if a > p {
                by_copy.push(Ext { orig: p, copy: cursor, len: a - p });
                cursor += a - p;
            }
            p = p.max(b);
        }
        let mut by_orig = by_copy.clone();
        by_orig.sort_unstable_by_key(|x| (x.orig, x.orig + x.len));
        let mut max_end = Vec::with_capacity(by_orig.len());
        let mut m = 0;
        for x in &by_orig {
            m = m.max(x.orig + x.len);
            max_end.push(m);
        }
        Ok(ExpertLayout { space: cursor, fingerprint: map.fingerprint(), orders, by_orig, max_end, by_copy })
    }

    /// Splits original bytes [a, b) (page aligned) into copy segments: at each page the
    /// extent that runs furthest, so a slice asked for alone is one segment. Neighbouring
    /// segments that are contiguous in the copy are joined.
    pub fn cover(&self, a: u64, b: u64, out: &mut Vec<Seg>) {
        let mut p = a;
        while p < b {
            let idx = self.by_orig.partition_point(|x| x.orig <= p);
            let mut best: Option<&Ext> = None;
            let mut j = idx;
            while j > 0 && self.max_end[j - 1] > p {
                j -= 1;
                let x = &self.by_orig[j];
                if x.orig + x.len > p && best.is_none_or(|bx| x.orig + x.len > bx.orig + bx.len) {
                    best = Some(x);
                }
            }
            let Some(x) = best else { break };
            let to = (x.orig + x.len).min(b);
            let seg = Seg { orig: p, copy: x.copy + (p - x.orig), len: to - p };
            match out.last_mut() {
                Some(last) if last.orig + last.len == seg.orig && last.copy + last.len == seg.copy => last.len += seg.len,
                _ => out.push(seg),
            }
            p = to;
        }
    }

    /// For filling the copy: the original ranges that make up copy bytes [c0, c1), as
    /// (original offset, offset inside [c0, c1), length).
    pub fn sources_of(&self, c0: u64, c1: u64) -> Vec<(u64, u64, u64)> {
        let mut out = Vec::new();
        let mut i = self.by_copy.partition_point(|x| x.copy + x.len <= c0);
        while let Some(x) = self.by_copy.get(i) {
            if x.copy >= c1 {
                break;
            }
            let from = x.copy.max(c0);
            let to = (x.copy + x.len).min(c1);
            out.push((x.orig + (from - x.copy), from - c0, to - from));
            i += 1;
        }
        out
    }

    /// The layout file kept next to a copy, so the copy is read the way it was written.
    pub fn to_text(&self) -> String {
        let mut s = format!("corsia-layout experts 1\nfingerprint {:016x}\nspace {}\n", self.fingerprint, self.space);
        for (l, o) in self.orders.iter().enumerate() {
            let ids: Vec<String> = o.iter().map(|e| e.to_string()).collect();
            s.push_str(&format!("order {l} {}\n", ids.join(",")));
        }
        s
    }

    /// Rebuilds a layout from its file; fails when it was made for another model file.
    pub fn from_text(map: &ExpertMap, text: &str) -> Result<ExpertLayout, String> {
        let mut lines = text.lines();
        if lines.next() != Some("corsia-layout experts 1") {
            return Err("not an expert layout file".into());
        }
        let mut orders: Vec<Vec<u32>> = vec![Vec::new(); map.layers.len()];
        let mut fp = None;
        for line in lines {
            let mut it = line.split_whitespace();
            match it.next() {
                Some("fingerprint") => fp = it.next().and_then(|h| u64::from_str_radix(h, 16).ok()),
                Some("order") => {
                    let l: usize = it.next().and_then(|v| v.parse().ok()).ok_or("bad order line")?;
                    let ids = it.next().unwrap_or("");
                    let o: Result<Vec<u32>, _> = ids.split(',').filter(|s| !s.is_empty()).map(|s| s.parse::<u32>()).collect();
                    *orders.get_mut(l).ok_or("order for a layer the model does not have")? = o.map_err(|_| "bad expert id")?;
                }
                _ => {}
            }
        }
        if fp != Some(map.fingerprint()) {
            return Err("the copy was laid out for a different model file".into());
        }
        ExpertLayout::new(map, orders)
    }
}

/// Reads to issue for some copy segments: sorted by copy offset, then joined when the gap
/// between them is at most `gap` bytes (reading through it is cheaper than a seek) and the
/// read stays within `max_span`. Returns (copy start, copy end, the segments in it).
pub fn group_reads(segs: &mut [Seg], gap: u64, max_span: u64) -> Vec<(u64, u64, Vec<Seg>)> {
    segs.sort_unstable_by_key(|s| (s.copy, s.orig));
    let mut out: Vec<(u64, u64, Vec<Seg>)> = Vec::new();
    for s in segs.iter() {
        match out.last_mut() {
            Some(g) if s.copy <= g.1 + gap && (s.copy + s.len).max(g.1) - g.0 <= max_span => {
                g.1 = g.1.max(s.copy + s.len);
                g.2.push(*s);
            }
            _ => out.push((s.copy, s.copy + s.len, vec![*s])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_file(map: &ExpertMap) -> Vec<u8> {
        (0..map.file_size).map(|i| (i.wrapping_mul(2654435761) >> 7) as u8).collect()
    }

    /// The copy a layout describes, built byte by byte from the original.
    fn build_copy(lay: &ExpertLayout, orig: &[u8]) -> Vec<u8> {
        let mut copy = vec![0u8; lay.space as usize];
        for (o, at, len) in lay.sources_of(0, lay.space) {
            for k in 0..len {
                copy[(at + k) as usize] = *orig.get((o + k) as usize).unwrap_or(&0);
            }
        }
        copy
    }

    fn small_map() -> ExpertMap {
        // 3 layers × 6 experts, slices of 5000 bytes (not page multiples).
        let mut m = ExpertMap::synthetic(3, 6, 5000);
        // Shrink the filler so the test file stays small.
        let mut pos = 1000u64;
        for l in &mut m.layers {
            pos += 7000;
            for t in l.iter_mut() {
                t.offs = pos.div_ceil(32) * 32;
                pos = t.offs + t.stride * t.n_expert;
            }
        }
        m.file_size = pos + 9999;
        m
    }

    #[test]
    fn copy_reads_match_the_original() {
        let map = small_map();
        let orig = fake_file(&map);
        let orders = vec![vec![3, 0, 5, 1, 4, 2], vec![0, 1, 2, 3, 4, 5], vec![5, 4, 3, 2, 1, 0]];
        let lay = ExpertLayout::new(&map, orders).unwrap();
        assert_eq!(lay.space % PAGE, 0);
        assert!(lay.space >= ceil_page(map.file_size));
        let copy = build_copy(&lay, &orig);
        // Every page-aligned range of the original reads back the same through the copy.
        let end = ceil_page(map.file_size);
        let mut padded = orig.clone();
        padded.resize(end as usize, 0);
        let mut rng = 7u64;
        for _ in 0..2000 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = (rng >> 20) % (end / PAGE) * PAGE;
            let n = 1 + (rng >> 50) % 12;
            let b = (a + n * PAGE).min(end);
            let mut segs = Vec::new();
            lay.cover(a, b, &mut segs);
            let mut got = Vec::new();
            let mut p = a;
            for s in &segs {
                assert_eq!(s.orig, p, "segments are in order and leave no hole");
                got.extend_from_slice(&copy[s.copy as usize..(s.copy + s.len) as usize]);
                p += s.len;
            }
            assert_eq!(p, b);
            assert_eq!(got, padded[a as usize..b as usize]);
        }
    }

    #[test]
    fn one_expert_is_one_read() {
        let map = small_map();
        let lay = ExpertLayout::new(&map, identity_order(&map)).unwrap();
        for l in 0..3 {
            for e in 0..6 {
                let mut segs = Vec::new();
                for (a, b) in map.slices(l, e) {
                    lay.cover(a, b, &mut segs);
                }
                let reads = group_reads(&mut segs, 0, u64::MAX);
                assert_eq!(reads.len(), 1, "layer {l} expert {e}: {reads:?}");
            }
        }
        // Two neighbours in the order merge; the original layout would need six reads.
        let mut segs = Vec::new();
        for e in [2, 3] {
            for (a, b) in map.slices(1, e) {
                lay.cover(a, b, &mut segs);
            }
        }
        assert_eq!(group_reads(&mut segs, PAGE, u64::MAX).len(), 1);
    }

    #[test]
    fn layout_file_round_trips_and_refuses_other_models() {
        let map = small_map();
        let lay = ExpertLayout::new(&map, vec![vec![1, 0, 2, 3, 4, 5], vec![0, 1, 2, 3, 4, 5], vec![5, 4, 3, 2, 1, 0]]).unwrap();
        let back = ExpertLayout::from_text(&map, &lay.to_text()).unwrap();
        assert_eq!(back.orders, lay.orders);
        assert_eq!(back.space, lay.space);
        let mut other = map.clone();
        other.layers[0][0].offs += 32;
        assert!(ExpertLayout::from_text(&other, &lay.to_text()).is_err());
        assert!(ExpertLayout::new(&map, vec![vec![0, 0, 1, 2, 3, 4], vec![0; 6], vec![0; 6]]).is_err());
    }

    #[test]
    fn cofiring_experts_become_neighbours() {
        // Experts 1 and 6, and 3 and 7, always fire together.
        let tokens: Vec<Vec<Vec<u16>>> = (0..50).map(|i| vec![if i % 2 == 0 { vec![1, 6] } else { vec![3, 7] }]).collect();
        let trace = Trace { layers: 1, experts: 8, topk: 2, source: "test".into(), tokens };
        let o = cofire_order(&trace, 0..50, 8);
        let at = |e: u32| o[0].iter().position(|&x| x == e).unwrap() as i64;
        assert_eq!((at(1) - at(6)).abs(), 1);
        assert_eq!((at(3) - at(7)).abs(), 1);
        assert!((adjacency(&trace, 0..50, &o) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn reads_the_real_model_header_when_present() {
        let p = std::path::Path::new(concat!(env!("HOME"), "/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf"));
        if !p.exists() {
            return;
        }
        let map = ExpertMap::from_gguf(p).unwrap();
        assert_eq!(map.layers.len(), 48);
        assert!(map.layers.iter().all(|l| l.len() == 3 && l.iter().all(|t| t.n_expert == 128)));
        let lay = ExpertLayout::new(&map, identity_order(&map)).unwrap();
        // Pages stored twice cost well under 2% extra.
        assert!(lay.space < map.file_size + map.file_size / 50);
    }
}
