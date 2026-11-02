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
    pub identity_lamports: Option<u64>,
    /// SOL deposited into the bond by this run; 0 when none.
    pub last_topup_lamports: u64,
}

pub trait MetricsSink {
    fn preflight(&self, path: &Path) -> Result<()>;
    fn write(&self, path: &Path, metrics: &RunMetrics) -> Result<()>;
    fn write_squads(&self, path: &Path, metrics: &SquadsRunMetrics) -> Result<()>;
}

pub struct FileMetricsSink;

impl MetricsSink for FileMetricsSink {
    fn preflight(&self, path: &Path) -> Result<()> {
        preflight_metrics(path)
    }

    fn write(&self, path: &Path, metrics: &RunMetrics) -> Result<()> {
        write_metrics(path, metrics)
    }

    fn write_squads(&self, path: &Path, metrics: &SquadsRunMetrics) -> Result<()> {
        write_atomically(path, &render_squads_prometheus(metrics)?)
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
            "Configured bond value floor in SOL",
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

/// One Squads-mode run's metrics; unknown values are left out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SquadsRunMetrics {
    pub vote: Option<Pubkey>,
    pub last_run_timestamp_seconds: u64,
    pub success: bool,
    pub bond_value_lamports: Option<u64>,
    pub bond_jsol: Option<u64>,
    pub jsol_sol_rate: Option<f64>,
    pub burn_lamports_per_day: Option<f64>,
    pub runway_days: Option<f64>,
    pub floor_lamports: Option<u64>,
    pub propose_runway_days: Option<f64>,
    pub critical_runway_days: Option<f64>,
    pub proposal_pending: Option<bool>,
    pub proposal_index: Option<u64>,
    pub proposal_age_seconds: Option<i64>,
    pub proposal_amount_lamports: Option<u64>,
    pub last_proposal_created_timestamp_seconds: Option<i64>,
    pub vote_withdrawable_lamports: Option<u64>,
    pub proposer_lamports: Option<u64>,
}

/// Prometheus float text: `+Inf` for an unbounded runway.
fn float(value: f64, decimals: usize) -> String {
    if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else {
        format!("{value:.decimals$}")
    }
}

pub fn render_squads_prometheus(metrics: &SquadsRunMetrics) -> Result<String> {
    let labels = format!(
        "vote=\"{}\"",
        metrics
            .vote
            .map(|vote| vote.to_string())
            .unwrap_or_default()
    );
    let mut output = String::new();
    let sol = |value: Option<u64>| value.map(format_base_units);
    let series: [(&str, &str, Option<String>); 17] = [
        (
            "jpool_bond_value_sol",
            "SOL value of the JSOL held by the JPool performance bond",
            sol(metrics.bond_value_lamports),
        ),
        (
            "jpool_bond_jsol",
            "JSOL held by the JPool performance bond",
            sol(metrics.bond_jsol),
        ),
        (
            "jpool_bond_jsol_sol_rate",
            "SOL per JSOL from the JPool stake pool",
            metrics.jsol_sol_rate.map(|rate| format!("{rate:.9}")),
        ),
        (
            "jpool_bond_burn_sol_per_day",
            "Estimated bond burn (claims) in SOL per day",
            metrics
                .burn_lamports_per_day
                .map(|lamports| float(lamports / 1e9, 9)),
        ),
        (
            "jpool_bond_runway_days",
            "Days until the bond value reaches the floor at the burn rate",
            metrics.runway_days.map(|days| float(days, 3)),
        ),
        (
            "jpool_bond_floor_sol",
            "Configured bond value floor in SOL",
            sol(metrics.floor_lamports),
        ),
        (
            "jpool_bond_propose_runway_days",
            "Runway below which a Squads top-up proposal is created",
            metrics.propose_runway_days.map(|days| float(days, 3)),
        ),
        (
            "jpool_bond_critical_runway_days",
            "Runway below which the bond is critical (alerting threshold)",
            metrics.critical_runway_days.map(|days| float(days, 3)),
        ),
        (
            "jpool_bond_proposal_pending",
            "Whether one of our top-up proposals awaits approval or execution",
            metrics
                .proposal_pending
                .map(|pending| u8::from(pending).to_string()),
        ),
        (
            "jpool_bond_proposal_index",
            "Squads transaction index of the pending top-up proposal",
            metrics.proposal_index.map(|index| index.to_string()),
        ),
        (
            "jpool_bond_proposal_age_seconds",
            "Age of the pending top-up proposal",
            metrics.proposal_age_seconds.map(|age| age.to_string()),
        ),
        (
            "jpool_bond_proposal_amount_sol",
            "SOL the pending top-up proposal moves into the bond",
            sol(metrics.proposal_amount_lamports),
        ),
        (
            "jpool_bond_last_proposal_created_timestamp_seconds",
            "Unix time our latest top-up proposal was created",
            metrics
                .last_proposal_created_timestamp_seconds
                .map(|time| time.to_string()),
        ),
        (
            "jpool_bond_vote_withdrawable_sol",
            "SOL withdrawable from the vote account above its rent-exempt minimum",
            sol(metrics.vote_withdrawable_lamports),
        ),
        (
            "jpool_bond_proposer_balance_sol",
            "SOL balance of the Squads proposer key",
            sol(metrics.proposer_lamports),
        ),
        (
            "jpool_bond_last_run_success",
            "Whether the last run succeeded (refusals of a due proposal count as failures)",
            Some(u8::from(metrics.success).to_string()),
        ),
        (
            "jpool_bond_last_run_timestamp_seconds",
            "Unix timestamp of the last run",
            Some(metrics.last_run_timestamp_seconds.to_string()),
        ),
    ];
    for (name, help, value) in series {
        if let Some(value) = value {
            gauge(&mut output, name, help, &labels, &value)?;
        }
    }
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
    fn renders_squads_metrics_and_omits_unknown() {
        let label = "{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"}";
        let body = render_squads_prometheus(&SquadsRunMetrics {
            vote: Some(pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr")),
            last_run_timestamp_seconds: 1_700_000_000,
            success: true,
            bond_value_lamports: Some(14_800_000_000),
            burn_lamports_per_day: Some(66_000_000.0),
            runway_days: Some(f64::INFINITY),
            floor_lamports: Some(5_000_000_000),
            propose_runway_days: Some(10.0),
            critical_runway_days: Some(3.0),
            proposal_pending: Some(true),
            proposal_index: Some(4),
            proposal_age_seconds: Some(120),
            proposal_amount_lamports: Some(2_500_000_000),
            last_proposal_created_timestamp_seconds: Some(1_699_999_880),
            vote_withdrawable_lamports: Some(59_700_000_000),
            proposer_lamports: Some(50_000_000),
            ..SquadsRunMetrics::default()
        })
        .unwrap();
        for line in [
            format!("jpool_bond_value_sol{label} 14.8"),
            format!("jpool_bond_burn_sol_per_day{label} 0.066000000"),
            format!("jpool_bond_runway_days{label} +Inf"),
            format!("jpool_bond_floor_sol{label} 5"),
            format!("jpool_bond_propose_runway_days{label} 10.000"),
            format!("jpool_bond_critical_runway_days{label} 3.000"),
            format!("jpool_bond_proposal_pending{label} 1"),
            format!("jpool_bond_proposal_index{label} 4"),
            format!("jpool_bond_proposal_age_seconds{label} 120"),
            format!("jpool_bond_proposal_amount_sol{label} 2.5"),
            format!("jpool_bond_last_proposal_created_timestamp_seconds{label} 1699999880"),
            format!("jpool_bond_vote_withdrawable_sol{label} 59.7"),
            format!("jpool_bond_proposer_balance_sol{label} 0.05"),
            format!("jpool_bond_last_run_success{label} 1"),
            format!("jpool_bond_last_run_timestamp_seconds{label} 1700000000"),
        ] {
            assert!(body.contains(&line), "missing {line} in\n{body}");
        }
        assert!(!body.contains("jpool_bond_jsol{"));
        assert!(!body.contains("jpool_bond_last_topup_sol"));
        assert!(!body.contains("jpool_bond_min_value_sol"));
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
