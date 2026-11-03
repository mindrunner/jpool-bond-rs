//! JPool bond program (Anchor) accounts and the `bond_top_up` instruction.

use crate::{ChainAccount, token::token_program_id};
use anyhow::{Result, bail};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

pub const BOND_PROGRAM_ID: Pubkey = pubkey!("BondQ7KqZreTcW2UbeTNDcLCJQ3aXAtLn2Fm6ftaJDU");
pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");

pub const BOND_STATE_SEED: &[u8] = b"bond_state";
pub const VALIDATOR_BOND_SEED: &[u8] = b"validator_bond";
/// Seed for `BondType::Standard`; the only bond type this tool handles.
pub const STANDARD_BOND_SEED: &[u8] = b"standard";

pub const BOND_STATE_DISCRIMINATOR: [u8; 8] = [251, 95, 76, 47, 191, 108, 163, 92];
pub const VALIDATOR_BOND_DISCRIMINATOR: [u8; 8] = [82, 127, 243, 208, 195, 42, 80, 35];
pub const BOND_TOP_UP_DISCRIMINATOR: [u8; 8] = [132, 225, 254, 187, 152, 162, 176, 66];
pub const BOND_WITHDRAW_DISCRIMINATOR: [u8; 8] = [99, 64, 127, 178, 53, 117, 70, 204];

pub fn find_bond_state_address(bond_name: &str) -> Pubkey {
    Pubkey::find_program_address(
        &[BOND_STATE_SEED, STANDARD_BOND_SEED, bond_name.as_bytes()],
        &BOND_PROGRAM_ID,
    )
    .0
}

pub fn find_validator_bond_address(bond_state: &Pubkey, vote_account: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            VALIDATOR_BOND_SEED,
            bond_state.as_ref(),
            vote_account.as_ref(),
        ],
        &BOND_PROGRAM_ID,
    )
    .0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BondType {
    Standard,
    Crowdfunding,
}

/// Borsh enum: tag 0 Native, 1 StakeAccount, 2 Token(mint pubkey).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollateralType {
    Native,
    StakeAccount,
    Token(Pubkey),
}

impl std::fmt::Display for CollateralType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native => write!(f, "Native"),
            Self::StakeAccount => write!(f, "StakeAccount"),
            Self::Token(mint) => write!(f, "Token({mint})"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BondStatus {
    Active,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BondState {
    pub name: String,
    pub bond_type: BondType,
    pub collateral_type: CollateralType,
    pub reserve: Pubkey,
    pub authority: Pubkey,
    pub session_id: u32,
    pub total_members: u16,
    pub total_compensation_amount: u64,
    pub session_start_ts: i64,
    pub session_finish_ts: i64,
    pub status: BondStatus,
    pub bump: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatorBond {
    pub state: Pubkey,
    pub identity: Pubkey,
    pub vote_account: Pubkey,
    pub creator: Pubkey,
    pub withdrawal_authority: Option<Pubkey>,
    pub created_at: i64,
    pub bump: u8,
    pub locked: u64,
    pub pending_unlock: u64,
    pub pending_unlock_epoch: u64,
}

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} data truncated at offset {} (length {})",
                    self.what,
                    self.offset,
                    self.data.len()
                )
            })?;
        let slice = &self.data[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }

    fn pubkey(&mut self) -> Result<Pubkey> {
        Ok(Pubkey::new_from_array(self.take(32)?.try_into()?))
    }
}

fn check_owner_and_discriminator(
    what: &str,
    address: &Pubkey,
    account: &ChainAccount,
    discriminator: &[u8; 8],
) -> Result<()> {
    if account.owner != BOND_PROGRAM_ID {
        bail!(
            "{what} {address} is owned by {}, expected bond program {BOND_PROGRAM_ID}",
            account.owner
        );
    }
    if account.data.get(..8) != Some(discriminator.as_slice()) {
        bail!("{what} {address} has the wrong account discriminator");
    }
    Ok(())
}

