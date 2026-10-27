use anyhow::Result;
use clap::{Parser, Subcommand};
use jpool_bond_rs::{
    Addresses, Assessment, ChainDataSource, Decision, MAX_ATTEMPTS, RpcChainDataSource,
    config::{Config, format_base_units, format_sol, redact_rpc_url},
    metrics::{FileMetricsSink, MetricsSink, RunMetrics},
    plan, render_assessment,
    transaction::submit_with_backend,
    verify_topup,
};
use solana_client::rpc_client::RpcClient;
use solana_instruction::Instruction;
use solana_keypair::{Keypair, read_keypair_file};
use solana_signature::Signature;
use solana_signer::Signer;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Parser)]
#[command(about = "Guarded JPool performance bond (JSOL) top-up")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read-only: evaluate guards and print the decision. Never loads the keypair.
    Plan {
        #[arg(long)]
        config: PathBuf,
    },
    /// Evaluate, and if the bond is below the floor, sign and submit one top-up.
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Plan,
    Run,
}

/// The side-effecting half of `run`, separated so tests can stub it.
trait Executor {
    fn submit(&self, payer: &Keypair, instructions: &[Instruction]) -> Result<Signature>;
    fn verify(&self, addresses: &Addresses, before_jsol: u64, topup_jsol: u64) -> Result<u64>;
}

struct RpcExecutor<'a> {
    client: &'a RpcClient,
    priority_fee_micro_lamports: u64,
}

impl Executor for RpcExecutor<'_> {
    fn submit(&self, payer: &Keypair, instructions: &[Instruction]) -> Result<Signature> {
        submit_with_backend(
            self.client,
            payer,
            instructions,
            self.priority_fee_micro_lamports,
            MAX_ATTEMPTS,
        )
    }

    fn verify(&self, addresses: &Addresses, before_jsol: u64, topup_jsol: u64) -> Result<u64> {
        verify_topup(self.client, addresses, before_jsol, topup_jsol)
    }
}

fn main() -> ExitCode {
    match run_cli(Cli::parse(), &FileMetricsSink) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn run_cli<S: MetricsSink>(cli: Cli, sink: &S) -> Result<()> {
    let (config_path, mode) = match &cli.command {
        Command::Plan { config } => (config.as_path(), Mode::Plan),
        Command::Run { config } => (config.as_path(), Mode::Run),
    };
    let config = load_config(config_path, sink)?;
    // The default finalized commitment rejects confirmed blockhashes.
    let client = RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_commitment_config::CommitmentConfig::confirmed(),
    );
    let source = RpcChainDataSource { client: &client };
    let executor = RpcExecutor {
        client: &client,
        priority_fee_micro_lamports: config.priority_fee_micro_lamports,
    };
    // Client errors can quote the request URL, which carries the API key.
    execute(mode, &config, sink, &source, &executor, now_seconds())
        .map_err(|error| anyhow::anyhow!(redact_rpc_url(&format!("{error:#}"), &config.rpc_url)))
}

fn load_config<S: MetricsSink>(path: &Path, sink: &S) -> Result<Config> {
    match Config::load(path) {
        Ok(config) => Ok(config),
        Err(error) => {
            if let Some(hint) = Config::metrics_hint(path) {
                let metrics = RunMetrics {
                    vote: hint.vote_account,
                    last_run_timestamp_seconds: now_seconds(),
                    ..RunMetrics::default()
                };
                return Err(with_failure_metrics(
                    sink,
                    &hint.metrics_file,
                    &metrics,
                    error,
                ));
            }
            Err(error)
        }
    }
}

fn base_metrics(config: &Config, now: u64) -> RunMetrics {
    RunMetrics {
        vote: Some(config.vote_account),
        last_run_timestamp_seconds: now,
        min_value_lamports: Some(config.min_value_lamports),
        ..RunMetrics::default()
    }
}

fn assessment_metrics(config: &Config, assessment: &Assessment, now: u64) -> RunMetrics {
    RunMetrics {
        bond_value_lamports: Some(assessment.bond_value_lamports),
        bond_jsol: Some(assessment.bond_jsol),
        jsol_sol_rate: Some(assessment.jsol_sol_rate),
        identity_lamports: Some(assessment.identity_lamports),
        ..base_metrics(config, now)
    }
}

fn with_failure_metrics<S: MetricsSink>(
    sink: &S,
    path: &Path,
    metrics: &RunMetrics,
    primary: anyhow::Error,
) -> anyhow::Error {
    let mut metrics = metrics.clone();
    metrics.success = false;
    match sink.write(path, &metrics) {
        Ok(()) => primary,
        Err(metrics_error) => anyhow::anyhow!(
            "{primary:#}; additionally failed to update failure metrics: {metrics_error:#}"
        ),
    }
}

