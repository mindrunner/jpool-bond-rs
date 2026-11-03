pub mod bond;
pub mod config;
pub mod history;
pub mod metrics;
pub mod pool;
pub mod token;
pub mod transaction;
pub mod withdraw;

use anyhow::{Context, Result, bail};
use bond::{
    BondState, BondStatus, BondType, CollateralType, ValidatorBond, bond_top_up_token_instruction,
    decode_bond_state, decode_validator_bond, find_bond_state_address, find_validator_bond_address,
};
use config::{Config, format_base_units, format_sol};
use history::RecentTopUp;
use pool::{
    DepositQuote, JPOOL_STAKE_POOL, JSOL_DECIMALS, decode_jpool, deposit_sol_instruction, jsol_mint,
};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent;
use std::{thread, time::Duration};
use token::{associated_token_address, token_program_id, validate_mint, validate_token_account};

/// At most one top-up per this window, enforced from chain history.
pub const TOPUP_COOLDOWN_SECONDS: i64 = 20 * 60 * 60;
/// Submission attempts; a retry needs proof the previous one expired unseen.
pub const MAX_ATTEMPTS: usize = 3;
const MAX_COMPUTE_UNIT_LIMIT: u64 = 1_400_000;
const SIGNATURE_FEE_LAMPORTS: u64 = 5_000;
const VERIFY_POLLS: usize = 30;
const VERIFY_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainAccount {
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
}

/// Every account this tool touches, derived from the config alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addresses {
    pub vote_account: Pubkey,
    pub identity: Pubkey,
    pub bond_state: Pubkey,
    pub validator_bond: Pubkey,
    pub bond_token_account: Pubkey,
    pub identity_token_account: Pubkey,
}

