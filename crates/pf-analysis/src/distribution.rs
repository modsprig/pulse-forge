//! Repeated-run distribution statistics (roadmap PF-1104).
//!
//! Median / IQR / MAD with an explicit outlier fence — and **no** p-values:
//! the roadmap forbids claiming statistical significance from small
//! repeated samples without a justified model.

/// Descriptive statistics over repeated measurements of one metric.
#[derive(Clone, Debug, PartialEq)]
pub struct Distribution {
    pub n: usize,
    pub median: f64,
    pub q1: f64,
    pub q3: f64,
    /// Inter-quartile range.
    pub iqr: f64,
    /// Median absolute deviation (median of |x − median|).
    pub mad: f64,
    pub min: f64,
    pub max: f64,
    /// Indices (into the input order) flagged as outliers by the standard
    /// 1.5×IQR fence.
    pub outlier_indices: Vec<usize>,
}

/// Compute distribution statistics. `values` must be non-empty; NaN/inf
/// values are an error (never averaged into silence).
// Quantile indices are bounded by the sorted sample length.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn distribution_of(values: &[f64]) -> Result<Distribution, String> {
    if values.is_empty() {
        return Err("distribution needs at least one value".into());
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err("non-finite value in distribution input".into());
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    let quantile = |q: f64| -> f64 {
        // Linear interpolation on the sorted sample (Type 7).
        let pos = q * (n as f64 - 1.0);
        let low = pos.floor() as usize;
        let high = pos.ceil() as usize;
        let frac = pos - low as f64;
        sorted[low] + (sorted[high] - sorted[low]) * frac
    };
    let median = quantile(0.5);
    let q1 = quantile(0.25);
    let q3 = quantile(0.75);
    let iqr = q3 - q1;
    let mut devs: Vec<f64> = values.iter().map(|v| (v - median).abs()).collect();
    devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let dev_pos = 0.5 * (devs.len() as f64 - 1.0);
    let dev_low = dev_pos.floor() as usize;
    let dev_high = dev_pos.ceil() as usize;
    let dev_frac = dev_pos - dev_low as f64;
    let mad = devs[dev_low] + (devs[dev_high] - devs[dev_low]) * dev_frac;

    let lo_fence = q1 - 1.5 * iqr;
    let hi_fence = q3 + 1.5 * iqr;
    let outlier_indices = values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v < lo_fence || **v > hi_fence)
        .map(|(i, _)| i)
        .collect();

    Ok(Distribution {
        n,
        median,
        q1,
        q3,
        iqr,
        mad,
        min: sorted[0],
        max: sorted[n - 1],
        outlier_indices,
    })
}

impl Distribution {
    pub fn to_json(&self, unit: &str) -> pf_types::json::JsonValue {
        use pf_types::json::JsonValue as J;
        J::Object(vec![
            ("unit".into(), J::Str(unit.into())),
            ("runs".into(), J::UInt(self.n as u64)),
            ("median".into(), J::Float(self.median)),
            ("q1".into(), J::Float(self.q1)),
            ("q3".into(), J::Float(self.q3)),
            ("iqr".into(), J::Float(self.iqr)),
            ("mad".into(), J::Float(self.mad)),
            ("min".into(), J::Float(self.min)),
            ("max".into(), J::Float(self.max)),
            (
                "outliers".into(),
                J::Array(
                    self.outlier_indices
                        .iter()
                        .map(|i| J::UInt(*i as u64))
                        .collect(),
                ),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn known_quantiles() {
        let d = distribution_of(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        assert!(close(d.median, 3.0));
        assert!(close(d.q1, 2.0));
        assert!(close(d.q3, 4.0));
        assert!(close(d.iqr, 2.0));
        assert!(close(d.min, 1.0) && close(d.max, 5.0));
        assert!(d.outlier_indices.is_empty());
    }

    #[test]
    fn median_of_even_sample_interpolates() {
        let d = distribution_of(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert!(close(d.median, 2.5));
    }

    #[test]
    fn outliers_are_flagged_not_hidden() {
        // 10 values near 100 plus one at 10000.
        let mut values: Vec<f64> = (0..10).map(|i| 100.0 + i as f64).collect();
        values.push(10_000.0);
        let d = distribution_of(&values).unwrap();
        assert_eq!(d.outlier_indices, vec![10]);
    }

    #[test]
    fn non_finite_input_is_an_error() {
        assert!(distribution_of(&[1.0, f64::NAN]).is_err());
        assert!(distribution_of(&[f64::INFINITY]).is_err());
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(distribution_of(&[]).is_err());
    }
}
