//! Squads mode: estimates the bond's burn rate and runway, and when the
//! runway is short proposes one atomic vault transaction to the owner's
//! Squads v4 multisig: vote Withdraw -> vault, create the vault's JSOL ATA,
//! JPool DepositSol, bond_top_up. The proposer only creates the proposal;
//! members approve and execute it in the Squads UI.

use crate::{
    BondAccounts, BondAddresses, BondCheck, ChainAccount, TOPUP_COOLDOWN_SECONDS,
    bond::{
        BOND_PROGRAM_ID, BOND_TOP_UP_DISCRIMINATOR, BondState, BondStatus, ValidatorBond,
        bond_top_up_token_instruction, find_bond_state_address, find_validator_bond_address,
    },
    burn::{BondHistory, BurnEstimate, BurnSource, SECONDS_PER_DAY, estimate_burn},
    check_bond_and_pool,
    config::{
        HARD_MAX_PROPOSAL_SOL, LAMPORTS_PER_SOL, MIN_PROPOSAL_SOL, SquadsConfig, format_base_units,
        format_sol,
    },
    fee_buffer_lamports,
    pool::{
        DepositQuote, JPOOL_STAKE_POOL, STAKE_POOL_PROGRAM_ID, deposit_sol_instruction, jsol_mint,
    },
    squads::{
        Member, Multisig, ProposalStatus, SYSTEM_PROGRAM_ID, VaultMessage,
        VaultTransactionCreateArgs, decode_multisig, decode_proposal, decode_vault_transaction,
        proposal_account_len, proposal_address, proposal_create_instruction, transaction_address,
        vault_address, vault_transaction_account_len, vault_transaction_create_instruction,
    },
    token::{
        TOKEN_ACCOUNT_LEN, associated_token_address, token_program_id, validate_token_account,
    },
    transaction::SimulationResult,
    vote::{
        VoteAccountInfo, decode_vote_account, vote_withdraw_instruction, vote_withdraw_lamports,
        withdrawable_lamports,
    },
};
use anyhow::{Context, Result, bail};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig};
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent;
use std::{thread, time::Duration};

/// Proposal amounts are rounded up to this step.
pub const PROPOSAL_STEP_LAMPORTS: u64 = LAMPORTS_PER_SOL / 2;
pub const MAX_TRANSACTION_SIZE: usize = 1_232;
const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
const VERIFY_POLLS: usize = 30;
const VERIFY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Every account Squads mode touches, derived from the config alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SquadsAddresses {
    pub vote_account: Pubkey,
    pub bond_state: Pubkey,
    pub validator_bond: Pubkey,
    pub bond_token_account: Pubkey,
    pub multisig: Pubkey,
    pub vault_index: u8,
    pub vault: Pubkey,
    pub vault_token_account: Pubkey,
    pub proposer: Pubkey,
}

impl SquadsAddresses {
    pub fn derive(config: &SquadsConfig) -> Self {
        let bond_state = find_bond_state_address(&config.bond_name);
        let validator_bond = find_validator_bond_address(&bond_state, &config.vote_account);
        let vault = vault_address(&config.squads.multisig, config.squads.vault_index);
        Self {
            vote_account: config.vote_account,
            bond_state,
            validator_bond,
            bond_token_account: associated_token_address(&validator_bond, &jsol_mint()),
            multisig: config.squads.multisig,
            vault_index: config.squads.vault_index,
            vault,
            vault_token_account: associated_token_address(&vault, &jsol_mint()),
            proposer: config.squads.proposer_pubkey,
        }
    }