fn record_success<S: MetricsSink>(
    sink: &S,
    path: &Path,
    metrics: &RunMetrics,
    topup_confirmed: bool,
) -> Result<()> {
    let mut metrics = metrics.clone();
    metrics.success = true;
    sink.write(path, &metrics).map_err(|error| {
        if topup_confirmed {
            anyhow::anyhow!("top-up confirmed and verified; metrics update failed: {error:#}")
        } else {
            anyhow::anyhow!("run succeeded; metrics update failed: {error:#}")
        }
    })
}

fn execute<S: MetricsSink, D: ChainDataSource, E: Executor>(
    mode: Mode,
    config: &Config,
    sink: &S,
    source: &D,
    executor: &E,
    now: u64,
) -> Result<()> {
    let path = config.metrics_file.as_path();
    if let Err(error) = sink.preflight(path) {
        return Err(with_failure_metrics(
            sink,
            path,
            &base_metrics(config, now),
            error,
        ));
    }
    let now_unix = i64::try_from(now)?;
    let assessment = match plan(source, config, now_unix) {
        Ok(assessment) => assessment,
        Err(error) => {
            return Err(with_failure_metrics(
                sink,
                path,
                &base_metrics(config, now),
                error,
            ));
        }
    };
    println!("{}", render_assessment(&assessment));
    let mut metrics = assessment_metrics(config, &assessment, now);

    let topup = match &assessment.decision {
        Decision::Refused(reason) => {
            return Err(with_failure_metrics(
                sink,
                path,
                &metrics,
                anyhow::anyhow!("refused: {reason}"),
            ));
        }
        Decision::NoTopUp if mode == Mode::Plan => {
            return record_success(sink, path, &metrics, false);
        }
        Decision::TopUp(_) if mode == Mode::Plan => {
            println!("plan only: nothing signed or sent");
            return record_success(sink, path, &metrics, false);
        }
        Decision::NoTopUp => None,
        Decision::TopUp(topup) => Some(topup),
    };

    // Run mode: the keypair is loaded and checked even when nothing is due,
    // so a broken key path shows up before the bond actually needs money.
    let payer = match load_identity(config) {
        Ok(payer) => payer,
        Err(error) => return Err(with_failure_metrics(sink, path, &metrics, error)),
    };
    let Some(topup) = topup else {
        return record_success(sink, path, &metrics, false);
    };

    let signature = match executor.submit(&payer, &topup.instructions) {
        Ok(signature) => signature,
        Err(error) => {
            return Err(with_failure_metrics(
                sink,
                path,
                &metrics,
                error.context("top-up transaction failed"),
            ));
        }
    };
    println!("submitted and confirmed: {signature}");
    metrics.last_topup_lamports = topup.quote.lamports;
    metrics.identity_lamports = None;
    match executor.verify(
        &assessment.addresses,
        assessment.bond_jsol,
        topup.topup_jsol,
    ) {
        Ok(after) => {
            metrics.bond_jsol = Some(after);
            metrics.bond_value_lamports = Some(
                u64::try_from(
                    (after as u128) * (assessment.pool_total_lamports as u128)
                        / (assessment.pool_token_supply as u128),
                )
                .unwrap_or(u64::MAX),
            );
            println!(
                "verified: bond JSOL {} -> {} (+{} JSOL for {} SOL)",
                format_base_units(assessment.bond_jsol),
                format_base_units(after),
                format_base_units(topup.topup_jsol),
                format_sol(topup.quote.lamports)
            );
            record_success(sink, path, &metrics, true)
        }
        Err(error) => Err(with_failure_metrics(
            sink,
            path,
            &metrics,
            error.context(format!(
                "top-up {signature} confirmed but bond verification failed"
            )),
        )),
    }
}

fn load_identity(config: &Config) -> Result<Keypair> {
    let keypair = read_keypair_file(&config.identity_keypair_path).map_err(|_| {
        anyhow::anyhow!(
            "failed to read identity keypair {}",
            config.identity_keypair_path.display()
        )
    })?;
    if keypair.pubkey() != config.identity_pubkey {
        anyhow::bail!(
            "identity keypair {} is {}, expected identity_pubkey {}",
            config.identity_keypair_path.display(),
            keypair.pubkey(),
            config.identity_pubkey
        );
    }
    Ok(keypair)
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
