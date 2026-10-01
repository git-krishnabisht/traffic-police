//! Traffic over time for the graph (ARCHITECTURE.md §5.6, §5.8).
//!
//! Two series: bytes the runtime captured (10 ms bins, sparse), and the whole-app `TrafficStats`
//! counters the runtime samples every 500 ms (the default graph source).

use std::collections::{BTreeMap, HashMap};

use crate::fmt::{NS_PER_SEC, Ts};
use crate::model::SourceId;

pub const BIN_NS: u64 = 10_000_000;

/// Which numbers the graph draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphSource {
    /// Whole-app traffic as counted by Android (what Android Studio draws). Default.
    #[default]
    AppTotal,
    /// Bytes of the requests traffic-police captured.
    Captured,
}

impl GraphSource {
    /// `[ui] graph`: `app` (all app traffic) or `requests` (the captured requests).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "app" => Some(GraphSource::AppTotal),
            "requests" => Some(GraphSource::Captured),
            _ => None,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            GraphSource::AppTotal => GraphSource::Captured,
            GraphSource::Captured => GraphSource::AppTotal,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GraphSource::AppTotal => "all app traffic",
            GraphSource::Captured => "captured requests",
        }
    }
}

/// Bytes per second in `n` equal buckets.
#[derive(Debug, Clone, PartialEq)]
pub struct Buckets {
    pub rx: Vec<f64>,
    pub tx: Vec<f64>,
    pub bucket_ns: u64,
    /// The source actually used (whole-app data may be missing).
    pub source: GraphSource,
}

#[derive(Debug, Clone, Copy)]
struct AppInterval {
    from: Ts,
    to: Ts,
    rx: u64,
    tx: u64,
}

#[derive(Debug, Default, Clone)]
pub struct TrafficSeries {
    /// bin index (ts / BIN_NS) -> [rx, tx]
    captured: BTreeMap<u64, [u64; 2]>,
    app: Vec<AppInterval>,
    last_sample: HashMap<SourceId, (Ts, u64, u64)>,
}

impl TrafficSeries {
    pub fn add_captured(&mut self, at: Ts, rx: u64, tx: u64) {
        if rx == 0 && tx == 0 {
            return;
        }
        let bin = self.captured.entry(at / BIN_NS).or_insert([0, 0]);
        bin[0] += rx;
        bin[1] += tx;
    }

    /// A cumulative whole-app sample. The first sample of a source is only a baseline.
    pub fn add_app_sample(&mut self, source: SourceId, at: Ts, since: Option<Ts>, rx: u64, tx: u64) {
        if let Some(&(prev_at, prev_rx, prev_tx)) = self.last_sample.get(&source)
            && at > prev_at
        {
            let from = since.filter(|s| *s >= prev_at && *s < at).unwrap_or(prev_at);
            let (drx, dtx) = (rx.saturating_sub(prev_rx), tx.saturating_sub(prev_tx));
            if drx > 0 || dtx > 0 {
                let iv = AppInterval { from, to: at, rx: drx, tx: dtx };
                let pos = self.app.partition_point(|x| x.to <= iv.to);
                self.app.insert(pos, iv);
            }
        }
        self.last_sample.insert(source, (at, rx, tx));
    }

    pub fn has_app_data(&self) -> bool {
        !self.last_sample.is_empty()
    }

    /// Rates over `[from, to)` in `n` buckets.
    pub fn buckets(&self, wanted: GraphSource, from: Ts, to: Ts, n: usize) -> Buckets {
        let n = n.max(1);
        let span = to.saturating_sub(from).max(n as u64);
        let bucket_ns = span.div_ceil(n as u64).max(1);
        let mut rx = vec![0.0; n];
        let mut tx = vec![0.0; n];
        let source =
            if wanted == GraphSource::AppTotal && !self.has_app_data() { GraphSource::Captured } else { wanted };
        match source {
            GraphSource::Captured => {
                for (&bin, &[r, t]) in self.captured.range(from / BIN_NS..to.div_ceil(BIN_NS)) {
                    let at = bin * BIN_NS;
                    if at < from || at >= to {
                        continue;
                    }
                    let i = (((at - from) / bucket_ns) as usize).min(n - 1);
                    rx[i] += r as f64;
                    tx[i] += t as f64;
                }
            }
            GraphSource::AppTotal => {
                let first = self.app.partition_point(|iv| iv.to <= from);
                for iv in &self.app[first..] {
                    if iv.from >= to {
                        continue;
                    }
                    let len = (iv.to - iv.from).max(1) as f64;
                    let lo = iv.from.max(from);
                    let hi = iv.to.min(to);
                    let mut t = lo;
                    while t < hi {
                        let i = (((t - from) / bucket_ns) as usize).min(n - 1);
                        let bucket_end = (from + (i as u64 + 1) * bucket_ns).min(hi);
                        let frac = (bucket_end - t) as f64 / len;
                        rx[i] += iv.rx as f64 * frac;
                        tx[i] += iv.tx as f64 * frac;
                        t = bucket_end;
                    }
                }
            }
        }
        let per_sec = NS_PER_SEC as f64 / bucket_ns as f64;
        for v in rx.iter_mut().chain(tx.iter_mut()) {
            *v *= per_sec;
        }
        Buckets { rx, tx, bucket_ns, source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_bins_become_rates() {
        let mut s = TrafficSeries::default();
        s.add_captured(1_000_000_000, 1000, 10); // at 1.0 s
        s.add_captured(1_500_000_000, 1000, 0); // at 1.5 s
        let b = s.buckets(GraphSource::Captured, 0, 2 * NS_PER_SEC, 2);
        assert_eq!(b.source, GraphSource::Captured);
        assert_eq!(b.rx, vec![0.0, 2000.0]);
        assert_eq!(b.tx, vec![0.0, 10.0]);
    }

    #[test]
    fn app_samples_spread_only_over_their_tick() {
        let mut s = TrafficSeries::default();
        s.add_app_sample(0, NS_PER_SEC, None, 100, 0); // baseline
        // quiet for 9 s, then 1000 bytes within the last 500 ms tick
        s.add_app_sample(0, 10 * NS_PER_SEC, Some(9_500_000_000), 1100, 0);
        let b = s.buckets(GraphSource::AppTotal, 0, 10 * NS_PER_SEC, 20);
        assert_eq!(b.source, GraphSource::AppTotal);
        let total: f64 = b.rx.iter().map(|r| r * 0.5).sum();
        assert!((total - 1000.0).abs() < 1e-6);
        assert!(b.rx[..19].iter().all(|&r| r == 0.0));
        assert!((b.rx[19] - 2000.0).abs() < 1e-6);
    }

    #[test]
    fn missing_app_data_falls_back_to_captured() {
        let s = TrafficSeries::default();
        assert_eq!(s.buckets(GraphSource::AppTotal, 0, NS_PER_SEC, 4).source, GraphSource::Captured);
    }
}
