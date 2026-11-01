//! Commit-timing histograms, recorded only when `CYPHERLITE_COMMIT_TIMING=1`
//! (or [`crate::Database::enable_commit_timing`]). The group-commit queue and
//! the WAL batch stay in `commit`.

#[derive(Default)]
pub(crate) struct CommitTimingBuf {
    pub(super) samples: Vec<TimingSample>,
    /// `sync_all` invocations (one per group, ADR-0019).
    pub(super) sync_calls: u64,
}

pub(super) struct TimingSample {
    pub(super) parse_plan_ns: u64,
    pub(super) execute_ns: u64,
    pub(super) wal_lock_ns: u64,
    pub(super) encode_write_ns: u64,
    pub(super) sync_ns: u64,
    pub(super) apply_ns: u64,
}

/// Elapsed nanoseconds of an optional timing start: 0 when timing is off.
/// The one spelling of the conversion; commit and dispatch both use it.
pub(crate) fn elapsed_nanos(t: Option<web_time::Instant>) -> u64 {
    t.map(|t| t.elapsed().as_nanos() as u64).unwrap_or(0)
}

pub(crate) fn commit_timing_env() -> bool {
    std::env::var("CYPHERLITE_COMMIT_TIMING").as_deref() == Ok("1")
}

pub(crate) fn format_commit_timing(buf: &CommitTimingBuf) -> String {
    let n = buf.samples.len();
    if n == 0 {
        return String::new();
    }
    fn row(name: &str, samples: &[TimingSample], get: impl Fn(&TimingSample) -> u64) -> String {
        let mut v: Vec<u64> = samples.iter().map(get).collect();
        v.sort_unstable();
        let n = v.len();
        let sum: u64 = v.iter().copied().sum();
        let mean = sum / n as u64;
        let pct = |p: f64| {
            let i = ((n as f64 - 1.0) * p).round() as usize;
            v[i.min(n - 1)]
        };
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        format!(
            "  {name:<18} n={n:<6} min={:>8.3}  p50={:>8.3}  p95={:>8.3}  max={:>8.3}  mean={:>8.3}  sum={:>7.1}  (ms)\n",
            ms(v[0]),
            ms(pct(0.50)),
            ms(pct(0.95)),
            ms(v[n - 1]),
            ms(mean),
            ms(sum),
        )
    }
    let mut s = format!(
        "cypherlite commit timing ({} durable commits, {} sync_all):\n",
        n, buf.sync_calls
    );
    s.push_str(&row("parse+plan", &buf.samples, |s| s.parse_plan_ns));
    s.push_str(&row("overlay execute", &buf.samples, |s| s.execute_ns));
    s.push_str(&row("wal_lock wait", &buf.samples, |s| s.wal_lock_ns));
    s.push_str(&row("encode+write", &buf.samples, |s| s.encode_write_ns));
    s.push_str(&row("sync_all", &buf.samples, |s| s.sync_ns));
    s.push_str(&row("apply_redo", &buf.samples, |s| s.apply_ns));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_commit_timing_lists_exactly_the_six_rows() {
        let mut buf = CommitTimingBuf::default();
        buf.samples.push(TimingSample {
            parse_plan_ns: 1,
            execute_ns: 1,
            wal_lock_ns: 1,
            encode_write_ns: 1,
            sync_ns: 1,
            apply_ns: 1,
        });
        let report = format_commit_timing(&buf);
        // Each row is `  {name:<18} n=...`; collect the name field so the
        // whole row set is compared once, in order. Header text cannot
        // satisfy this.
        let rows: Vec<&str> = report
            .lines()
            .filter_map(|line| {
                let (name, _) = line.strip_prefix("  ")?.split_once(" n=")?;
                Some(name.trim_end())
            })
            .collect();
        assert_eq!(
            rows,
            [
                "parse+plan",
                "overlay execute",
                "wal_lock wait",
                "encode+write",
                "sync_all",
                "apply_redo",
            ],
            "unexpected row set:\n{report}"
        );
    }
}
