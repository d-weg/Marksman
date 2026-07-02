mod stats;

/// Rolling summary of request latencies, in milliseconds.
pub struct LatencySummary {
    pub p50: f64,
    pub p99: f64,
    pub mean: f64,
}

pub fn summarize(mut samples: Vec<f64>) -> LatencySummary {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    LatencySummary {
        p50: stats::percentile(&samples, 0.50),
        p99: stats::percentile(&samples, 0.99),
        mean: stats::mean(&samples),
    }
}
