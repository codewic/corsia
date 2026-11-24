//! Expert routing traces: which experts each token used in each layer.
//!
//! Text format, one token per line, one group of comma-separated expert ids per layer:
//!
//! ```text
//! # corsia-trace 1
//! # layers=48 experts=128 topk=8 source=synthetic skew=0.6 repeat=0.3 seed=1
//! 3,17,22,40,51,77,90,101 5,9,12,33,48,60,99,127 ...
//! ```
//!
//! Real traces come from an engine run on the target machine; synthetic ones stand in
//! until then and are labelled as such everywhere they are shown.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub struct Trace {
    pub layers: usize,
    pub experts: usize,
    pub topk: usize,
    /// Free text from the header: "synthetic skew=0.6 ..." or where a real trace came from.
    pub source: String,
    /// token -> layer -> expert ids
    pub tokens: Vec<Vec<Vec<u16>>>,
}

pub struct SynthParams {
    pub tokens: usize,
    pub layers: usize,
    pub experts: usize,
    pub topk: usize,
    /// Zipf exponent of expert popularity within a layer. 0 is uniform.
    pub skew: f64,
    /// Chance that an expert used by the previous token in a layer is used again.
    pub repeat: f64,
    /// Chance that a slot is filled by a usual follower of one of the previous layer's
    /// experts (each expert has 4). 0 means layers are independent.
    pub affinity: f64,
    pub seed: u64,
}

impl Default for SynthParams {
    fn default() -> Self {
        SynthParams { tokens: 512, layers: 48, experts: 128, topk: 8, skew: 0.6, repeat: 0.3, affinity: 0.0, seed: 1 }
    }
}

const FOLLOWERS: usize = 4;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

pub fn synth(p: &SynthParams) -> Trace {
    let mut rng = Rng(p.seed);
    // Each layer gets its own popularity order and a cumulative Zipf table.
    let mut perms: Vec<Vec<u16>> = Vec::with_capacity(p.layers);
    for _ in 0..p.layers {
        let mut perm: Vec<u16> = (0..p.experts as u16).collect();
        for i in (1..perm.len()).rev() {
            perm.swap(i, rng.below(i + 1));
        }
        perms.push(perm);
    }
    let weights: Vec<f64> = (0..p.experts).map(|r| 1.0 / ((r + 1) as f64).powf(p.skew)).collect();
    let total: f64 = weights.iter().sum();
    let mut cdf = Vec::with_capacity(p.experts);
    let mut acc = 0.0;
    for w in &weights {
        acc += w / total;
        cdf.push(acc);
    }

    // followers[l][e]: the experts at layer l+1 that usually come after expert e at layer l.
    let followers: Vec<Vec<[u16; FOLLOWERS]>> = (0..p.layers)
        .map(|_| (0..p.experts).map(|_| std::array::from_fn(|_| rng.below(p.experts) as u16)).collect())
        .collect();

    let topk = p.topk.min(p.experts);
    let mut prev: Vec<Vec<u16>> = vec![Vec::new(); p.layers];
    let mut tokens = Vec::with_capacity(p.tokens);
    for _ in 0..p.tokens {
        let mut tok = Vec::with_capacity(p.layers);
        for l in 0..p.layers {
            let mut set: Vec<u16> = Vec::with_capacity(topk);
            for &e in &prev[l] {
                if set.len() < topk && rng.unit() < p.repeat {
                    set.push(e);
                }
            }
            let mut tries = 0;
            while set.len() < topk {
                tries += 1;
                let e = if l > 0 && tries <= 4 * topk && rng.unit() < p.affinity {
                    let before: &Vec<u16> = &tok[l - 1];
                    let from = before[rng.below(before.len())] as usize;
                    followers[l - 1][from][rng.below(FOLLOWERS)]
                } else {
                    let u = rng.unit();
                    let rank = cdf.partition_point(|c| *c < u).min(p.experts - 1);
                    perms[l][rank]
                };
                if !set.contains(&e) {
                    set.push(e);
                }
            }
            set.sort_unstable();
            prev[l] = set.clone();
            tok.push(set);
        }
        tokens.push(tok);
    }
    Trace {
        layers: p.layers,
        experts: p.experts,
        topk,
        source: format!("synthetic skew={} repeat={} affinity={} seed={}", p.skew, p.repeat, p.affinity, p.seed),
        tokens,
    }
}

pub fn save(t: &Trace, path: &Path) -> io::Result<()> {
    let mut w = BufWriter::new(fs::File::create(path)?);
    writeln!(w, "# corsia-trace 1")?;
    writeln!(w, "# layers={} experts={} topk={} source={}", t.layers, t.experts, t.topk, t.source)?;
    for tok in &t.tokens {
        let groups: Vec<String> = tok
            .iter()
            .map(|g| g.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(","))
            .collect();
        writeln!(w, "{}", groups.join(" "))?;
    }
    w.flush()
}

pub fn load(path: &Path) -> Result<Trace, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("Can't read the trace {}: {e}", path.display()))?;
    let mut layers = 0usize;
    let mut experts = 0usize;
    let mut topk = 0usize;
    let mut source = String::from("unknown");
    let mut tokens = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(head) = line.strip_prefix('#') {
            if let Some(i) = head.find("source=") {
                source = head[i + 7..].trim().to_string();
            }
            for kv in head.split_whitespace() {
                match kv.split_once('=') {
                    Some(("layers", v)) => layers = v.parse().unwrap_or(0),
                    Some(("experts", v)) => experts = v.parse().unwrap_or(0),
                    Some(("topk", v)) => topk = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
            continue;
        }
        if layers == 0 || experts == 0 {
            return Err(format!("{} has no header with layers= and experts= before line {}.", path.display(), n + 1));
        }
        let mut tok = Vec::with_capacity(layers);
        for g in line.split_whitespace() {
            let mut ids = Vec::new();
            for id in g.split(',').filter(|s| !s.is_empty()) {
                let e: u16 = id.parse().map_err(|_| format!("Line {}: \"{id}\" is not an expert id.", n + 1))?;
                if e as usize >= experts {
                    return Err(format!("Line {}: expert {e} is out of range (the model has {experts}).", n + 1));
                }
                ids.push(e);
            }
            tok.push(ids);
        }
        if tok.len() != layers {
            return Err(format!("Line {} has {} layers; the header says {layers}.", n + 1, tok.len()));
        }
        tokens.push(tok);
    }
    if tokens.is_empty() {
        return Err(format!("{} has no tokens.", path.display()));
    }
    if topk == 0 {
        topk = tokens.iter().flatten().map(|g| g.len()).max().unwrap_or(0);
    }
    Ok(Trace { layers, experts, topk, source, tokens })
}
