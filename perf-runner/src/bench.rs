//! The benchmark's side of the contract: one `PERFLAB {json}` line on stdout.

use std::collections::BTreeMap;

use crate::stats::Better;

pub const LINE_PREFIX: &str = "PERFLAB ";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct BenchReport {
    pub metrics: BTreeMap<String, f64>,
    pub units: BTreeMap<String, String>,
}

/// Parse the LAST `PERFLAB ` line of `stdout`. `Ok(None)` = no such line;
/// `Err` = a line that is not a JSON object of numbers (the benchmark is
/// broken, which must not read as "no metrics").
pub fn parse(stdout: &str) -> Result<Option<BenchReport>, String> {
    let Some(line) = stdout.lines().rev().find(|l| l.trim_start().starts_with(LINE_PREFIX)) else {
        return Ok(None);
    };
    let json = line.trim_start()[LINE_PREFIX.len()..].trim();
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("PERFLAB line is not valid JSON ({e}): {json}"))?;
    let obj = value.as_object().ok_or_else(|| format!("PERFLAB line is not a JSON object: {json}"))?;
    let mut rep = BenchReport::default();
    for (k, v) in obj {
        if let Some(unit_of) = k.strip_suffix("_unit")
            && let Some(s) = v.as_str() {
                rep.units.insert(unit_of.to_string(), s.to_string());
                continue;
            }
        match v.as_f64() {
            Some(x) if x.is_finite() => {
                rep.metrics.insert(k.clone(), x);
            }
            _ => return Err(format!("PERFLAB key {k:?} is not a finite number: {v}")),
        }
    }
    Ok(Some(rep))
}

/// Exact latency quantiles from a raw samples file: little-endian u64
/// nanoseconds, one per measured iteration (the `PERFLAB_SAMPLES` protocol).
/// `Ok(None)` when the file is absent or empty.
pub fn samples_metrics(bytes: &[u8]) -> Result<Option<BTreeMap<String, f64>>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() % 8 != 0 {
        return Err(format!("the samples file is {} bytes, not a whole number of u64 values", bytes.len()));
    }
    let mut v: Vec<u64> = bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    let n = v.len();
    let mut m = BTreeMap::new();
    let mean = v.iter().map(|&x| x as f64).sum::<f64>() / n as f64;
    // Order statistic at rank floor(q * (n - 1)), as nth_element benchmarks do.
    for (name, q) in [("p50_ns", 0.50), ("p90_ns", 0.90), ("p99_ns", 0.99), ("p999_ns", 0.999), ("p9999_ns", 0.9999)] {
        // A quantile needs ~10 samples beyond it to mean anything.
        if (n as f64) * (1.0 - q) < 10.0 {
            continue;
        }
        let k = ((n - 1) as f64 * q) as usize;
        let (_, x, _) = v.select_nth_unstable(k);
        m.insert(name.to_string(), *x as f64);
    }
    m.insert("max_ns".to_string(), *v.iter().max().unwrap() as f64);
    m.insert("mean_ns".to_string(), mean);
    m.insert("samples".to_string(), n as f64);
    Ok(Some(m))
}

/// Direction of a metric from its name.
pub fn better_of(name: &str) -> Better {
    let n = name.to_ascii_lowercase();
    if n == "throughput" || n.starts_with("throughput_") || n.ends_with("_per_s") || n.ends_with("_per_sec") || n.ends_with("_per_second") {
        return Better::Higher;
    }
    for suffix in ["_ns", "_us", "_ms", "_s", "_cycles", "_sec", "_seconds"] {
        if n.ends_with(suffix) {
            return Better::Lower;
        }
    }
    Better::Info
}

/// Unit shown for a metric: the benchmark's `*_unit`, else the name suffix.
pub fn unit_of(name: &str, units: &BTreeMap<String, String>) -> Option<String> {
    if let Some(u) = units.get(name) {
        return Some(u.clone());
    }
    let n = name.to_ascii_lowercase();
    for (suffix, unit) in [("_ns", "ns"), ("_us", "us"), ("_ms", "ms"), ("_cycles", "cycles"), ("_s", "s")] {
        if n.ends_with(suffix) {
            return Some(unit.to_string());
        }
    }
    None
}

/// Display order: the latency quantiles first, then throughput, then the rest.
pub fn display_rank(name: &str) -> (u8, String) {
    let rank = match name {
        "p50_ns" => 0,
        "p90_ns" => 1,
        "p99_ns" => 2,
        "p999_ns" => 3,
        "p9999_ns" => 4,
        "max_ns" => 5,
        "mean_ns" => 6,
        "throughput" => 7,
        "wall_s" => 9,
        _ => 8,
    };
    (rank, name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_last_line_and_units() {
        let out = "loading\nPERFLAB {\"p50_ns\": 1}\nwarm\nPERFLAB {\"p50_ns\": 412.5, \"throughput\": 2.3e6, \"throughput_unit\": \"msg/s\", \"allocs_per_msg\": 0}\n";
        let r = parse(out).unwrap().unwrap();
        assert_eq!(r.metrics["p50_ns"], 412.5);
        assert_eq!(r.metrics["throughput"], 2.3e6);
        assert_eq!(r.metrics["allocs_per_msg"], 0.0);
        assert_eq!(r.units["throughput"], "msg/s");
    }

    #[test]
    fn missing_line_is_none_and_bad_line_is_error() {
        assert_eq!(parse("hello\n").unwrap(), None);
        assert!(parse("PERFLAB {not json}\n").is_err());
        assert!(parse("PERFLAB [1,2]\n").is_err());
        assert!(parse("PERFLAB {\"p50_ns\": \"fast\"}\n").is_err());
    }

    #[test]
    fn samples_file_quantiles() {
        assert_eq!(samples_metrics(&[]).unwrap(), None);
        // 1..=10000 shuffled-ish: quantiles are exact order statistics.
        let mut vals: Vec<u64> = (1..=10_000).collect();
        vals.reverse();
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let m = samples_metrics(&bytes).unwrap().unwrap();
        assert_eq!(m["p50_ns"], 5000.0);
        assert_eq!(m["p99_ns"], 9900.0);
        assert_eq!(m["p999_ns"], 9990.0);
        assert!(!m.contains_key("p9999_ns"), "10k samples cannot give a p99.99");
        assert_eq!(m["max_ns"], 10_000.0);
        assert_eq!(m["samples"], 10_000.0);
        assert!(samples_metrics(&[1u8, 2, 3]).is_err());
    }

    #[test]
    fn directions() {
        assert_eq!(better_of("p50_ns"), Better::Lower);
        assert_eq!(better_of("p999_ns"), Better::Lower);
        assert_eq!(better_of("wall_s"), Better::Lower);
        assert_eq!(better_of("cycles_per_msg"), Better::Info);
        assert_eq!(better_of("throughput"), Better::Higher);
        assert_eq!(better_of("msgs_per_sec"), Better::Higher);
        assert_eq!(better_of("allocs_per_msg"), Better::Info);
    }
}
