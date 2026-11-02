//! `plan` and `run` in Squads mode.

use crate::Mode;
use anyhow::Result;
use jpool_bond_rs::{
    MAX_ATTEMPTS,
    config::{SquadsConfig, format_sol},
    metrics::{MetricsSink, SquadsRunMetrics},
    squads_mode::{
        ProposalPlan, SquadsAddresses, SquadsAssessment, SquadsDataSource, SquadsDecision,
        plan_squads, render_squads_assessment, verify_proposal,
    },
    transaction::submit_with_backend,
};
use solana_client::rpc_client::RpcClient;
use solana_instruction::Instruction;
use solana_keypair::{Keypair, read_keypair_file};
use solana_signature::Signature;
use solana_signer::Signer;
use std::path::Path;

/// The side-effecting half of a Squads `run`, separated so tests can stub it.
pub(crate) trait SquadsExecutor {
    fn submit(&self, payer: &Keypair, instructions: &[Instruction]) -> Result<Signature>;
    fn verify(&self, addresses: &SquadsAddresses, plan: &ProposalPlan) -> Result<()>;
}

pub(crate) struct RpcSquadsExecutor<'a> {
    pub client: &'a RpcClient,
    pub priority_fee_micro_lamports: u64,
}

impl SquadsExecutor for RpcSquadsExecutor<'_> {
    fn submit(&self, payer: &Keypair, instructions: &[Instruction]) -> Result<Signature> {
        submit_with_backend(
            self.client,
            payer,
            instructions,
            self.priority_fee_micro_lamports,
            MAX_ATTEMPTS,
        )
    }

    fn verify(&self, addresses: &SquadsAddresses, plan: &ProposalPlan) -> Result<()> {
        verify_proposal(self.client, addresses, plan)
    }
}

pub(crate) fn base_metrics(config: &SquadsConfig, now: u64) -> SquadsRunMetrics {
    SquadsRunMetrics {
        vote: Some(config.vote_account),
        last_run_timestamp_seconds: now,
        floor_lamports: Some(config.floor_lamports),
        propose_runway_days: Some(config.propose_runway_days),
        critical_runway_days: Some(config.critical_runway_days),
        ..SquadsRunMetrics::default()
    }
}

fn assessment_metrics(
    config: &SquadsConfig,
    assessment: &SquadsAssessment,
    now: u64,
) -> SquadsRunMetrics {
    let a = assessment;
    let now_unix = i64::try_from(now).unwrap_or(i64::MAX);
    SquadsRunMetrics {
        bond_value_lamports: Some(a.bond_value_lamports),
        bond_jsol: Some(a.bond_jsol),
        jsol_sol_rate: Some(a.jsol_sol_rate),
        burn_lamports_per_day: Some(a.burn.lamports_per_day),
        runway_days: Some(a.runway_days),
        proposal_pending: Some(a.pending.is_some()),
        proposal_index: a.pending.as_ref().map(|pending| pending.index),
        proposal_age_seconds: a
            .pending
            .as_ref()
            .and_then(|pending| pending.created_at)
            .map(|created| now_unix - created),
        proposal_amount_lamports: a.pending.as_ref().and_then(|p| p.amount_lamports),
        last_proposal_created_timestamp_seconds: a.last_created_at(),
        vote_withdrawable_lamports: Some(a.vote_withdrawable_lamports),
        proposer_lamports: Some(a.proposer_lamports),
        ..base_metrics(config, now)
    }
}

pub(crate) fn with_failure_metrics<S: MetricsSink>(
    sink: &S,
    path: &Path,
    metrics: &SquadsRunMetrics,
    primary: anyhow::Error,
) -> anyhow::Error {
    let mut metrics = metrics.clone();
    metrics.success = false;
    match sink.write_squads(path, &metrics) {
        Ok(()) => primary,
        Err(metrics_error) => anyhow::anyhow!(
            "{primary:#}; additionally failed to update failure metrics: {metrics_error:#}"
        ),
    }
}

