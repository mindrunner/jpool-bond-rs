//! JPool SPL stake pool: guarded decoding, DepositSol math and instruction.

use crate::{ChainAccount, token::token_program_id};
use anyhow::{Result, bail};
use borsh::BorshDeserialize;
use solana_instruction::Instruction;
use solana_pubkey::{Pubkey, pubkey};
use spl_stake_pool::state::{AccountType, StakePool};

pub const JPOOL_STAKE_POOL: Pubkey = pubkey!("CtMyWsrUtAwXWiGr9WjHT5fC3p3fgV8cyGpLTo2LJzG1");
pub const STAKE_POOL_PROGRAM_ID: Pubkey = pubkey!("SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy");
pub const JSOL_MINT: Pubkey = pubkey!("7Q2afV64in6N6SeZsAAB81TJzwDoD6zpqmHkzi9Dcavn");
pub const JPOOL_RESERVE_STAKE: Pubkey = pubkey!("AXu3DTw9AFq9FDTzX4vqA3XiT7LjrS5DpbsZaPpEx6qR");
pub const JPOOL_MANAGER_FEE_ACCOUNT: Pubkey =
    pubkey!("GLysLmE2bwaTNCioDoadMvt9A4RvdFokE4BZwuFoeSn4");
pub const JSOL_DECIMALS: u8 = 9;

pub fn jsol_mint() -> Pubkey {
    JSOL_MINT
}

pub fn withdraw_authority() -> Pubkey {
    spl_stake_pool::find_withdraw_authority_program_address(
        &STAKE_POOL_PROGRAM_ID,
        &JPOOL_STAKE_POOL,
    )
    .0
}

/// The fields of the JPool stake pool this tool relies on, after the guards.
#[derive(Clone, Debug, PartialEq)]
pub struct JPool {
    pub pool: StakePool,
}

impl JPool {
    pub fn total_lamports(&self) -> u64 {
        self.pool.total_lamports
    }

    pub fn pool_token_supply(&self) -> u64 {
        self.pool.pool_token_supply
    }

    pub fn last_update_epoch(&self) -> u64 {
        self.pool.last_update_epoch
    }

    /// SOL value of `jsol` base units at the pool rate, rounded down.
    pub fn jsol_to_lamports(&self, jsol: u64) -> Result<u64> {
        let value = (jsol as u128)
            .checked_mul(self.pool.total_lamports as u128)
            .ok_or_else(|| anyhow::anyhow!("JSOL value overflow"))?
            / self.pool.pool_token_supply as u128;
        u64::try_from(value).map_err(|_| anyhow::anyhow!("JSOL value overflow"))
    }

    /// JSOL worth `lamports` at the pool rate, rounded down.
    pub fn lamports_to_jsol(&self, lamports: u64) -> Result<u64> {
        let value = (lamports as u128)
            .checked_mul(self.pool.pool_token_supply as u128)
            .ok_or_else(|| anyhow::anyhow!("JSOL amount overflow"))?
            / self.pool.total_lamports as u128;
        u64::try_from(value).map_err(|_| anyhow::anyhow!("JSOL amount overflow"))
    }

    pub fn rate(&self) -> f64 {
        self.pool.total_lamports as f64 / self.pool.pool_token_supply as f64
    }