impl Addresses {
    pub fn derive(config: &Config) -> Self {
        let bond_state = find_bond_state_address(&config.bond_name);
        let validator_bond = find_validator_bond_address(&bond_state, &config.vote_account);
        Self {
            vote_account: config.vote_account,
            identity: config.identity_pubkey,
            bond_state,
            validator_bond,
            bond_token_account: associated_token_address(&validator_bond, &jsol_mint()),
            identity_token_account: associated_token_address(&config.identity_pubkey, &jsol_mint()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChainSnapshot {
    pub current_epoch: u64,
    pub bond_state: Option<ChainAccount>,
    pub validator_bond: Option<ChainAccount>,
    pub bond_token_account: Option<ChainAccount>,
    pub stake_pool: Option<ChainAccount>,
    pub jsol_mint: Option<ChainAccount>,
    pub identity_token_account: Option<ChainAccount>,
    pub identity_lamports: u64,
    /// Rent for a token account; only fetched when the identity ATA is missing.
    pub token_account_rent_lamports: Option<u64>,
}

pub trait ChainDataSource {
    fn snapshot(&self, addresses: &Addresses) -> Result<ChainSnapshot>;
    fn recent_identity_topup(
        &self,
        addresses: &Addresses,
        since_unix: i64,
    ) -> Result<Option<RecentTopUp>>;
}

pub struct RpcChainDataSource<'a> {
    pub client: &'a RpcClient,
}

impl ChainDataSource for RpcChainDataSource<'_> {
    fn snapshot(&self, addresses: &Addresses) -> Result<ChainSnapshot> {
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
            addresses.identity_token_account,
        ];
        let accounts = self
            .client
            .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
            .context("failed to fetch bond and stake pool accounts")?
            .value;
        if accounts.len() != keys.len() {
            bail!(
                "RPC returned {} accounts for {} requested",
                accounts.len(),
                keys.len()
            );
        }
        let account = |index: usize| {
            accounts[index].as_ref().map(|account| ChainAccount {
                owner: account.owner,
                lamports: account.lamports,
                data: account.data.clone(),
            })
        };
        let identity_lamports = self
            .client
            .get_balance_with_commitment(&addresses.identity, CommitmentConfig::confirmed())
            .context("failed to fetch identity balance")?
            .value;
        let token_account_rent_lamports = if accounts[5].is_none() {
            Some(
                self.client
                    .get_minimum_balance_for_rent_exemption(token::TOKEN_ACCOUNT_LEN)
                    .context("failed to fetch token account rent")?,
            )
        } else {
            None
        };
        Ok(ChainSnapshot {
            current_epoch,
            bond_state: account(0),
            validator_bond: account(1),
            bond_token_account: account(2),
            stake_pool: account(3),
            jsol_mint: account(4),
            identity_token_account: account(5),
            identity_lamports,
            token_account_rent_lamports,
        })
    }

    fn recent_identity_topup(
        &self,
        addresses: &Addresses,
        since_unix: i64,
    ) -> Result<Option<RecentTopUp>> {
        history::find_recent_identity_topup(self.client, addresses, since_unix)
    }
}

#[derive(Clone, Debug)]
pub struct TopUpPlan {
    pub quote: DepositQuote,
    /// JSOL passed to bond_top_up: exactly what the deposit credits.
    pub topup_jsol: u64,
    pub required_identity_lamports: u64,
    pub fee_buffer_lamports: u64,
    pub ata_rent_lamports: u64,
    pub instructions: Vec<Instruction>,
}

#[derive(Clone, Debug)]
pub enum Decision {
    NoTopUp,
    TopUp(TopUpPlan),
    Refused(String),
}

#[derive(Clone, Debug)]
pub struct Assessment {
    pub addresses: Addresses,
    pub bond_state: BondState,
    pub validator_bond: ValidatorBond,
    pub current_epoch: u64,
    pub pool_last_update_epoch: u64,
    pub pool_total_lamports: u64,
    pub pool_token_supply: u64,
    pub jsol_sol_rate: f64,
    pub bond_jsol: u64,
    pub bond_value_lamports: u64,
    pub min_value_lamports: u64,
    pub topup_lamports: u64,
    pub identity_lamports: u64,
    pub identity_jsol: Option<u64>,
    pub decision: Decision,
    /// The top-up a cooldown refusal held back, for display only.
    pub blocked_topup: Option<TopUpPlan>,
}

pub fn fee_buffer_lamports(priority_fee_micro_lamports: u64) -> Result<u64> {
    let priority = u64::try_from(
        (priority_fee_micro_lamports as u128)
            .checked_mul(MAX_COMPUTE_UNIT_LIMIT as u128)
            .ok_or_else(|| anyhow::anyhow!("priority fee overflow"))?
            .div_ceil(1_000_000),
    )?;
    let per_attempt = priority
        .checked_add(SIGNATURE_FEE_LAMPORTS)
        .ok_or_else(|| anyhow::anyhow!("fee buffer overflow"))?;
    per_attempt
        .checked_mul(MAX_ATTEMPTS as u64)
        .ok_or_else(|| anyhow::anyhow!("fee buffer overflow"))
}

/// Pure guard and decision logic over one chain snapshot.
pub fn assess(
    config: &Config,
    addresses: &Addresses,
    snapshot: &ChainSnapshot,
) -> Result<Assessment> {
    config.validate()?;
    if *addresses != Addresses::derive(config) {
        bail!("account addresses do not match the config");
    }
    let bond_state_account = snapshot
        .bond_state
        .as_ref()
        .with_context(|| format!("bond_state {} does not exist", addresses.bond_state))?;
    let bond_state = decode_bond_state(&addresses.bond_state, bond_state_account)?;
    if bond_state.name != config.bond_name {
        bail!(
            "bond_state {} is named {:?}, expected {:?}",
            addresses.bond_state,
            bond_state.name,
            config.bond_name
        );
    }
    if bond_state.bond_type != BondType::Standard {
        bail!(
            "bond_state {} has bond type {:?}, expected Standard",
            addresses.bond_state,
            bond_state.bond_type
        );
    }
    if bond_state.collateral_type != CollateralType::Token(jsol_mint()) {
        bail!(
            "bond_state {} collateral is {}, expected Token({})",
            addresses.bond_state,
            bond_state.collateral_type,
            jsol_mint()
        );
    }

    let validator_bond_account = snapshot.validator_bond.as_ref().with_context(|| {
        format!(
            "validator_bond {} does not exist (vote {} not registered in bond {:?})",
            addresses.validator_bond, config.vote_account, config.bond_name
        )
    })?;
    let validator_bond = decode_validator_bond(&addresses.validator_bond, validator_bond_account)?;
    if validator_bond.vote_account != config.vote_account {
        bail!(
            "validator_bond {} vote account is {}, expected {}",
            addresses.validator_bond,
            validator_bond.vote_account,
            config.vote_account
        );
    }
    if validator_bond.state != addresses.bond_state {
        bail!(
            "validator_bond {} belongs to bond_state {}, expected {}",
            addresses.validator_bond,
            validator_bond.state,
            addresses.bond_state
        );
    }

    let bond_token_account = snapshot.bond_token_account.as_ref().with_context(|| {
        format!(
            "bond JSOL account {} does not exist",
            addresses.bond_token_account
        )
    })?;
    let bond_jsol = validate_token_account(
        "bond JSOL account",
        &addresses.bond_token_account,
        bond_token_account,
        &addresses.validator_bond,
        &jsol_mint(),
    )?;

    let mint_account = snapshot
        .jsol_mint
        .as_ref()
        .with_context(|| format!("JSOL mint {} does not exist", jsol_mint()))?;
    validate_mint(&jsol_mint(), mint_account, JSOL_DECIMALS)?;
    let pool_account = snapshot
        .stake_pool
        .as_ref()
        .with_context(|| format!("stake pool {JPOOL_STAKE_POOL} does not exist"))?;
    let pool = decode_jpool(&JPOOL_STAKE_POOL, pool_account)?;

    let identity_jsol = snapshot
        .identity_token_account
        .as_ref()
        .map(|account| {
            validate_token_account(
                "identity JSOL account",
                &addresses.identity_token_account,
                account,
                &addresses.identity,
                &jsol_mint(),
            )
        })
        .transpose()?;

    let bond_value_lamports = pool.jsol_to_lamports(bond_jsol)?;
    let mut assessment = Assessment {
        addresses: *addresses,
        bond_state,
        validator_bond,
        current_epoch: snapshot.current_epoch,
        pool_last_update_epoch: pool.last_update_epoch(),
        pool_total_lamports: pool.total_lamports(),
        pool_token_supply: pool.pool_token_supply(),
        jsol_sol_rate: pool.rate(),
        bond_jsol,
        bond_value_lamports,
        min_value_lamports: config.min_value_lamports,
        topup_lamports: config.topup_lamports,
        identity_lamports: snapshot.identity_lamports,
        identity_jsol,
        decision: Decision::NoTopUp,
        blocked_topup: None,
    };
    if bond_value_lamports >= config.min_value_lamports {
        return Ok(assessment);
    }

    if assessment.bond_state.status != BondStatus::Active {
        assessment.decision = Decision::Refused(format!(
            "bond_state {} status is {:?}, top-up needs Active",
            addresses.bond_state, assessment.bond_state.status
        ));
        return Ok(assessment);
    }
    if pool.last_update_epoch() < snapshot.current_epoch {
        assessment.decision = Decision::Refused(format!(
            "stake pool last_update_epoch {} is behind current epoch {}; DepositSol would fail until the pool is updated",
            pool.last_update_epoch(),
            snapshot.current_epoch
        ));
        return Ok(assessment);
    }
    let quote = pool.deposit_sol_quote(config.topup_lamports)?;
    let ata_rent_lamports = if snapshot.identity_token_account.is_some() {
        0
    } else {
        snapshot
            .token_account_rent_lamports
            .context("identity JSOL account is missing and no rent estimate was fetched")?
    };
    let fee_buffer = fee_buffer_lamports(config.priority_fee_micro_lamports)?;
    let required = config
        .topup_lamports
        .checked_add(config.identity_reserve_lamports)
        .and_then(|amount| amount.checked_add(fee_buffer))
        .and_then(|amount| amount.checked_add(ata_rent_lamports))
        .ok_or_else(|| anyhow::anyhow!("required identity balance overflows"))?;
    if snapshot.identity_lamports < required {
        assessment.decision = Decision::Refused(format!(
            "identity {} balance {} SOL is below top-up {} + reserve {} + fee buffer {} + ATA rent {} = {} SOL",
            addresses.identity,
            format_sol(snapshot.identity_lamports),
            format_sol(config.topup_lamports),
            format_sol(config.identity_reserve_lamports),
            format_sol(fee_buffer),
            format_sol(ata_rent_lamports),
            format_sol(required)
        ));
        return Ok(assessment);
    }

    let instructions = vec![
        create_associated_token_account_idempotent(
            &addresses.identity,
            &addresses.identity,
            &jsol_mint(),
            &token_program_id(),
        ),
        deposit_sol_instruction(
            &addresses.identity,
            &addresses.identity_token_account,
            config.topup_lamports,
        ),
        bond_top_up_token_instruction(
            addresses.bond_state,
            addresses.validator_bond,
            addresses.identity,
            addresses.identity_token_account,
            addresses.bond_token_account,
            quote.minted_to_destination,
        ),
    ];
    assessment.decision = Decision::TopUp(TopUpPlan {
        quote,
        topup_jsol: quote.minted_to_destination,
        required_identity_lamports: required,
        fee_buffer_lamports: fee_buffer,
        ata_rent_lamports,
        instructions,
    });
    Ok(assessment)
}

/// Full read-only plan: snapshot, guards, decision, and the chain-history
/// cooldown when a top-up would otherwise go ahead.
pub fn plan<S: ChainDataSource>(source: &S, config: &Config, now_unix: i64) -> Result<Assessment> {
    config.validate()?;
    let addresses = Addresses::derive(config);
    let snapshot = source.snapshot(&addresses)?;
    let mut assessment = assess(config, &addresses, &snapshot)?;
    if matches!(assessment.decision, Decision::TopUp(_)) {
        let since = now_unix - TOPUP_COOLDOWN_SECONDS;
        if let Some(recent) = source.recent_identity_topup(&addresses, since)? {
            let reason = format!(
                "identity {} already topped up the bond in {} at unix time {} ({}s ago); at most one top-up per {}h",
                addresses.identity,
                recent.signature,
                recent
                    .block_time
                    .map(|time| time.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                recent
                    .block_time
                    .map(|time| (now_unix - time).to_string())
                    .unwrap_or_else(|| "?".into()),
                TOPUP_COOLDOWN_SECONDS / 3600
            );
            let blocked = std::mem::replace(&mut assessment.decision, Decision::Refused(reason));
            if let Decision::TopUp(plan) = blocked {
                assessment.blocked_topup = Some(plan);
            }
        }
    }
    Ok(assessment)
}

fn render_topup(lines: &mut Vec<String>, plan: &TopUpPlan, identity_lamports: u64) {
    lines.push(format!(
        "top-up:             DepositSol {} SOL -> {} JSOL to identity (deposit fee {} JSOL, of which referral {} back to identity) -> bond_top_up {} JSOL",
        format_sol(plan.quote.lamports),
        format_base_units(plan.quote.minted_to_destination),
        format_base_units(plan.quote.manager_fee + plan.quote.referral_fee),
        format_base_units(plan.quote.referral_fee),
        format_base_units(plan.topup_jsol)
    ));
    lines.push(format!(
        "identity guard:     needs {} SOL (fee buffer {} SOL, ATA rent {} SOL), has {} SOL",
        format_sol(plan.required_identity_lamports),
        format_sol(plan.fee_buffer_lamports),
        format_sol(plan.ata_rent_lamports),
        format_sol(identity_lamports)
    ));
}

pub fn render_assessment(assessment: &Assessment) -> String {
    let a = assessment;
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
            format_sol(a.min_value_lamports)
        ),
        format!(
            "identity:           {} balance {} SOL, JSOL account {} ({})",
            a.addresses.identity,
            format_sol(a.identity_lamports),
            a.addresses.identity_token_account,
            a.identity_jsol
                .map(|jsol| format!("{} JSOL", format_base_units(jsol)))
                .unwrap_or_else(|| "missing, will be created".into())
        ),
    ];
    let topup_due = |lines: &mut Vec<String>| {
        lines.push(format!(
            "decision:           top-up {} SOL (value {} SOL < floor {} SOL)",
            format_sol(a.topup_lamports),
            format_sol(a.bond_value_lamports),
            format_sol(a.min_value_lamports)
        ))
    };
    match &a.decision {
        Decision::NoTopUp => lines.push(format!(
            "decision:           no top-up (value {} SOL >= floor {} SOL)",
            format_sol(a.bond_value_lamports),
            format_sol(a.min_value_lamports)
        )),
        Decision::TopUp(plan) => {
            topup_due(&mut lines);
            render_topup(&mut lines, plan, a.identity_lamports);
        }
        Decision::Refused(reason) => {
            topup_due(&mut lines);
            if let Some(plan) = &a.blocked_topup {
                render_topup(&mut lines, plan, a.identity_lamports);
            }
            lines.push(format!("refused:            {reason}"));
        }
    }
    lines.join("\n")
}