    fn bond(&self) -> BondAddresses {
        BondAddresses {
            bond_state: self.bond_state,
            validator_bond: self.validator_bond,
            bond_token_account: self.bond_token_account,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SquadsSnapshot {
    pub current_epoch: u64,
    pub bond_state: Option<ChainAccount>,
    pub validator_bond: Option<ChainAccount>,
    pub bond_token_account: Option<ChainAccount>,
    pub stake_pool: Option<ChainAccount>,
    pub jsol_mint: Option<ChainAccount>,
    pub multisig: Option<ChainAccount>,
    pub vote_account: Option<ChainAccount>,
    pub vault: Option<ChainAccount>,
    pub vault_token_account: Option<ChainAccount>,
    pub proposer_lamports: u64,
}

/// The proposal and vault transaction accounts at one transaction index.
#[derive(Clone, Debug)]
pub struct ScannedTransaction {
    pub index: u64,
    pub proposal: Option<ChainAccount>,
    pub transaction: Option<ChainAccount>,
}

pub trait SquadsDataSource {
    fn snapshot(&self, addresses: &SquadsAddresses) -> Result<SquadsSnapshot>;
    fn rent_exempt_minimum(&self, data_len: usize) -> Result<u64>;
    fn scan_transactions(
        &self,
        multisig: &Pubkey,
        indexes: &[u64],
    ) -> Result<Vec<ScannedTransaction>>;
    /// Block time of the oldest transaction that touched `account`.
    fn creation_time(&self, account: &Pubkey) -> Result<Option<i64>>;
    fn bond_history(&self, addresses: &SquadsAddresses, window_start: i64) -> Result<BondHistory>;
    /// Runs the vault transaction's instructions as a standalone transaction
    /// paid by the vault, without signature verification. Read-only.
    fn simulate_vault_transaction(
        &self,
        vault: &Pubkey,
        instructions: &[Instruction],
    ) -> Result<SimulationResult>;
}

pub struct RpcSquadsDataSource<'a> {
    pub client: &'a RpcClient,
}

// The RPC account type lives in a crate this package does not depend on.
macro_rules! chain_account {
    ($account:expr) => {
        $account.map(|account| ChainAccount {
            owner: account.owner,
            lamports: account.lamports,
            data: account.data.clone(),
        })
    };
}

impl SquadsDataSource for RpcSquadsDataSource<'_> {
    fn snapshot(&self, addresses: &SquadsAddresses) -> Result<SquadsSnapshot> {
        let current_epoch = self
            .client
            .get_epoch_info_with_commitment(CommitmentConfig::confirmed())
            .context("failed to fetch epoch info")?
            .epoch;
        let keys = [
            addresses.bond_state,
            addresses.validator_bond,
            addresses.bond_token_account,
            JPOOL_STAKE_POOL,
            jsol_mint(),
            addresses.multisig,
            addresses.vote_account,
            addresses.vault,
            addresses.vault_token_account,
        ];
        let accounts = self
            .client
            .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
            .context("failed to fetch bond, pool and multisig accounts")?
            .value;
        if accounts.len() != keys.len() {
            bail!(
                "RPC returned {} accounts for {} requested",
                accounts.len(),
                keys.len()
            );
        }
        let account = |index: usize| chain_account!(accounts[index].as_ref());
        let proposer_lamports = self
            .client
            .get_balance_with_commitment(&addresses.proposer, CommitmentConfig::confirmed())
            .context("failed to fetch proposer balance")?
            .value;
        Ok(SquadsSnapshot {
            current_epoch,
            bond_state: account(0),
            validator_bond: account(1),
            bond_token_account: account(2),
            stake_pool: account(3),
            jsol_mint: account(4),
            multisig: account(5),
            vote_account: account(6),
            vault: account(7),
            vault_token_account: account(8),
            proposer_lamports,
        })
    }

    fn rent_exempt_minimum(&self, data_len: usize) -> Result<u64> {
        self.client
            .get_minimum_balance_for_rent_exemption(data_len)
            .with_context(|| format!("failed to fetch rent for {data_len} bytes"))
    }

    fn scan_transactions(
        &self,
        multisig: &Pubkey,
        indexes: &[u64],
    ) -> Result<Vec<ScannedTransaction>> {
        if indexes.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<Pubkey> = indexes
            .iter()
            .flat_map(|index| {
                [
                    proposal_address(multisig, *index),
                    transaction_address(multisig, *index),
                ]
            })
            .collect();
        let accounts = self
            .client
            .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
            .context("failed to fetch Squads proposals")?
            .value;
        if accounts.len() != keys.len() {
            bail!(
                "RPC returned {} accounts for {} requested",
                accounts.len(),
                keys.len()
            );
        }
        Ok(indexes
            .iter()
            .enumerate()
            .map(|(position, index)| ScannedTransaction {
                index: *index,
                proposal: chain_account!(accounts[2 * position].as_ref()),
                transaction: chain_account!(accounts[2 * position + 1].as_ref()),
            })
            .collect())
    }

    fn creation_time(&self, account: &Pubkey) -> Result<Option<i64>> {
        let signatures = self
            .client
            .get_signatures_for_address_with_config(
                account,
                solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(1_000),
                    commitment: Some(CommitmentConfig::confirmed()),
                },
            )
            .with_context(|| format!("failed to fetch signatures of {account}"))?;
        Ok(signatures.last().and_then(|entry| entry.block_time))
    }

    fn bond_history(&self, addresses: &SquadsAddresses, window_start: i64) -> Result<BondHistory> {
        crate::burn::fetch_bond_history(
            self.client,
            &addresses.bond_token_account,
            &jsol_mint(),
            window_start,
        )
    }

    fn simulate_vault_transaction(
        &self,
        vault: &Pubkey,
        instructions: &[Instruction],
    ) -> Result<SimulationResult> {
        let transaction = Transaction::new_with_payer(instructions, Some(vault));
        let value = self
            .client
            .simulate_transaction_with_config(
                &transaction,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..RpcSimulateTransactionConfig::default()
                },
            )
            .context("vault transaction simulation RPC failed")?
            .value;
        Ok(SimulationResult {
            units_consumed: value.units_consumed,
            error: value.err.map(|error| error.to_string()),
            logs: value.logs.unwrap_or_default(),
        })
    }
}

/// A proposal in the scan window whose vault transaction tops up our bond.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OurProposal {
    pub index: u64,
    pub transaction: Pubkey,
    pub status: ProposalStatus,
    /// At or below the multisig's stale_transaction_index: Draft and
    /// Active proposals can no longer be approved.
    pub stale: bool,
    /// SOL the vault transaction moves (vote Withdraw, else DepositSol).
    pub amount_lamports: Option<u64>,
    pub created_at: Option<i64>,
}

impl OurProposal {
    pub fn is_pending(&self) -> bool {
        match self.status {
            ProposalStatus::Draft(_) | ProposalStatus::Active(_) => !self.stale,
            ProposalStatus::Approved(_) | ProposalStatus::Executing => true,
            _ => false,
        }
    }
}

/// True when the message has a bond_top_up whose validator_bond is ours.
/// An account behind a lookup table cannot be resolved offline and is
/// counted as ours, so an unclear proposal blocks rather than duplicates.
pub fn is_our_topup(message: &VaultMessage, validator_bond: &Pubkey) -> bool {
    message.instructions.iter().any(|instruction| {
        message.static_key(instruction.program_id_index) == Some(BOND_PROGRAM_ID)
            && instruction.data.starts_with(&BOND_TOP_UP_DISCRIMINATOR)
            && instruction.account_indexes.get(1).is_none_or(|index| {
                match message.static_key(*index) {
                    Some(key) => key == *validator_bond,
                    None => true,
                }
            })
    })
}

