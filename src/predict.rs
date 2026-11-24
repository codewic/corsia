//! Guesses which experts the next layers will use, before their routers run.
//!
//! Learns online from the routing it sees, with three signals:
//! - affinity: experts at layer L+d that followed this layer's experts before,
//! - repeat: what the previous token used at layer L+d,
//! - popularity: what layer L+d uses most.
//!
//! A predictor that reads the model's hidden state (running the next router early) is more
//! accurate; that needs the engine patch (hinted mode). This one needs only the routing.

use crate::trace::Trace;

#[derive(Clone, Copy, Debug)]
pub struct Weights {
    pub affinity: f32,
    pub repeat: f32,
    pub popularity: f32,
}

impl Weights {
    pub const COMBINED: Weights = Weights { affinity: 1.0, repeat: 0.8, popularity: 0.3 };
    pub const AFFINITY: Weights = Weights { affinity: 1.0, repeat: 0.0, popularity: 0.0 };
    pub const REPEAT: Weights = Weights { affinity: 0.0, repeat: 1.0, popularity: 0.0 };
    pub const POPULARITY: Weights = Weights { affinity: 0.0, repeat: 0.0, popularity: 1.0 };
}

pub struct Predictor {
    layers: usize,
    experts: usize,
    ahead: usize,
    weights: Weights,
    /// [d-1][layer][from][to]: how often `to` at layer+d followed `from` at layer.
    follow: Vec<u32>,
    /// [layer][expert]: how often each expert was used.
    used: Vec<u32>,
    /// [layer]: routing decisions seen per layer.
    seen: Vec<u32>,
    /// Previous token's experts per layer.
    prev: Vec<Vec<u16>>,
    /// The current token's experts so far, per layer.
    cur: Vec<Vec<u16>>,
    scores: Vec<f32>,
}

impl Predictor {
    pub fn new(layers: usize, experts: usize, ahead: usize, weights: Weights) -> Predictor {
        let ahead = ahead.max(1);
        Predictor {
            layers,
            experts,
            ahead,
            weights,
            follow: vec![0; ahead * layers * experts * experts],
            used: vec![0; layers * experts],
            seen: vec![0; layers],
            prev: vec![Vec::new(); layers],
            cur: vec![Vec::new(); layers],
            scores: vec![0.0; experts],
        }
    }

    fn fidx(&self, d: usize, layer: usize, from: usize, to: usize) -> usize {
        (((d - 1) * self.layers + layer) * self.experts + from) * self.experts + to
    }

    /// Records the experts a layer actually used. Call layers in order, 0 first.
    pub fn observe(&mut self, layer: usize, experts: &[u16]) {
        if layer == 0 {
            self.prev = std::mem::replace(&mut self.cur, vec![Vec::new(); self.layers]);
        }
        for d in 1..=self.ahead.min(layer) {
            let from_layer = layer - d;
            for fi in 0..self.cur[from_layer].len() {
                let from = self.cur[from_layer][fi] as usize;
                for &to in experts {
                    let i = self.fidx(d, from_layer, from, to as usize);
                    self.follow[i] = self.follow[i].saturating_add(1);
                }
            }
        }
        for &e in experts {
            let i = layer * self.experts + e as usize;
            self.used[i] = self.used[i].saturating_add(1);
        }
        self.seen[layer] = self.seen[layer].saturating_add(1);
        self.cur[layer] = experts.to_vec();
    }

    /// Best `pick` guesses for layer `layer + d`, given `layer`'s experts (already observed).
    pub fn predict(&mut self, layer: usize, d: usize, pick: usize, out: &mut Vec<u16>) {
        self.rank(layer, d, out);
        out.truncate(pick);
    }

    /// Every expert with any evidence for layer `layer + d`, most likely first. Callers that
    /// skip experts already in RAM walk down this list.
    pub fn rank(&mut self, layer: usize, d: usize, out: &mut Vec<u16>) {
        out.clear();
        let target = layer + d;
        if d == 0 || d > self.ahead || target >= self.layers {
            return;
        }
        let w = self.weights;
        self.scores.iter_mut().for_each(|s| *s = 0.0);
        let now = &self.cur[layer];
        if w.affinity > 0.0 && !now.is_empty() {
            for &from in now {
                let from = from as usize;
                let base = self.fidx(d, layer, from, 0);
                let total = self.used[layer * self.experts + from].max(1) as f32;
                for to in 0..self.experts {
                    self.scores[to] += w.affinity * self.follow[base + to] as f32 / total / now.len() as f32;
                }
            }
        }
        if w.repeat > 0.0 {
            for &e in &self.prev[target] {
                self.scores[e as usize] += w.repeat;
            }
        }
        if w.popularity > 0.0 {
            let seen = self.seen[target].max(1) as f32;
            for e in 0..self.experts {
                self.scores[e] += w.popularity * self.used[target * self.experts + e] as f32 / seen;
            }
        }
        out.extend((0..self.experts as u16).filter(|e| self.scores[*e as usize] > 0.0));
        out.sort_by(|a, b| self.scores[*b as usize].total_cmp(&self.scores[*a as usize]));
    }
}

/// Share of the experts actually used at layer L+d that the predictor named in advance.
pub struct Accuracy {
    pub name: &'static str,
    /// recall[d-1] for d = 1..=ahead
    pub recall: Vec<f64>,
}

/// Runs the predictor over the trace online (it learns as it goes) and scores the tokens
/// after `warmup`.
pub fn evaluate(trace: &Trace, ahead: usize, pick: usize, warmup: usize) -> Vec<Accuracy> {
    let methods = [
        ("Combined", Weights::COMBINED),
        ("Affinity only", Weights::AFFINITY),
        ("Repeat only", Weights::REPEAT),
        ("Popularity only", Weights::POPULARITY),
    ];
    methods
        .iter()
        .map(|(name, w)| {
            let mut p = Predictor::new(trace.layers, trace.experts, ahead, *w);
            let mut hit = vec![0u64; ahead];
            let mut total = vec![0u64; ahead];
            let mut guess = Vec::new();
            for (t, tok) in trace.tokens.iter().enumerate() {
                for (l, group) in tok.iter().enumerate() {
                    p.observe(l, group);
                    if t < warmup {
                        continue;
                    }
                    for d in 1..=ahead {
                        if l + d >= trace.layers {
                            break;
                        }
                        p.predict(l, d, pick, &mut guess);
                        let truth = &tok[l + d];
                        total[d - 1] += truth.len() as u64;
                        hit[d - 1] += truth.iter().filter(|e| guess.contains(e)).count() as u64;
                    }
                }
            }
            Accuracy {
                name,
                recall: hit.iter().zip(&total).map(|(h, t)| if *t == 0 { 0.0 } else { *h as f64 / *t as f64 }).collect(),
            }
        })
        .collect()
}