/// Polls the bond JSOL account until it moves, then requires the increase
/// to equal the topped-up amount exactly.
pub fn verify_topup(
    client: &RpcClient,
    addresses: &Addresses,
    before_jsol: u64,
    topup_jsol: u64,
) -> Result<u64> {
    let expected = before_jsol
        .checked_add(topup_jsol)
        .ok_or_else(|| anyhow::anyhow!("bond JSOL overflow"))?;
    let mut last = before_jsol;
    for poll in 0..VERIFY_POLLS {
        let account = client
            .get_account_with_commitment(
                &addresses.bond_token_account,
                CommitmentConfig::confirmed(),
            )
            .context("failed to re-read bond JSOL account")?
            .value
            .with_context(|| {
                format!(
                    "bond JSOL account {} disappeared",
                    addresses.bond_token_account
                )
            })?;
        last = validate_token_account(
            "bond JSOL account",
            &addresses.bond_token_account,
            &ChainAccount {
                owner: account.owner,
                lamports: account.lamports,
                data: account.data,
            },
            &addresses.validator_bond,
            &jsol_mint(),
        )?;
        if last != before_jsol {
            break;
        }
        if poll + 1 < VERIFY_POLLS {
            thread::sleep(VERIFY_POLL_INTERVAL);
        }
    }
    check_topup_increase(before_jsol, last, topup_jsol)?;
    debug_assert_eq!(last, expected);
    Ok(last)
}

pub fn check_topup_increase(before: u64, after: u64, topup: u64) -> Result<()> {
    if after.checked_sub(before) != Some(topup) {
        bail!(
            "bond JSOL went from {} to {} after the confirmed top-up; expected an increase of exactly {}",
            format_base_units(before),
            format_base_units(after),
            format_base_units(topup)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