fn message_amount(message: &VaultMessage) -> Option<u64> {
    let resolved: Vec<(Pubkey, &[u8])> = message
        .instructions
        .iter()
        .filter_map(|instruction| {
            message
                .static_key(instruction.program_id_index)
                .map(|program| (program, instruction.data.as_slice()))
        })
        .collect();
    resolved
        .iter()
        .find_map(|(program, data)| vote_withdraw_lamports(program, data))
        .or_else(|| {
            resolved.iter().find_map(|(program, data)| {
                // StakePoolInstruction::DepositSol is borsh tag 14.
                if *program == STAKE_POOL_PROGRAM_ID && data.len() == 9 && data[0] == 14 {
                    Some(u64::from_le_bytes(data[1..9].try_into().ok()?))
                } else {
                    None
                }
            })
        })
}

/// Our top-up proposals among the scanned indexes, highest index first.
pub fn classify_proposals(
    multisig_address: &Pubkey,
    multisig: &Multisig,
    validator_bond: &Pubkey,
    scanned: &[ScannedTransaction],
) -> Result<Vec<OurProposal>> {
    let mut ours = Vec::new();
    for entry in scanned {
        let (Some(proposal_account), Some(transaction_account)) =
            (&entry.proposal, &entry.transaction)
        else {
            continue;
        };
        let proposal_key = proposal_address(multisig_address, entry.index);
        let transaction_key = transaction_address(multisig_address, entry.index);
        let proposal = decode_proposal(&proposal_key, proposal_account)?;
        let transaction = decode_vault_transaction(&transaction_key, transaction_account)?;
        if proposal.multisig != *multisig_address
            || proposal.transaction_index != entry.index
            || transaction.multisig != *multisig_address
            || transaction.index != entry.index
        {
            bail!(
                "Squads accounts at index {} do not belong to multisig {multisig_address}",
                entry.index
            );
        }
        if !is_our_topup(&transaction.message, validator_bond) {
            continue;
        }
        ours.push(OurProposal {
            index: entry.index,
            transaction: transaction_key,
            status: proposal.status,
            stale: entry.index <= multisig.stale_transaction_index,
            amount_lamports: message_amount(&transaction.message),
            created_at: None,
        });
    }
    ours.sort_by(|a, b| b.index.cmp(&a.index));
    Ok(ours)
}

/// X = burn/day * topup_days + max(0, floor - value), rounded up to 0.5
/// SOL, at least 1 SOL, at most the 20 SOL hard cap.
pub fn proposal_lamports(
    burn_lamports_per_day: f64,
    topup_days: f64,
    floor_lamports: u64,
    bond_value_lamports: u64,
) -> u64 {
    let cap = HARD_MAX_PROPOSAL_SOL * LAMPORTS_PER_SOL;
    let burn = (burn_lamports_per_day * topup_days).ceil();
    let burn = if burn.is_finite() && burn > 0.0 {
        burn.min(cap as f64) as u64
    } else {
        0
    };
    let deficit = floor_lamports.saturating_sub(bond_value_lamports);
    let need = burn.saturating_add(deficit).min(cap);
    let rounded = need.div_ceil(PROPOSAL_STEP_LAMPORTS) * PROPOSAL_STEP_LAMPORTS;
    rounded.clamp(MIN_PROPOSAL_SOL * LAMPORTS_PER_SOL, cap)
}

/// Days until the bond value reaches the floor at the burn rate; 0 at or
/// below the floor, infinite with no burn.
pub fn runway_days(bond_value_lamports: u64, floor_lamports: u64, lamports_per_day: f64) -> f64 {
    if bond_value_lamports <= floor_lamports {
        return 0.0;
    }
    if lamports_per_day <= 0.0 {
        return f64::INFINITY;
    }
    (bond_value_lamports - floor_lamports) as f64 / lamports_per_day
}

fn short_vec_len(len: usize) -> usize {
    match len {
        0..0x80 => 1,
        0x80..0x4000 => 2,
        _ => 3,
    }
}

/// Serialized size of the signed legacy transaction, compute budget included.
pub fn transaction_size(payer: &Pubkey, instructions: &[Instruction]) -> usize {
    let mut all = vec![
        ComputeBudgetInstruction::set_compute_unit_price(0),
        ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT),
    ];
    all.extend_from_slice(instructions);
    let message = Transaction::new_with_payer(&all, Some(payer)).message;
    let signatures = usize::from(message.header.num_required_signatures);
    short_vec_len(signatures)
        + 64 * signatures
        + 3
        + short_vec_len(message.account_keys.len())
        + 32 * message.account_keys.len()
        + 32
        + short_vec_len(message.instructions.len())
        + message
            .instructions
            .iter()
            .map(|instruction| {
                1 + short_vec_len(instruction.accounts.len())
                    + instruction.accounts.len()
                    + short_vec_len(instruction.data.len())
                    + instruction.data.len()
            })
            .sum::<usize>()
}

#[derive(Clone, Debug)]
pub struct ProposalPlan {
    pub transaction_index: u64,
    pub transaction: Pubkey,
    pub proposal: Pubkey,
    pub topup_lamports: u64,
    pub quote: DepositQuote,
    pub jsol_margin_bps: u16,
    /// JSOL passed to bond_top_up: the quoted mint minus the margin.
    pub topup_jsol: u64,
    /// The four instructions the vault executes after approval.
    pub vault_instructions: Vec<Instruction>,
    pub message: VaultMessage,
    pub memo: String,
    /// vault_transaction_create and proposal_create, signed by the proposer.
    pub instructions: Vec<Instruction>,
    pub vault_transaction_rent: u64,
    pub proposal_rent: u64,
    pub fee_buffer_lamports: u64,
    pub required_proposer_lamports: u64,
    pub transaction_size: usize,
    pub simulation: Option<SimulationResult>,
}

