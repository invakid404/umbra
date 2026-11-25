//! D6 arithmetic. Inputs are retained observations in nanoseconds.

#[derive(Debug, PartialEq)]
pub struct Distribution {
    pub median: f64,
    pub p95: f64,
    pub maximum: f64,
}

pub fn distribution(samples: &[f64]) -> Result<Distribution, &'static str> {
    if samples.is_empty() || samples.iter().any(|n| !n.is_finite()) {
        return Err("missing or nonfinite sample");
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };
    Ok(Distribution {
        median,
        p95: sorted[(95 * n).div_ceil(100) - 1],
        maximum: sorted[n - 1],
    })
}

#[derive(Debug)]
pub struct Comparison {
    pub base: Distribution,
    pub head: Distribution,
    pub paired: Distribution,
    pub differences: Vec<f64>,
}

pub fn compare(base: &[f64], head: &[f64]) -> Result<Comparison, &'static str> {
    if base.len() != 30 || head.len() != 30 {
        return Err("D6 requires all thirty measured pairs");
    }
    if base.iter().chain(head).any(|n| *n < 0.0) {
        return Err("negative elapsed time");
    }
    let differences: Vec<_> = base.iter().zip(head).map(|(b, h)| h - b).collect();
    Ok(Comparison {
        base: distribution(base)?,
        head: distribution(head)?,
        paired: distribution(&differences)?,
        differences,
    })
}

impl Comparison {
    pub fn teardown_passes(&self) -> bool {
        self.head.median - self.base.median <= 5_000_000.0_f64.max(self.base.median * 0.10)
            && self.head.p95 - self.base.p95 <= 10_000_000.0_f64.max(self.base.p95 * 0.20)
    }
    pub fn helper_start_passes(&self) -> bool {
        self.head.median - self.base.median <= 25_000_000.0
            && self.head.p95 - self.base.p95 <= 50_000_000.0
    }
}

pub fn admission_passes(samples: &[f64]) -> Result<bool, &'static str> {
    if samples.iter().any(|n| *n < 0.0) {
        return Err("negative admission time");
    }
    let d = distribution(samples)?;
    Ok(d.median <= 1_000_000.0 && d.p95 <= 5_000_000.0)
}