pub fn decode_bond_state(address: &Pubkey, account: &ChainAccount) -> Result<BondState> {
    check_owner_and_discriminator("bond_state", address, account, &BOND_STATE_DISCRIMINATOR)?;
    let mut reader = Reader {
        data: &account.data,
        offset: 8,
        what: "bond_state",
    };
    let name_len = usize::try_from(reader.u32()?)?;
    if name_len > 32 {
        bail!("bond_state {address} name length {name_len} exceeds 32");
    }
    let name = String::from_utf8(reader.take(name_len)?.to_vec())
        .map_err(|_| anyhow::anyhow!("bond_state {address} name is not UTF-8"))?;
    let bond_type = match reader.u8()? {
        0 => BondType::Standard,
        1 => BondType::Crowdfunding,
        tag => bail!("bond_state {address} has unknown bond_type tag {tag}"),
    };
    let collateral_type = match reader.u8()? {
        0 => CollateralType::Native,
        1 => CollateralType::StakeAccount,
        2 => CollateralType::Token(reader.pubkey()?),
        tag => bail!("bond_state {address} has unknown collateral_type tag {tag}"),
    };
    let reserve = reader.pubkey()?;
    let authority = reader.pubkey()?;
    let session_id = reader.u32()?;
    let total_members = reader.u16()?;
    let total_compensation_amount = reader.u64()?;
    let session_start_ts = reader.i64()?;
    let session_finish_ts = reader.i64()?;
    let status = match reader.u8()? {
        0 => BondStatus::Active,
        1 => BondStatus::Disabled,
        tag => bail!("bond_state {address} has unknown status tag {tag}"),
    };
    let bump = reader.u8()?;
    Ok(BondState {
        name,
        bond_type,
        collateral_type,
        reserve,
        authority,
        session_id,
        total_members,
        total_compensation_amount,
        session_start_ts,
        session_finish_ts,
        status,
        bump,
    })
}

pub fn decode_validator_bond(address: &Pubkey, account: &ChainAccount) -> Result<ValidatorBond> {
    check_owner_and_discriminator(
        "validator_bond",
        address,
        account,
        &VALIDATOR_BOND_DISCRIMINATOR,
    )?;
    let mut reader = Reader {
        data: &account.data,
        offset: 8,
        what: "validator_bond",
    };
    let state = reader.pubkey()?;
    let identity = reader.pubkey()?;
    let vote_account = reader.pubkey()?;
    let creator = reader.pubkey()?;
    let withdrawal_authority = match reader.u8()? {
        0 => None,
        1 => Some(reader.pubkey()?),
        tag => bail!("validator_bond {address} has invalid withdrawal_authority tag {tag}"),
    };
    Ok(ValidatorBond {
        state,
        identity,
        vote_account,
        creator,
        withdrawal_authority,
        created_at: reader.i64()?,
        bump: reader.u8()?,
        locked: reader.u64()?,
        pending_unlock: reader.u64()?,
        pending_unlock_epoch: reader.u64()?,
    })
}

/// Accounts for `bond_top_up` with token collateral. Anchor encodes an
/// absent optional account as the program id itself.
pub fn bond_top_up_token_instruction(
    bond_state: Pubkey,
    validator_bond: Pubkey,
    payer: Pubkey,
    payer_token_account: Pubkey,
    bond_token_account: Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&BOND_TOP_UP_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: BOND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(bond_state, false),
            AccountMeta::new(validator_bond, false),
            AccountMeta::new(payer, true),
            AccountMeta::new(payer_token_account, false),
            AccountMeta::new(bond_token_account, false),
            // stake_account: None
            AccountMeta::new_readonly(BOND_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(token_program_id(), false),
            // stake_program: None
            AccountMeta::new_readonly(BOND_PROGRAM_ID, false),
        ],
        data,
    }
}

/// Accounts for `bond_withdraw` with token collateral, in the order of the
/// program's on-chain Anchor IDL: the JSOL goes from the bond's token account
/// to `destination_token_account`. `authority` is the validator identity (or
/// the bond's withdrawal authority, when one is set).
pub fn bond_withdraw_token_instruction(
    bond_state: Pubkey,
    validator_bond: Pubkey,
    authority: Pubkey,
    destination_token_account: Pubkey,
    bond_token_account: Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&BOND_WITHDRAW_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: BOND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(bond_state, false),
            AccountMeta::new(validator_bond, false),
            AccountMeta::new(authority, true),
            AccountMeta::new(authority, false),
            AccountMeta::new(destination_token_account, false),
            AccountMeta::new(bond_token_account, false),
            // stake_account: None
            AccountMeta::new_readonly(BOND_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(token_program_id(), false),
            // stake_program: None
            AccountMeta::new_readonly(BOND_PROGRAM_ID, false),
        ],
        data,
    }
}