#[derive(Clone, Debug)]
pub enum SquadsDecision {
    /// Runway at or above the proposal threshold.
    Healthy,
    /// Due, but one of our proposals is still open.
    Pending,
    Propose(Box<ProposalPlan>),
    /// Due and blocked by a guard.
    Refused(String),
}

#[derive(Clone, Debug)]
pub struct SquadsAssessment {
    pub addresses: SquadsAddresses,
    pub bond_state: BondState,
    pub validator_bond: ValidatorBond,
    pub multisig: Multisig,
    pub vote: VoteAccountInfo,
    pub current_epoch: u64,
    pub pool_last_update_epoch: u64,
    pub pool_total_lamports: u64,
    pub pool_token_supply: u64,
    pub jsol_sol_rate: f64,
    pub bond_jsol: u64,
    pub bond_value_lamports: u64,
    pub floor_lamports: u64,
    pub burn: BurnEstimate,
    pub runway_days: f64,
    pub propose_runway_days: f64,
    pub critical_runway_days: f64,
    pub vote_lamports: u64,
    pub vote_rent_lamports: u64,
    pub vote_withdrawable_lamports: u64,
    pub vault_lamports: u64,
    pub vault_jsol: Option<u64>,
    pub proposer_lamports: u64,
    pub proposer_member: Option<Member>,
    /// Our top-up proposals in the scan window, highest index first.
    pub ours: Vec<OurProposal>,
    pub scanned_from: u64,
    pub pending: Option<OurProposal>,
    pub decision: SquadsDecision,
    /// The proposal a refusal held back, for display.
    pub blocked_proposal: Option<ProposalPlan>,
    pub notes: Vec<String>,
}

impl SquadsAssessment {
    pub fn last_created_at(&self) -> Option<i64> {
        self.ours.first().and_then(|proposal| proposal.created_at)
    }
}

