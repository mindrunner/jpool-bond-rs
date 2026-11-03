//! Manual bond withdrawal: JSOL out of the validator bond to the identity,
//! then WithdrawSol so the SOL lands on `destination` in the same
//! transaction. Signed by the validator identity, which the bond program
//! accepts as withdrawal authority while none is set on the validator bond.

use crate::{
    Addresses, ChainSnapshot,
    bond::{bond_withdraw_token_instruction, decode_bond_state, decode_validator_bond},
    pool::{JPOOL_STAKE_POOL, decode_jpool, jsol_mint, withdraw_sol_instruction},
    token::{associated_token_address, token_program_id, validate_token_account},
};
use anyhow::{Context, Result, bail};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent;

#[derive(Clone, Debug)]
pub struct WithdrawPlan {
    pub identity: Pubkey,
    pub destination: Pubkey,
    pub bond_jsol_before: u64,
    pub withdraw_jsol: u64,
    /// SOL value of the withdrawn JSOL at the pool rate (before pool fees).
    pub withdraw_value_lamports: u64,
    pub bond_value_after_lamports: u64,
    pub instructions: Vec<Instruction>,
}

/// Builds the withdrawal of `lamports` worth of JSOL, rounded down to whole
/// JSOL base units, from the bond in `addresses`.
pub fn plan_withdraw(
    addresses: &Addresses,
    snapshot: &ChainSnapshot,
    identity: Pubkey,
    lamports: u64,
    destination: Pubkey,
) -> Result<WithdrawPlan> {
    let bond_state_account = snapshot
        .bond_state
        .as_ref()
        .with_context(|| format!("bond state {} does not exist", addresses.bond_state))?;
    decode_bond_state(&addresses.bond_state, bond_state_account)?;
    let validator_bond_account = snapshot
        .validator_bond
        .as_ref()
        .with_context(|| format!("validator bond {} does not exist", addresses.validator_bond))?;
    let validator_bond = decode_validator_bond(&addresses.validator_bond, validator_bond_account)?;
    if validator_bond.vote_account != addresses.vote_account {
        bail!(
            "validator bond {} belongs to vote account {}, not {}",
            addresses.validator_bond,
            validator_bond.vote_account,
            addresses.vote_account
        );
    }
    if validator_bond.identity != identity {
        bail!(
            "validator bond identity is {}, the keypair is {identity}",
            validator_bond.identity
        );
    }
    if let Some(authority) = validator_bond.withdrawal_authority {
        bail!(
            "validator bond has withdrawal authority {authority}; this command signs as the identity only"
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
    let pool_account = snapshot
        .stake_pool
        .as_ref()
        .with_context(|| format!("stake pool {JPOOL_STAKE_POOL} does not exist"))?;
    let pool = decode_jpool(&JPOOL_STAKE_POOL, pool_account)?;
    let withdraw_jsol = pool.lamports_to_jsol(lamports)?;
    if withdraw_jsol == 0 {
        bail!("amount is below one JSOL base unit");
    }
    if withdraw_jsol > bond_jsol {
        bail!("bond holds {bond_jsol} JSOL base units, cannot withdraw {withdraw_jsol}");
    }
    let identity_token_account = associated_token_address(&identity, &jsol_mint());
    let instructions = vec![
        create_associated_token_account_idempotent(
            &identity,
            &identity,
            &jsol_mint(),
            &token_program_id(),
        ),
        bond_withdraw_token_instruction(
            addresses.bond_state,
            addresses.validator_bond,
            identity,
            identity_token_account,
            addresses.bond_token_account,
            withdraw_jsol,
        ),
        withdraw_sol_instruction(
            &identity,
            &identity_token_account,
            &destination,
            withdraw_jsol,
        ),
    ];
    Ok(WithdrawPlan {
        identity,
        destination,
        bond_jsol_before: bond_jsol,
        withdraw_jsol,
        withdraw_value_lamports: pool.jsol_to_lamports(withdraw_jsol)?,
        bond_value_after_lamports: pool.jsol_to_lamports(bond_jsol - withdraw_jsol)?,
        instructions,
    })
}

/// Parses a positive SOL amount with at most 9 decimals into lamports.
pub fn parse_sol(value: &str) -> Result<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !fraction.chars().all(|c| c.is_ascii_digit())
        || fraction.len() > 9
    {
        bail!("invalid SOL amount {value:?}");
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse()? };
    let fraction: u64 = format!("{fraction:0<9}").parse()?;
    let lamports = whole
        .checked_mul(1_000_000_000)
        .and_then(|lamports| lamports.checked_add(fraction))
        .with_context(|| format!("SOL amount {value} overflows"))?;
    if lamports == 0 {
        bail!("SOL amount must be positive");
    }
    Ok(lamports)
}
