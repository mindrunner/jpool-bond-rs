//! SPL Token account helpers (classic Token program only).

use crate::ChainAccount;
use anyhow::{Result, bail};
use solana_pubkey::{Pubkey, pubkey};
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;

const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_ACCOUNT_LEN: usize = 165;
pub const MINT_LEN: usize = 82;

pub fn token_program_id() -> Pubkey {
    TOKEN_PROGRAM_ID
}

/// ATA address; off-curve owners (PDAs) are valid, as with allowOwnerOffCurve.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(owner, mint, &TOKEN_PROGRAM_ID)
}

/// Validates an initialized token account for `mint` held by `authority`
/// at its ATA address and returns its balance in base units.
pub fn validate_token_account(
    what: &str,
    address: &Pubkey,
    account: &ChainAccount,
    authority: &Pubkey,
    mint: &Pubkey,
) -> Result<u64> {
    let expected = associated_token_address(authority, mint);
    if *address != expected {
        bail!("{what} {address} is not the ATA {expected} of {authority} for mint {mint}");
    }
    if account.owner != TOKEN_PROGRAM_ID || account.data.len() != TOKEN_ACCOUNT_LEN {
        bail!(
            "{what} {address} has owner {} and length {}, expected {TOKEN_PROGRAM_ID} and {TOKEN_ACCOUNT_LEN}",
            account.owner,
            account.data.len()
        );
    }
    let account_mint = Pubkey::new_from_array(account.data[0..32].try_into()?);
    let account_authority = Pubkey::new_from_array(account.data[32..64].try_into()?);
    if account_mint != *mint || account_authority != *authority {
        bail!(
            "{what} {address} has mint {account_mint} and owner {account_authority}, expected {mint} and {authority}"
        );
    }
    // AccountState: 0 uninitialized, 1 initialized, 2 frozen.
    if account.data[108] != 1 {
        bail!(
            "{what} {address} is not usable (state {})",
            account.data[108]
        );
    }
    Ok(u64::from_le_bytes(account.data[64..72].try_into()?))
}

/// Returns the mint's supply after checking owner, length and decimals.
pub fn validate_mint(address: &Pubkey, account: &ChainAccount, decimals: u8) -> Result<u64> {
    if account.owner != TOKEN_PROGRAM_ID || account.data.len() != MINT_LEN {
        bail!(
            "mint {address} has owner {} and length {}, expected {TOKEN_PROGRAM_ID} and {MINT_LEN}",
            account.owner,
            account.data.len()
        );
    }
    if account.data[44] != decimals || account.data[45] != 1 {
        bail!(
            "mint {address} has decimals {} (initialized {}), expected {decimals}",
            account.data[44],
            account.data[45]
        );
    }
    Ok(u64::from_le_bytes(account.data[36..44].try_into()?))
}