/// Read-only Squads plan: guards, burn estimate, runway, the pending
/// proposal scan, and when a proposal is due, the exact transaction and
/// every pre-check (including a simulation of the vault transaction).
pub fn plan_squads<S: SquadsDataSource>(
    source: &S,
    config: &SquadsConfig,
    now: i64,
) -> Result<SquadsAssessment> {
    config.validate()?;
    let addresses = SquadsAddresses::derive(config);
    let snapshot = source.snapshot(&addresses)?;

    let BondCheck {
        bond_state,
        validator_bond,
        bond_jsol,
        pool,
    } = check_bond_and_pool(
        &config.bond_name,
        &config.vote_account,
        &addresses.bond(),
        &BondAccounts {
            bond_state: snapshot.bond_state.as_ref(),
            validator_bond: snapshot.validator_bond.as_ref(),
            bond_token_account: snapshot.bond_token_account.as_ref(),
            jsol_mint: snapshot.jsol_mint.as_ref(),
            stake_pool: snapshot.stake_pool.as_ref(),
        },
    )?;

    let multisig_account = snapshot
        .multisig
        .as_ref()
        .with_context(|| format!("multisig {} does not exist", addresses.multisig))?;
    let multisig = decode_multisig(&addresses.multisig, multisig_account)?;

    let vote_account = snapshot
        .vote_account
        .as_ref()
        .with_context(|| format!("vote account {} does not exist", addresses.vote_account))?;
    let vote = decode_vote_account(&addresses.vote_account, vote_account)?;
    if vote.authorized_withdrawer != addresses.vault {
        bail!(
            "vote account {} withdrawer is {}, expected Squads vault {} (multisig {}, vault index {})",
            addresses.vote_account,
            vote.authorized_withdrawer,
            addresses.vault,
            addresses.multisig,
            addresses.vault_index
        );
    }
    let vote_rent_lamports = source.rent_exempt_minimum(vote_account.data.len())?;
    let vote_withdrawable_lamports =
        withdrawable_lamports(vote_account.lamports, vote_rent_lamports);

    let vault_lamports = match &snapshot.vault {
        Some(vault) if vault.owner != SYSTEM_PROGRAM_ID || !vault.data.is_empty() => bail!(
            "vault {} is owned by {} with {} data bytes, expected an empty system account",
            addresses.vault,
            vault.owner,
            vault.data.len()
        ),
        Some(vault) => vault.lamports,
        None => 0,
    };
    let vault_jsol = snapshot
        .vault_token_account
        .as_ref()
        .map(|account| {
            validate_token_account(
                "vault JSOL account",
                &addresses.vault_token_account,
                account,
                &addresses.vault,
                &jsol_mint(),
            )
        })
        .transpose()?;

    let first_index = multisig
        .transaction_index
        .saturating_sub(config.squads.pending_scan - 1)
        .max(1);
    let indexes: Vec<u64> = (first_index..=multisig.transaction_index).rev().collect();
    let scanned = source.scan_transactions(&addresses.multisig, &indexes)?;
    let mut ours = classify_proposals(
        &addresses.multisig,
        &multisig,
        &addresses.validator_bond,
        &scanned,
    )?;
    let pending_position = ours.iter().position(OurProposal::is_pending);
    for position in [Some(0), pending_position].into_iter().flatten() {
        if let Some(proposal) = ours.get_mut(position)
            && proposal.created_at.is_none()
        {
            proposal.created_at =
                source
                    .creation_time(&proposal.transaction)?
                    .or(match proposal.status {
                        ProposalStatus::Draft(time) | ProposalStatus::Active(time) => Some(time),
                        _ => None,
                    });
        }
    }
    let pending = pending_position.map(|position| ours[position].clone());

    let window_start = now - i64::from(config.burn_lookback_days) * SECONDS_PER_DAY;
    let history = source.bond_history(&addresses, window_start)?;
    let burn = estimate_burn(
        &history,
        window_start,
        now,
        (config.burn_min_span_days * SECONDS_PER_DAY as f64).ceil() as i64,
        config.burn_fallback_lamports_per_day,
        |jsol| pool.jsol_to_lamports(jsol),
    )?;
    let bond_value_lamports = pool.jsol_to_lamports(bond_jsol)?;
    let runway = runway_days(
        bond_value_lamports,
        config.floor_lamports,
        burn.lamports_per_day,
    );
    let proposer_member = multisig.member(&addresses.proposer).copied();

    let mut assessment = SquadsAssessment {
        addresses,
        bond_state,
        validator_bond,
        multisig,
        vote,
        current_epoch: snapshot.current_epoch,
        pool_last_update_epoch: pool.last_update_epoch(),
        pool_total_lamports: pool.total_lamports(),
        pool_token_supply: pool.pool_token_supply(),
        jsol_sol_rate: pool.rate(),
        bond_jsol,
        bond_value_lamports,
        floor_lamports: config.floor_lamports,
        burn,
        runway_days: runway,
        propose_runway_days: config.propose_runway_days,
        critical_runway_days: config.critical_runway_days,
        vote_lamports: vote_account.lamports,
        vote_rent_lamports,
        vote_withdrawable_lamports,
        vault_lamports,
        vault_jsol,
        proposer_lamports: snapshot.proposer_lamports,
        proposer_member,
        ours,
        scanned_from: first_index,
        pending,
        decision: SquadsDecision::Healthy,
        blocked_proposal: None,
        notes: Vec::new(),
    };
    if !proposer_member.is_some_and(|member| member.can_initiate()) {
        assessment.notes.push(format!(
            "proposer {} is not a multisig member with Initiate permission; a due proposal would be refused",
            assessment.addresses.proposer
        ));
    }
    if runway >= config.propose_runway_days {
        return Ok(assessment);
    }
    if assessment.pending.is_some() {
        assessment.decision = SquadsDecision::Pending;
        return Ok(assessment);
    }

    let a = &assessment;
    let topup_lamports = proposal_lamports(
        a.burn.lamports_per_day,
        config.topup_days,
        config.floor_lamports,
        bond_value_lamports,
    );
    let mut reasons = Vec::new();
    if a.bond_state.status != BondStatus::Active {
        reasons.push(format!(
            "bond_state {} status is {:?}, top-up needs Active",
            a.addresses.bond_state, a.bond_state.status
        ));
    }
    if pool.last_update_epoch() < snapshot.current_epoch {
        reasons.push(format!(
            "stake pool last_update_epoch {} is behind current epoch {}; DepositSol would fail until the pool is updated",
            pool.last_update_epoch(),
            snapshot.current_epoch
        ));
    }
    let cooldown_start = now - TOPUP_COOLDOWN_SECONDS;
    if let Some(executed) = a.ours.iter().find(|proposal| {
        matches!(proposal.status, ProposalStatus::Executed(time) if time >= cooldown_start)
    }) {
        reasons.push(format!(
            "our proposal #{} was executed at unix time {}; at most one top-up per {}h",
            executed.index,
            executed.status.timestamp().unwrap_or_default(),
            TOPUP_COOLDOWN_SECONDS / 3600
        ));
    }
    if let Some(increase) = history.increase_since(cooldown_start) {
        reasons.push(format!(
            "the bond JSOL grew by {} JSOL in {} at unix time {}; at most one top-up per {}h",
            format_base_units(u64::try_from(increase.delta).unwrap_or(u64::MAX)),
            increase.signature,
            increase
                .block_time
                .map(|time| time.to_string())
                .unwrap_or_else(|| "unknown".into()),
            TOPUP_COOLDOWN_SECONDS / 3600
        ));
    }
    if a.vote_withdrawable_lamports < topup_lamports {
        reasons.push(format!(
            "vote account {} withdrawable {} SOL (balance {} - rent-exempt minimum {}) is below the top-up {} SOL",
            a.addresses.vote_account,
            format_sol(a.vote_withdrawable_lamports),
            format_sol(a.vote_lamports),
            format_sol(a.vote_rent_lamports),
            format_sol(topup_lamports)
        ));
    }
    if a.vault_jsol.is_none() {
        let ata_rent = source.rent_exempt_minimum(TOKEN_ACCOUNT_LEN)?;
        let vault_rent = source.rent_exempt_minimum(0)?;
        if a.vault_lamports < ata_rent + vault_rent {
            reasons.push(format!(
                "vault {} balance {} SOL cannot pay the JSOL ATA rent {} SOL and stay rent exempt ({} SOL)",
                a.addresses.vault,
                format_sol(a.vault_lamports),
                format_sol(ata_rent),
                format_sol(vault_rent)
            ));
        }
    }

    let quote = pool.deposit_sol_quote(topup_lamports)?;
    let topup_jsol = u64::try_from(
        u128::from(quote.minted_to_destination)
            * u128::from(10_000 - config.squads.jsol_margin_bps)
            / 10_000,
    )?;
    if topup_jsol == 0 {
        bail!("bond_top_up amount rounds to zero JSOL");
    }
    let vault = a.addresses.vault;
    let vault_instructions = vec![
        vote_withdraw_instruction(&a.addresses.vote_account, &vault, &vault, topup_lamports),
        create_associated_token_account_idempotent(
            &vault,
            &vault,
            &jsol_mint(),
            &token_program_id(),
        ),
        deposit_sol_instruction(&vault, &a.addresses.vault_token_account, topup_lamports),
        bond_top_up_token_instruction(
            a.addresses.bond_state,
            a.addresses.validator_bond,
            vault,
            a.addresses.vault_token_account,
            a.addresses.bond_token_account,
            topup_jsol,
        ),
    ];
    let message = VaultMessage::compile(&vault, &vault_instructions)?;
    let transaction_index = a
        .multisig
        .transaction_index
        .checked_add(1)
        .context("multisig transaction index overflows")?;
    let memo = format!("jpool-bond-rs top-up {} SOL", format_sol(topup_lamports));
    let args = VaultTransactionCreateArgs {
        vault_index: a.addresses.vault_index,
        ephemeral_signers: 0,
        transaction_message: message.encode_transaction_message()?,
        memo: Some(memo.clone()),
    };
    let proposer = a.addresses.proposer;
    let instructions = vec![
        vault_transaction_create_instruction(
            &a.addresses.multisig,
            transaction_index,
            &proposer,
            &proposer,
            &args,
        )?,
        proposal_create_instruction(
            &a.addresses.multisig,
            transaction_index,
            &proposer,
            &proposer,
            false,
        ),
    ];
    let vault_transaction_rent =
        source.rent_exempt_minimum(vault_transaction_account_len(0, &message))?;
    let proposal_rent =
        source.rent_exempt_minimum(proposal_account_len(a.multisig.members.len()))?;
    let fee_buffer = fee_buffer_lamports(config.priority_fee_micro_lamports)?;
    let required_proposer_lamports = vault_transaction_rent
        .checked_add(proposal_rent)
        .and_then(|amount| amount.checked_add(fee_buffer))
        .and_then(|amount| amount.checked_add(config.squads.proposer_reserve_lamports))
        .context("required proposer balance overflows")?;
    let size = transaction_size(&proposer, &instructions);
    if size > MAX_TRANSACTION_SIZE {
        reasons.push(format!(
            "proposal transaction is {size} bytes, above the {MAX_TRANSACTION_SIZE}-byte limit"
        ));
    }
    let vault_side_ok = reasons.is_empty();

    match a.proposer_member {
        None => reasons.push(format!(
            "proposer {proposer} is not a member of multisig {}; the owner must add it with Initiate permission",
            a.addresses.multisig
        )),
        Some(member) if !member.can_initiate() => reasons.push(format!(
            "proposer {proposer} is a member of multisig {} with permissions {}, but needs Initiate",
            a.addresses.multisig,
            member.describe_permissions()
        )),
        Some(_) => {}
    }
    if a.proposer_lamports < required_proposer_lamports {
        reasons.push(format!(
            "proposer {proposer} balance {} SOL is below vault transaction rent {} + proposal rent {} + fee buffer {} + reserve {} = {} SOL",
            format_sol(a.proposer_lamports),
            format_sol(vault_transaction_rent),
            format_sol(proposal_rent),
            format_sol(fee_buffer),
            format_sol(config.squads.proposer_reserve_lamports),
            format_sol(required_proposer_lamports)
        ));
    }

    // Execution happens later and cannot be simulated through Squads, so
    // run the same instructions now as the vault would, without signatures.
    let simulation = if vault_side_ok {
        let result = source.simulate_vault_transaction(&vault, &vault_instructions)?;
        if let Some(error) = &result.error {
            let logs = if result.logs.is_empty() {
                String::new()
            } else {
                format!("\n{}", result.logs.join("\n"))
            };
            reasons.push(format!(
                "vault transaction simulation failed: {error}{logs}"
            ));
        }
        Some(result)
    } else {
        None
    };

    let plan = ProposalPlan {
        transaction_index,
        transaction: transaction_address(&a.addresses.multisig, transaction_index),
        proposal: proposal_address(&a.addresses.multisig, transaction_index),
        topup_lamports,
        quote,
        jsol_margin_bps: config.squads.jsol_margin_bps,
        topup_jsol,
        vault_instructions,
        message,
        memo,
        instructions,
        vault_transaction_rent,
        proposal_rent,
        fee_buffer_lamports: fee_buffer,
        required_proposer_lamports,
        transaction_size: size,
        simulation,
    };
    if reasons.is_empty() {
        assessment.decision = SquadsDecision::Propose(Box::new(plan));
    } else {
        assessment.decision = SquadsDecision::Refused(reasons.join("; "));
        assessment.blocked_proposal = Some(plan);
    }
    Ok(assessment)
}

