//! Traffic over time for the graph (ARCHITECTURE.md §5.6, §5.8).
//!
//! Two series: bytes the runtime captured (10 ms bins, sparse; past an hour's worth of bins the
//! older ones merge into 100 ms bins), and the whole-app `TrafficStats` counters the runtime
//! samples every 500 ms (the default graph source).

use std::collections::{BTreeMap, HashMap};

use crate::fmt::{NS_PER_SEC, Ts};
use crate::model::SourceId;

pub const BIN_NS: u64 = 10_000_000;
/// The bins older captured bytes merge into.
pub const COARSE_BIN_NS: u64 = 100_000_000;
/// Fine bins kept at most: an hour of traffic in every 10 ms bin. Past it, the older half merges
/// into coarse bins, so a long session costs a tenth as much per hour (ARCHITECTURE.md §5.6).
const FINE_BINS: usize = (3600 * NS_PER_SEC / BIN_NS) as usize;

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

#[derive(Debug, Clone)]
pub struct TrafficSeries {
    /// bin index (ts / BIN_NS) -> [rx, tx], from `fine_from` on
    captured: BTreeMap<u64, [u64; 2]>,
    /// bin index (ts / COARSE_BIN_NS) -> [rx, tx], before `fine_from`
    coarse: BTreeMap<u64, [u64; 2]>,
    fine_from: Ts,
    fine_limit: usize,
    app: Vec<AppInterval>,
    last_sample: HashMap<SourceId, (Ts, u64, u64)>,
}

impl Default for TrafficSeries {
    fn default() -> Self {
        TrafficSeries::with_fine_limit(FINE_BINS)
    }
}

fn add_to(map: &mut BTreeMap<u64, [u64; 2]>, bin: u64, rx: u64, tx: u64) {
    let b = map.entry(bin).or_insert([0, 0]);
    b[0] = b[0].saturating_add(rx);
    b[1] = b[1].saturating_add(tx);
}

impl TrafficSeries {
    /// A series that keeps at most `fine_limit` 10 ms bins before merging the older half.
    pub fn with_fine_limit(fine_limit: usize) -> Self {
        TrafficSeries {
            captured: BTreeMap::new(),
            coarse: BTreeMap::new(),
            fine_from: 0,
            fine_limit: fine_limit.max(2),
            app: Vec::new(),
            last_sample: HashMap::new(),
        }
    }

    pub fn add_captured(&mut self, at: Ts, rx: u64, tx: u64) {
        if rx == 0 && tx == 0 {
            return;
        }
        if at < self.fine_from {
            return add_to(&mut self.coarse, at / COARSE_BIN_NS, rx, tx);
        }
        add_to(&mut self.captured, at / BIN_NS, rx, tx);
        if self.captured.len() > self.fine_limit {
            self.coarsen();
        }
    }

    /// Merges the older half of the fine bins into coarse bins, cut on a coarse bin's edge.
    fn coarsen(&mut self) {
        let Some((&oldest, _)) = self.captured.first_key_value() else { return };
        let Some((&newest, _)) = self.captured.last_key_value() else { return };
        let middle = oldest + (newest - oldest) / 2;
        let cut = (middle * BIN_NS / COARSE_BIN_NS + 1) * COARSE_BIN_NS;
        let keep = self.captured.split_off(&(cut / BIN_NS));
        for (bin, [rx, tx]) in std::mem::replace(&mut self.captured, keep) {
            add_to(&mut self.coarse, bin * BIN_NS / COARSE_BIN_NS, rx, tx);
        }
        self.fine_from = self.fine_from.max(cut);
    }

    /// How many bins the captured series holds (fine, coarse).
    pub fn bins(&self) -> (usize, usize) {
        (self.captured.len(), self.coarse.len())
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
                // a coarse bin is spread over the buckets it covers, as a rate
                for (&bin, &[r, t]) in self.coarse.range(from / COARSE_BIN_NS..to.div_ceil(COARSE_BIN_NS)) {
                    let (lo, hi) = (bin * COARSE_BIN_NS, (bin + 1) * COARSE_BIN_NS);
                    let mut at = lo.max(from);
                    while at < hi.min(to) {
                        let i = (((at - from) / bucket_ns) as usize).min(n - 1);
                        let bucket_end = (from + (i as u64 + 1) * bucket_ns).min(hi).min(to).max(at + 1);
                        let frac = (bucket_end - at) as f64 / COARSE_BIN_NS as f64;
                        rx[i] += r as f64 * frac;
                        tx[i] += t as f64 * frac;
                        at = bucket_end;
                    }
                }
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

    /// Past the limit, older bins merge into 100 ms ones: no byte is lost, the newest stay fine,
    /// and a window over the old part still shows the traffic at 100 ms resolution.
    #[test]
    fn old_captured_bins_merge_into_coarse_ones() {
        let mut s = TrafficSeries::with_fine_limit(1000);
        // 30 s of traffic, 1 KB in every 10 ms bin
        for i in 0..3000u64 {
            s.add_captured(i * BIN_NS + 1, 1000, 1);
        }
        let (fine, coarse) = s.bins();
        assert!(fine <= 1000 && coarse > 0, "{fine} fine, {coarse} coarse");
        let all = s.buckets(GraphSource::Captured, 0, 30 * NS_PER_SEC, 30);
        let total: f64 = all.rx.iter().sum::<f64>() * all.bucket_ns as f64 / NS_PER_SEC as f64;
        assert!((total - 3_000_000.0).abs() < 1.0, "{total}");
        // a steady 100 KB/s everywhere, merged or not
        assert!(all.rx.iter().all(|r| (r - 100_000.0).abs() < 1.0), "{:?}", all.rx);
        let old = s.buckets(GraphSource::Captured, 0, NS_PER_SEC, 20);
        assert!(old.rx.iter().all(|r| (r - 100_000.0).abs() < 1.0), "{:?}", old.rx);
        // late bytes for the merged part go to its coarse bins
        s.add_captured(5 * BIN_NS, 1000, 0);
        assert_eq!(s.bins().0, fine);
    }

    #[test]
    fn missing_app_data_falls_back_to_captured() {
        let s = TrafficSeries::default();
        assert_eq!(s.buckets(GraphSource::AppTotal, 0, NS_PER_SEC, 4).source, GraphSource::Captured);
    }
}