/// Recognises a top-level `bond_top_up` by program and discriminator.
pub fn is_bond_top_up(program_id: &Pubkey, data: &[u8]) -> bool {
    *program_id == BOND_PROGRAM_ID && data.starts_with(&BOND_TOP_UP_DISCRIMINATOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pool::jsol_mint, token::associated_token_address};

    const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");
    const IDENTITY: Pubkey = pubkey!("EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej");
    const PERF_BOND_STATE: Pubkey = pubkey!("Acc4urGx7NqoHw2Whd2nMDrEamh98r3m7CdCzqkzi9Xu");
    const PERFORMANCE_BOND_STATE: Pubkey = pubkey!("B1H5wi6YpLm4DAWsbofpHCJy4LRHV7CLT3ocmXnWAQCJ");
    const VALIDATOR_BOND: Pubkey = pubkey!("5vAKRZfvuaxwamQSgDFQsTgQqFSaBt6KueUTK2w6nTkV");
    const BOND_JSOL_ATA: Pubkey = pubkey!("5WLKZYiBdE1hVYihrhgdBzZHE4BvVaucmsypN53HbQLS");

    fn fixture(data: &[u8]) -> ChainAccount {
        ChainAccount {
            owner: BOND_PROGRAM_ID,
            lamports: 1,
            data: data.to_vec(),
        }
    }

    #[test]
    fn derives_mainnet_addresses() {
        let bond_state = find_bond_state_address("perf");
        assert_eq!(bond_state, PERF_BOND_STATE);
        assert_eq!(
            find_bond_state_address("performance"),
            PERFORMANCE_BOND_STATE
        );
        let validator_bond = find_validator_bond_address(&bond_state, &VOTE);
        assert_eq!(validator_bond, VALIDATOR_BOND);
        assert_eq!(
            associated_token_address(&validator_bond, &jsol_mint()),
            BOND_JSOL_ATA
        );
    }

    #[test]
    fn decodes_live_jsol_bond_state_as_token_collateral() {
        let state = decode_bond_state(
            &PERF_BOND_STATE,
            &fixture(include_bytes!(
                "../tests/fixtures/mainnet_bond_state_perf.bin"
            )),
        )
        .expect("decode perf bond_state");
        assert_eq!(state.name, "perf");
        assert_eq!(state.bond_type, BondType::Standard);
        assert_eq!(state.collateral_type, CollateralType::Token(jsol_mint()));
        assert_eq!(state.status, BondStatus::Active);
        assert_eq!(
            Pubkey::find_program_address(
                &[BOND_STATE_SEED, STANDARD_BOND_SEED, b"perf"],
                &BOND_PROGRAM_ID
            )
            .1,
            state.bump
        );
    }

    #[test]
    fn decodes_live_sol_bond_state_as_native_collateral() {
        let state = decode_bond_state(
            &PERFORMANCE_BOND_STATE,
            &fixture(include_bytes!(
                "../tests/fixtures/mainnet_bond_state_performance.bin"
            )),
        )
        .expect("decode performance bond_state");
        assert_eq!(state.name, "performance");
        assert_eq!(state.bond_type, BondType::Standard);
        assert_eq!(state.collateral_type, CollateralType::Native);
        assert_eq!(state.status, BondStatus::Active);
    }

    #[test]
    fn decodes_live_validator_bond() {
        let bond = decode_validator_bond(
            &VALIDATOR_BOND,
            &fixture(include_bytes!(
                "../tests/fixtures/mainnet_validator_bond.bin"
            )),
        )
        .expect("decode validator_bond");
        assert_eq!(bond.state, PERF_BOND_STATE);
        assert_eq!(bond.vote_account, VOTE);
        assert_eq!(bond.identity, IDENTITY);
        assert_eq!(
            Pubkey::find_program_address(
                &[VALIDATOR_BOND_SEED, PERF_BOND_STATE.as_ref(), VOTE.as_ref()],
                &BOND_PROGRAM_ID
            )
            .1,
            bond.bump
        );
    }

    #[test]
    fn rejects_foreign_owner_and_wrong_discriminator() {
        let mut account = fixture(include_bytes!(
            "../tests/fixtures/mainnet_bond_state_perf.bin"
        ));
        account.owner = SYSTEM_PROGRAM_ID;
        let error = decode_bond_state(&PERF_BOND_STATE, &account).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("owned by 11111111111111111111111111111111")
        );
        let validator_bond = fixture(include_bytes!(
            "../tests/fixtures/mainnet_validator_bond.bin"
        ));
        assert!(decode_bond_state(&VALIDATOR_BOND, &validator_bond).is_err());
    }
}
