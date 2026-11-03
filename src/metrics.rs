use crate::config::format_base_units;
use anyhow::{Context, Result};
use solana_pubkey::Pubkey;
use std::{
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::Write as _,
    path::Path,
};

/// One run's metrics. Values unknown at failure time are left out of the
/// file instead of being written as a misleading zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunMetrics {
    pub vote: Option<Pubkey>,
    pub last_run_timestamp_seconds: u64,
    pub success: bool,
    pub bond_value_lamports: Option<u64>,
    pub bond_jsol: Option<u64>,
    pub jsol_sol_rate: Option<f64>,
    pub min_value_lamports: Option<u64>,
    pub required_lamports: Option<u64>,
    pub target_lamports: Option<u64>,
    pub identity_lamports: Option<u64>,
    /// SOL deposited into the bond by this run; 0 when none.
    pub last_topup_lamports: u64,
}

pub trait MetricsSink {
    fn preflight(&self, path: &Path) -> Result<()>;
    fn write(&self, path: &Path, metrics: &RunMetrics) -> Result<()>;
}

pub struct FileMetricsSink;

impl MetricsSink for FileMetricsSink {
    fn preflight(&self, path: &Path) -> Result<()> {
        preflight_metrics(path)
    }

    fn write(&self, path: &Path, metrics: &RunMetrics) -> Result<()> {
        write_metrics(path, metrics)
    }
}

fn gauge(output: &mut String, name: &str, help: &str, labels: &str, value: &str) -> Result<()> {
    writeln!(output, "# HELP {name} {help}")?;
    writeln!(output, "# TYPE {name} gauge")?;
    writeln!(output, "{name}{{{labels}}} {value}")?;
    Ok(())
}

pub fn render_prometheus(metrics: &RunMetrics) -> Result<String> {
    let labels = format!(
        "vote=\"{}\"",
        metrics
            .vote
            .map(|vote| vote.to_string())
            .unwrap_or_default()
    );
    let mut output = String::new();
    if let Some(value) = metrics.bond_value_lamports {
        gauge(
            &mut output,
            "jpool_bond_value_sol",
            "SOL value of the JSOL held by the JPool performance bond",
            &labels,
            &format_base_units(value),
        )?;
    }
    if let Some(jsol) = metrics.bond_jsol {
        gauge(
            &mut output,
            "jpool_bond_jsol",
            "JSOL held by the JPool performance bond",
            &labels,
            &format_base_units(jsol),
        )?;
    }
    if let Some(rate) = metrics.jsol_sol_rate {
        gauge(
            &mut output,
            "jpool_bond_jsol_sol_rate",
            "SOL per JSOL from the JPool stake pool",
            &labels,
            &format!("{rate:.9}"),
        )?;
    }
    if let Some(value) = metrics.min_value_lamports {
        gauge(
            &mut output,
            "jpool_bond_min_value_sol",
            "Bond value below which a top-up runs (trigger ratio x required) in SOL",
            &labels,
            &format_base_units(value),
        )?;
    }
    if let Some(value) = metrics.required_lamports {
        gauge(
            &mut output,
            "jpool_bond_required_sol",
            "Bond requirement (JPool security + performance, at least the configured minimum) in SOL",
            &labels,
            &format_base_units(value),
        )?;
    }
    if let Some(value) = metrics.target_lamports {
        gauge(
            &mut output,
            "jpool_bond_target_sol",
            "Bond value a top-up aims for (target ratio x required) in SOL",
            &labels,
            &format_base_units(value),
        )?;
    }
    if let Some(value) = metrics.identity_lamports {
        gauge(
            &mut output,
            "jpool_bond_identity_balance_sol",
            "SOL balance of the identity that funds top-ups",
            &labels,
            &format_base_units(value),
        )?;
    }
    gauge(
        &mut output,
        "jpool_bond_last_topup_sol",
        "SOL deposited into the bond by the last run (0 if none)",
        &labels,
        &format_base_units(metrics.last_topup_lamports),
    )?;
    gauge(
        &mut output,
        "jpool_bond_last_run_success",
        "Whether the last run succeeded (refusals count as failures)",
        &labels,
        &u8::from(metrics.success).to_string(),
    )?;
    gauge(
        &mut output,
        "jpool_bond_last_run_timestamp_seconds",
        "Unix timestamp of the last run",
        &labels,
        &metrics.last_run_timestamp_seconds.to_string(),
    )?;
    Ok(output)
}

pub fn write_metrics(path: &Path, metrics: &RunMetrics) -> Result<()> {
    write_atomically(path, &render_prometheus(metrics)?)
}

pub fn preflight_metrics(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.{}.preflight",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("jpool-bond.prom"),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("failed to preflight {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync preflight for {}", path.display()))
    })();
    let remove_result = fs::remove_file(&temporary);
    result?;
    remove_result.with_context(|| format!("failed to remove {}", temporary.display()))
}

pub fn write_atomically(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("jpool-bond.prom");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(body.as_bytes())
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to rename metrics to {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::pubkey;

    fn sample(success: bool) -> RunMetrics {
        RunMetrics {
            vote: Some(pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr")),
            last_run_timestamp_seconds: 1_700_000_000,
            success,
            bond_value_lamports: Some(15_204_000_000),
            bond_jsol: Some(11_001_704_341),
            jsol_sol_rate: Some(1.382),
            min_value_lamports: Some(8_000_000_000),
            required_lamports: Some(5_720_000_000),
            target_lamports: Some(11_440_000_000),
            identity_lamports: Some(78_700_681_661),
            last_topup_lamports: 0,
        }
    }

    #[test]
    fn renders_all_metrics_with_vote_label() {
        let body = render_prometheus(&sample(true)).unwrap();
        let label = "{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"}";
        for line in [
            format!("jpool_bond_value_sol{label} 15.204"),
            format!("jpool_bond_jsol{label} 11.001704341"),
            format!("jpool_bond_jsol_sol_rate{label} 1.382000000"),
            format!("jpool_bond_min_value_sol{label} 8"),
            format!("jpool_bond_identity_balance_sol{label} 78.700681661"),
            format!("jpool_bond_last_topup_sol{label} 0"),
            format!("jpool_bond_last_run_success{label} 1"),
            format!("jpool_bond_last_run_timestamp_seconds{label} 1700000000"),
        ] {
            assert!(body.contains(&line), "missing {line} in\n{body}");
        }
    }

    #[test]
    fn failure_without_data_omits_unknown_values() {
        let body = render_prometheus(&RunMetrics {
            last_run_timestamp_seconds: 1,
            ..RunMetrics::default()
        })
        .unwrap();
        assert!(body.contains("jpool_bond_last_run_success{vote=\"\"} 0"));
        assert!(!body.contains("jpool_bond_value_sol"));
        assert!(!body.contains("jpool_bond_jsol{"));
    }

    #[test]
    fn writes_success_and_failure_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jpool.prom");
        write_metrics(&path, &sample(true)).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 1")
        );
        write_metrics(&path, &sample(false)).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 0")
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn preflight_checks_destination_without_replacing_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jpool.prom");
        std::fs::write(&path, "existing metrics\n").unwrap();
        preflight_metrics(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "existing metrics\n"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