fn record_success<S: MetricsSink>(
    sink: &S,
    path: &Path,
    metrics: &SquadsRunMetrics,
    proposal_created: bool,
) -> Result<()> {
    let mut metrics = metrics.clone();
    metrics.success = true;
    sink.write_squads(path, &metrics).map_err(|error| {
        if proposal_created {
            anyhow::anyhow!("proposal created and verified; metrics update failed: {error:#}")
        } else {
            anyhow::anyhow!("run succeeded; metrics update failed: {error:#}")
        }
    })
}

pub(crate) fn execute_squads<S: MetricsSink, D: SquadsDataSource, E: SquadsExecutor>(
    mode: Mode,
    config: &SquadsConfig,
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
    let assessment = match plan_squads(source, config, now_unix) {
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
    println!("{}", render_squads_assessment(&assessment, now_unix));
    let mut metrics = assessment_metrics(config, &assessment, now);

    // A refusal only exists when a proposal was due; a healthy bond or an
    // open proposal is a successful run.
    let proposal = match &assessment.decision {
        SquadsDecision::Refused(reason) => {
            return Err(with_failure_metrics(
                sink,
                path,
                &metrics,
                anyhow::anyhow!("refused: {reason}"),
            ));
        }
        SquadsDecision::Propose(_) if mode == Mode::Plan => {
            println!("plan only: nothing signed or sent");
            return record_success(sink, path, &metrics, false);
        }
        _ if mode == Mode::Plan => return record_success(sink, path, &metrics, false),
        SquadsDecision::Propose(plan) => Some(plan),
        SquadsDecision::Healthy | SquadsDecision::Pending => None,
    };

    // Run mode: the proposer key is loaded and checked even when nothing is
    // due, so a broken key path shows up before the bond needs money.
    let proposer = match load_proposer(config) {
        Ok(proposer) => proposer,
        Err(error) => return Err(with_failure_metrics(sink, path, &metrics, error)),
    };
    let Some(plan) = proposal else {
        return record_success(sink, path, &metrics, false);
    };

    let signature = match executor.submit(&proposer, &plan.instructions) {
        Ok(signature) => signature,
        Err(error) => {
            return Err(with_failure_metrics(
                sink,
                path,
                &metrics,
                error.context("proposal transaction failed"),
            ));
        }
    };
    println!("submitted and confirmed: {signature}");
    metrics.proposal_pending = Some(true);
    metrics.proposal_index = Some(plan.transaction_index);
    metrics.proposal_age_seconds = Some(0);
    metrics.proposal_amount_lamports = Some(plan.topup_lamports);
    metrics.last_proposal_created_timestamp_seconds = Some(now_unix);
    metrics.proposer_lamports = None;
    match executor.verify(&assessment.addresses, plan) {
        Ok(()) => {
            println!(
                "verified: proposal #{} ({}) is Active with the {} intended instructions; approve and execute it in the Squads UI to top up {} SOL",
                plan.transaction_index,
                plan.proposal,
                plan.vault_instructions.len(),
                format_sol(plan.topup_lamports)
            );
            record_success(sink, path, &metrics, true)
        }
        Err(error) => Err(with_failure_metrics(
            sink,
            path,
            &metrics,
            error.context(format!(
                "proposal transaction {signature} confirmed but verification failed"
            )),
        )),
    }
}

fn load_proposer(config: &SquadsConfig) -> Result<Keypair> {
    let path = &config.squads.proposer_keypair_path;
    let keypair = read_keypair_file(path)
        .map_err(|_| anyhow::anyhow!("failed to read proposer keypair {}", path.display()))?;
    if keypair.pubkey() != config.squads.proposer_pubkey {
        anyhow::bail!(
            "proposer keypair {} is {}, expected squads.proposer_pubkey {}",
            path.display(),
            keypair.pubkey(),
            config.squads.proposer_pubkey
        );
    }
    Ok(keypair)
}

#[cfg(test)]
#[path = "squads_cli_tests.rs"]
mod tests;