fn instruction_lines(lines: &mut Vec<String>, plan: &ProposalPlan, a: &SquadsAssessment) {
    let x = format_sol(plan.topup_lamports);
    let summaries = [
        format!(
            "Vote Withdraw {x} SOL from vote account {} to vault {} (withdraw authority: vault)",
            a.addresses.vote_account, a.addresses.vault
        ),
        format!(
            "ATA CreateIdempotent of vault JSOL account {} (payer vault, owner vault, mint {})",
            a.addresses.vault_token_account,
            jsol_mint()
        ),
        format!(
            "JPool DepositSol {x} SOL from vault into vault JSOL account (referral = vault JSOL account): {} JSOL at the current rate (deposit fee {} JSOL, referral {} back)",
            format_base_units(plan.quote.minted_to_destination),
            format_base_units(plan.quote.manager_fee + plan.quote.referral_fee),
            format_base_units(plan.quote.referral_fee)
        ),
        format!(
            "bond_top_up {} JSOL (minted {} minus {} bps margin) from vault JSOL account to bond JSOL account {}",
            format_base_units(plan.topup_jsol),
            format_base_units(plan.quote.minted_to_destination),
            plan.jsol_margin_bps,
            a.addresses.bond_token_account
        ),
    ];
    lines.push(format!(
        "vault transaction:  #{} at {}, proposal {}, memo {:?}",
        plan.transaction_index, plan.transaction, plan.proposal, plan.memo
    ));
    for (number, (summary, instruction)) in
        summaries.iter().zip(&plan.vault_instructions).enumerate()
    {
        lines.push(format!("  {}. {summary}", number + 1));
        lines.push(format!(
            "     program {} data {}",
            instruction.program_id,
            hex(&instruction.data)
        ));
        let accounts: Vec<String> = instruction
            .accounts
            .iter()
            .map(|meta| {
                format!(
                    "{}{}{}",
                    meta.pubkey,
                    if meta.is_writable { " (w)" } else { "" },
                    if meta.is_signer { " (s)" } else { "" }
                )
            })
            .collect();
        lines.push(format!("     accounts {}", accounts.join(", ")));
    }
    lines.push(format!(
        "proposer guard:     needs {} SOL (vault tx rent {}, proposal rent {}, fee buffer {}, reserve), has {} SOL",
        format_sol(plan.required_proposer_lamports),
        format_sol(plan.vault_transaction_rent),
        format_sol(plan.proposal_rent),
        format_sol(plan.fee_buffer_lamports),
        format_sol(a.proposer_lamports)
    ));
    lines.push(format!(
        "proposal tx:        vault_transaction_create + proposal_create, {} bytes, signed by proposer {}",
        plan.transaction_size, a.addresses.proposer
    ));
    lines.push(match &plan.simulation {
        Some(SimulationResult {
            error: None,
            units_consumed,
            ..
        }) => format!(
            "vault simulation:   ok ({} compute units)",
            units_consumed
                .map(|units| units.to_string())
                .unwrap_or_else(|| "?".into())
        ),
        Some(SimulationResult {
            error: Some(error), ..
        }) => format!("vault simulation:   failed: {error}"),
        None => "vault simulation:   not run (blocked by an earlier guard)".into(),
    });
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn format_days(days: f64) -> String {
    if days.is_infinite() {
        "infinite".into()
    } else {
        format!("{days:.1}")
    }
}

pub fn render_squads_assessment(assessment: &SquadsAssessment, now: i64) -> String {
    let a = assessment;
    let burn_source = match &a.burn.source {
        BurnSource::Observed => "observed".to_string(),
        BurnSource::Fallback(reason) => format!("fallback rate, {reason}"),
    };
    let mut lines = vec![
        format!("vote account:       {}", a.addresses.vote_account),
        format!(
            "bond_state:         {} (name {:?}, collateral {}, status {:?})",
            a.addresses.bond_state,
            a.bond_state.name,
            a.bond_state.collateral_type,
            a.bond_state.status
        ),
        format!(
            "validator_bond:     {} (identity {})",
            a.addresses.validator_bond, a.validator_bond.identity
        ),
        format!("bond JSOL account:  {}", a.addresses.bond_token_account),
        format!(
            "JSOL rate:          {:.9} SOL/JSOL (pool {} lamports / {} JSOL units, updated epoch {}, current epoch {})",
            a.jsol_sol_rate,
            a.pool_total_lamports,
            a.pool_token_supply,
            a.pool_last_update_epoch,
            a.current_epoch
        ),
        format!(
            "bond:               {} JSOL = {} SOL (floor {} SOL)",
            format_base_units(a.bond_jsol),
            format_sol(a.bond_value_lamports),
            format_sol(a.floor_lamports)
        ),
        format!(
            "burn:               {:.6} SOL/day ({} claims, {} JSOL out over {:.2} days; {burn_source})",
            a.burn.lamports_per_day / LAMPORTS_PER_SOL as f64,
            a.burn.claims,
            format_base_units(a.burn.outflow_jsol),
            a.burn.span_days()
        ),
        format!(
            "runway:             {} days to the floor (propose below {} days, critical below {} days)",
            format_days(a.runway_days),
            a.propose_runway_days,
            a.critical_runway_days
        ),
        format!(
            "multisig:           {} (threshold {} of {}, time lock {}s, transaction index {}, stale index {})",
            a.addresses.multisig,
            a.multisig.threshold,
            a.multisig.members.len(),
            a.multisig.time_lock,
            a.multisig.transaction_index,
            a.multisig.stale_transaction_index
        ),
        format!(
            "vault:              {} (index {}) balance {} SOL, JSOL account {} ({})",
            a.addresses.vault,
            a.addresses.vault_index,
            format_sol(a.vault_lamports),
            a.addresses.vault_token_account,
            a.vault_jsol
                .map(|jsol| format!("{} JSOL", format_base_units(jsol)))
                .unwrap_or_else(|| "missing, the proposal creates it".into())
        ),
        format!(
            "vote withdrawable:  {} SOL (balance {} - rent-exempt minimum {}; withdrawer {})",
            format_sol(a.vote_withdrawable_lamports),
            format_sol(a.vote_lamports),
            format_sol(a.vote_rent_lamports),
            a.vote.authorized_withdrawer
        ),
        format!(
            "proposer:           {} balance {} SOL, {}",
            a.addresses.proposer,
            format_sol(a.proposer_lamports),
            match &a.proposer_member {
                Some(member) => format!("member ({})", member.describe_permissions()),
                None => "not a member".into(),
            }
        ),
    ];
    let scan = format!(
        "indexes {}..={}",
        a.scanned_from, a.multisig.transaction_index
    );
    lines.push(match (&a.pending, a.ours.first()) {
        (Some(pending), _) => format!(
            "proposals:          ours pending: #{} {} ({}), {} SOL, age {} ({scan})",
            pending.index,
            pending.status.name(),
            pending.transaction,
            pending
                .amount_lamports
                .map(format_sol)
                .unwrap_or_else(|| "?".into()),
            pending
                .created_at
                .map(|time| format!("{}s", now - time))
                .unwrap_or_else(|| "unknown".into())
        ),
        (None, Some(last)) => format!(
            "proposals:          none of ours pending; latest ours #{} {} ({scan})",
            last.index,
            last.status.name()
        ),
        (None, None) => format!("proposals:          none of ours ({scan})"),
    });
    for note in &a.notes {
        lines.push(format!("note:               {note}"));
    }
    let due = |lines: &mut Vec<String>, plan: Option<&ProposalPlan>| {
        lines.push(format!(
            "decision:           propose {} (runway {} days < {} days)",
            plan.map(|plan| format!("{} SOL", format_sol(plan.topup_lamports)))
                .unwrap_or_else(|| "a top-up".into()),
            format_days(a.runway_days),
            a.propose_runway_days
        ));
    };
    match &a.decision {
        SquadsDecision::Healthy => lines.push(format!(
            "decision:           no proposal (runway {} days >= {} days)",
            format_days(a.runway_days),
            a.propose_runway_days
        )),
        SquadsDecision::Pending => lines.push(format!(
            "decision:           no new proposal, #{} is still open (runway {} days < {} days)",
            a.pending.as_ref().map(|p| p.index).unwrap_or_default(),
            format_days(a.runway_days),
            a.propose_runway_days
        )),
        SquadsDecision::Propose(plan) => {
            due(&mut lines, Some(plan));
            instruction_lines(&mut lines, plan, a);
        }
        SquadsDecision::Refused(reason) => {
            due(&mut lines, a.blocked_proposal.as_ref());
            if let Some(plan) = &a.blocked_proposal {
                instruction_lines(&mut lines, plan, a);
            }
            lines.push(format!("refused:            {reason}"));
        }
    }
    if a.runway_days < a.critical_runway_days {
        lines.push(format!(
            "critical:           runway {} days is below {} days",
            format_days(a.runway_days),
            a.critical_runway_days
        ));
    }
    lines.join("\n")
}

/// Checks a freshly created proposal and vault transaction against the plan.
pub fn check_created_proposal(
    addresses: &SquadsAddresses,
    plan: &ProposalPlan,
    proposal: &ChainAccount,
    transaction: &ChainAccount,
) -> Result<()> {
    let decoded = decode_proposal(&plan.proposal, proposal)?;
    if decoded.multisig != addresses.multisig || decoded.transaction_index != plan.transaction_index
    {
        bail!(
            "proposal {} belongs to multisig {} index {}, expected {} index {}",
            plan.proposal,
            decoded.multisig,
            decoded.transaction_index,
            addresses.multisig,
            plan.transaction_index
        );
    }
    if !matches!(decoded.status, ProposalStatus::Active(_)) {
        bail!(
            "proposal {} status is {}, expected Active",
            plan.proposal,
            decoded.status.name()
        );
    }
    let stored = decode_vault_transaction(&plan.transaction, transaction)?;
    if stored.multisig != addresses.multisig
        || stored.index != plan.transaction_index
        || stored.creator != addresses.proposer
        || stored.vault_index != addresses.vault_index
        || !stored.ephemeral_signer_bumps.is_empty()
    {
        bail!(
            "vault transaction {} has multisig {}, index {}, creator {}, vault index {}; expected {}, {}, {}, {}",
            plan.transaction,
            stored.multisig,
            stored.index,
            stored.creator,
            stored.vault_index,
            addresses.multisig,
            plan.transaction_index,
            addresses.proposer,
            addresses.vault_index
        );
    }
    if stored.message != plan.message {
        bail!(
            "vault transaction {} message differs from the proposed one",
            plan.transaction
        );
    }
    let decoded = stored.message.instructions()?;
    let same = decoded.len() == plan.vault_instructions.len()
        && decoded
            .iter()
            .zip(&plan.vault_instructions)
            .all(|(stored, intended)| {
                stored.program_id == intended.program_id
                    && stored.data == intended.data
                    && stored
                        .accounts
                        .iter()
                        .map(|meta| meta.pubkey)
                        .eq(intended.accounts.iter().map(|meta| meta.pubkey))
            });
    if !same {
        bail!(
            "vault transaction {} does not decode to the {} intended instructions",
            plan.transaction,
            plan.vault_instructions.len()
        );
    }
    Ok(())
}

/// Polls until the proposal and vault transaction exist, then checks them.
pub fn verify_proposal(
    client: &RpcClient,
    addresses: &SquadsAddresses,
    plan: &ProposalPlan,
) -> Result<()> {
    let keys = [plan.proposal, plan.transaction];
    for poll in 0..VERIFY_POLLS {
        let accounts = client
            .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
            .context("failed to re-read the proposal")?
            .value;
        if let [Some(proposal), Some(transaction)] = accounts.as_slice() {
            return check_created_proposal(
                addresses,
                plan,
                &chain_account!(Some(proposal)).expect("present"),
                &chain_account!(Some(transaction)).expect("present"),
            );
        }
        if poll + 1 < VERIFY_POLLS {
            thread::sleep(VERIFY_POLL_INTERVAL);
        }
    }
    bail!(
        "proposal {} or vault transaction {} not found after confirmation",
        plan.proposal,
        plan.transaction
    )
}

#[cfg(test)]
#[path = "squads_mode_tests.rs"]
mod tests;