    /// JSOL credited to the destination for a DepositSol of `lamports`,
    /// with the referrer account set to the destination. Mirrors
    /// `process_deposit_sol` of the stake-pool program.
    pub fn deposit_sol_quote(&self, lamports: u64) -> Result<DepositQuote> {
        let pool = &self.pool;
        let new_pool_tokens = pool
            .calc_pool_tokens_for_deposit(lamports)
            .ok_or_else(|| anyhow::anyhow!("deposit pool token calculation overflow"))?;
        let sol_deposit_fee = pool
            .calc_pool_tokens_sol_deposit_fee(new_pool_tokens)
            .ok_or_else(|| anyhow::anyhow!("deposit fee calculation overflow"))?;
        let pool_tokens_user = new_pool_tokens
            .checked_sub(sol_deposit_fee)
            .ok_or_else(|| anyhow::anyhow!("deposit fee exceeds minted pool tokens"))?;
        let referral_fee = pool
            .calc_pool_tokens_sol_referral_fee(sol_deposit_fee)
            .ok_or_else(|| anyhow::anyhow!("referral fee calculation overflow"))?;
        let manager_fee = sol_deposit_fee
            .checked_sub(referral_fee)
            .ok_or_else(|| anyhow::anyhow!("referral fee exceeds deposit fee"))?;
        if pool_tokens_user == 0 {
            bail!("a deposit of {lamports} lamports would mint zero JSOL");
        }
        let minted_to_destination = pool_tokens_user
            .checked_add(referral_fee)
            .ok_or_else(|| anyhow::anyhow!("minted JSOL overflow"))?;
        Ok(DepositQuote {
            lamports,
            pool_tokens_user,
            referral_fee,
            manager_fee,
            minted_to_destination,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositQuote {
    pub lamports: u64,
    pub pool_tokens_user: u64,
    pub referral_fee: u64,
    pub manager_fee: u64,
    /// pool_tokens_user + referral_fee, because the referrer is the destination.
    pub minted_to_destination: u64,
}

pub fn decode_jpool(address: &Pubkey, account: &ChainAccount) -> Result<JPool> {
    if *address != JPOOL_STAKE_POOL {
        bail!("stake pool address {address} is not JPool {JPOOL_STAKE_POOL}");
    }
    if account.owner != STAKE_POOL_PROGRAM_ID {
        bail!(
            "stake pool {address} is owned by {}, expected {STAKE_POOL_PROGRAM_ID}",
            account.owner
        );
    }
    // Pool accounts are allocated larger than their borsh payload, so
    // trailing bytes must be tolerated.
    let pool = StakePool::deserialize(&mut account.data.as_slice())
        .map_err(|error| anyhow::anyhow!("invalid stake pool data: {error}"))?;
    if pool.account_type != AccountType::StakePool {
        bail!(
            "stake pool {address} account type is {:?}",
            pool.account_type
        );
    }
    if pool.pool_mint != JSOL_MINT {
        bail!(
            "stake pool {address} mint is {}, expected JSOL {JSOL_MINT}",
            pool.pool_mint
        );
    }
    if pool.token_program_id != token_program_id() {
        bail!(
            "stake pool {address} token program is {}, expected {}",
            pool.token_program_id,
            token_program_id()
        );
    }
    if pool.reserve_stake != JPOOL_RESERVE_STAKE {
        bail!(
            "stake pool {address} reserve is {}, expected {JPOOL_RESERVE_STAKE}",
            pool.reserve_stake
        );
    }
    if pool.manager_fee_account != JPOOL_MANAGER_FEE_ACCOUNT {
        bail!(
            "stake pool {address} manager fee account is {}, expected {JPOOL_MANAGER_FEE_ACCOUNT}",
            pool.manager_fee_account
        );
    }
    let (_, bump) =
        spl_stake_pool::find_withdraw_authority_program_address(&STAKE_POOL_PROGRAM_ID, address);
    if pool.stake_withdraw_bump_seed != bump {
        bail!(
            "stake pool {address} withdraw bump is {}, expected {bump}",
            pool.stake_withdraw_bump_seed
        );
    }
    if pool.total_lamports == 0 || pool.pool_token_supply == 0 {
        bail!(
            "stake pool {address} is empty (total_lamports {}, pool_token_supply {})",
            pool.total_lamports,
            pool.pool_token_supply
        );
    }
    if let Some(authority) = pool.sol_deposit_authority {
        bail!("stake pool {address} requires SOL deposit authority {authority}");
    }
    Ok(JPool { pool })
}

/// DepositSol with `funder` paying and `destination` receiving the JSOL.
/// The referrer is the destination, as the JPool SDK does by default.
pub fn deposit_sol_instruction(
    funder: &Pubkey,
    destination: &Pubkey,
    lamports: u64,
) -> Instruction {
    spl_stake_pool::instruction::deposit_sol(
        &STAKE_POOL_PROGRAM_ID,
        &JPOOL_STAKE_POOL,
        &withdraw_authority(),
        &JPOOL_RESERVE_STAKE,
        funder,
        destination,
        &JPOOL_MANAGER_FEE_ACCOUNT,
        destination,
        &JSOL_MINT,
        &token_program_id(),
        lamports,
    )
}

/// WithdrawSol: burns `jsol` from `source` (held by `authority`) and pays
/// the SOL from the pool reserve to `lamports_to`.
pub fn withdraw_sol_instruction(
    authority: &Pubkey,
    source: &Pubkey,
    lamports_to: &Pubkey,
    jsol: u64,
) -> Instruction {
    spl_stake_pool::instruction::withdraw_sol(
        &STAKE_POOL_PROGRAM_ID,
        &JPOOL_STAKE_POOL,
        &withdraw_authority(),
        authority,
        source,
        &JPOOL_RESERVE_STAKE,
        lamports_to,
        &JPOOL_MANAGER_FEE_ACCOUNT,
        &JSOL_MINT,
        &token_program_id(),
        jsol,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn live_pool_account() -> ChainAccount {
        ChainAccount {
            owner: STAKE_POOL_PROGRAM_ID,
            lamports: 1,
            data: include_bytes!("../tests/fixtures/mainnet_jpool_stake_pool.bin").to_vec(),
        }
    }

    #[test]
    fn program_id_matches_spl_crate() {
        assert_eq!(STAKE_POOL_PROGRAM_ID, spl_stake_pool::id());
    }

    #[test]
    fn decodes_live_jpool() {
        let pool = decode_jpool(&JPOOL_STAKE_POOL, &live_pool_account()).expect("decode");
        let rate = pool.rate();
        assert!((1.2..1.6).contains(&rate), "unexpected JSOL rate {rate}");
        let value = pool.jsol_to_lamports(11_001_704_341).unwrap();
        assert!((14_000_000_000..16_500_000_000).contains(&value), "{value}");
    }

    #[test]
    fn rejects_foreign_owner_and_wrong_address() {
        let mut account = live_pool_account();
        assert!(decode_jpool(&JSOL_MINT, &account).is_err());
        account.owner = Pubkey::new_unique();
        let error = decode_jpool(&JPOOL_STAKE_POOL, &account).unwrap_err();
        assert!(error.to_string().contains("expected SPoo1"));
    }

    #[test]
    fn quote_matches_program_math_with_fees() {
        let mut pool = decode_jpool(&JPOOL_STAKE_POOL, &live_pool_account())
            .unwrap()
            .pool;
        pool.total_lamports = 1_400;
        pool.pool_token_supply = 1_000;
        pool.sol_deposit_fee = spl_stake_pool::state::Fee {
            numerator: 1,
            denominator: 100,
        };
        pool.sol_referral_fee = 50;
        let quote = JPool { pool }.deposit_sol_quote(5_000_000_000).unwrap();
        // 5e9 * 1000 / 1400 = 3_571_428_571; fee ceil(1%) = 35_714_286
        assert_eq!(
            quote.pool_tokens_user + quote.referral_fee + quote.manager_fee,
            3_571_428_571
        );
        assert_eq!(quote.referral_fee, 35_714_286 * 50 / 100);
        assert_eq!(
            quote.minted_to_destination,
            quote.pool_tokens_user + quote.referral_fee
        );
    }
}
