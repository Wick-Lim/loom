use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct LatencyStats {
    pub count: usize,
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

impl LatencyStats {
    pub fn from_ms(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self {
                count: 0,
                min_ms: 0.0,
                p50_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
                mean_ms: 0.0,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let sum: f64 = sorted.iter().sum();
        let mean_ms = sum / sorted.len() as f64;
        Self {
            count: sorted.len(),
            min_ms: sorted[0],
            p50_ms: percentile_nearest_rank(&sorted, 0.50),
            p95_ms: percentile_nearest_rank(&sorted, 0.95),
            p99_ms: percentile_nearest_rank(&sorted, 0.99),
            max_ms: *sorted.last().unwrap_or(&0.0),
            mean_ms,
        }
    }
}

fn percentile_nearest_rank(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[idx]
}

pub fn median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_stats_computes_nearest_rank_percentiles() {
        let samples: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        let stats = LatencyStats::from_ms(&samples);
        assert_eq!(stats.count, 100);
        assert!((stats.min_ms - 1.0).abs() < f64::EPSILON);
        assert!((stats.p50_ms - 50.0).abs() < f64::EPSILON);
        assert!((stats.p95_ms - 95.0).abs() < f64::EPSILON);
        assert!((stats.p99_ms - 99.0).abs() < f64::EPSILON);
        assert!((stats.max_ms - 100.0).abs() < f64::EPSILON);
        assert!((stats.mean_ms - 50.5).abs() < 1e-9);
    }
}
