//! Vote program: the `Withdraw` instruction and the account fields this
//! tool checks (authorized withdrawer, withdrawable balance).

use crate::ChainAccount;
use anyhow::{Result, bail};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

pub const VOTE_PROGRAM_ID: Pubkey = pubkey!("Vote111111111111111111111111111111111111111");
/// `VoteInstruction::Withdraw` bincode tag (u32 LE).
pub const VOTE_WITHDRAW_TAG: u32 = 3;

/// Withdraw `lamports` from `vote_account` to `recipient`, signed by the
/// authorized withdrawer.
pub fn vote_withdraw_instruction(
    vote_account: &Pubkey,
    recipient: &Pubkey,
    withdraw_authority: &Pubkey,
    lamports: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&VOTE_WITHDRAW_TAG.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: VOTE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*vote_account, false),
            AccountMeta::new(*recipient, false),
            AccountMeta::new_readonly(*withdraw_authority, true),
        ],
        data,
    }
}

/// Lamports of a vote `Withdraw` instruction, None for anything else.
pub fn vote_withdraw_lamports(program_id: &Pubkey, data: &[u8]) -> Option<u64> {
    if *program_id != VOTE_PROGRAM_ID || data.len() != 12 {
        return None;
    }
    if data[..4] != VOTE_WITHDRAW_TAG.to_le_bytes() {
        return None;
    }
    Some(u64::from_le_bytes(data[4..12].try_into().ok()?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoteAccountInfo {
    pub version: u32,
    pub node_pubkey: Pubkey,
    pub authorized_withdrawer: Pubkey,
}

/// Reads node and withdrawer. Versions 1 (V1_14_11), 2 (V3) and 3 (V4)
/// all start with version u32, node_pubkey, authorized_withdrawer; the
/// legacy version 0 layout is refused.
pub fn decode_vote_account(address: &Pubkey, account: &ChainAccount) -> Result<VoteAccountInfo> {
    if account.owner != VOTE_PROGRAM_ID {
        bail!(
            "vote account {address} is owned by {}, expected {VOTE_PROGRAM_ID}",
            account.owner
        );
    }
    if account.data.len() < 68 {
        bail!(
            "vote account {address} data is only {} bytes",
            account.data.len()
        );
    }
    let version = u32::from_le_bytes(account.data[0..4].try_into()?);
    if !(1..=3).contains(&version) {
        bail!("vote account {address} has unsupported state version {version}");
    }
    Ok(VoteAccountInfo {
        version,
        node_pubkey: Pubkey::new_from_array(account.data[4..36].try_into()?),
        authorized_withdrawer: Pubkey::new_from_array(account.data[36..68].try_into()?),
    })
}

/// What a withdraw can take while the account stays rent exempt.
pub fn withdrawable_lamports(lamports: u64, rent_exempt_minimum: u64) -> u64 {
    lamports.saturating_sub(rent_exempt_minimum)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");
    const IDENTITY: Pubkey = pubkey!("EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej");
    const VAULT: Pubkey = pubkey!("8iekq4VYWSqGPYF3wii4jA1RXTKm3hQGNBeyhX95aof8");

    #[test]
    fn decodes_live_vote_account_withdrawer() {
        let account = ChainAccount {
            owner: VOTE_PROGRAM_ID,
            lamports: 59_722_794_811,
            data: include_bytes!("../tests/fixtures/mainnet_vote_account.bin").to_vec(),
        };
        let info = decode_vote_account(&VOTE, &account).unwrap();
        assert_eq!(info.version, 3);
        assert_eq!(info.node_pubkey, IDENTITY);
        assert_eq!(info.authorized_withdrawer, VAULT);
        let mut foreign = account.clone();
        foreign.owner = Pubkey::new_unique();
        assert!(decode_vote_account(&VOTE, &foreign).is_err());
    }

    #[test]
    fn withdraw_encoding_round_trips() {
        let instruction = vote_withdraw_instruction(&VOTE, &VAULT, &VAULT, 5_000_000_000);
        assert_eq!(
            instruction.data,
            [3, 0, 0, 0, 0x00, 0xf2, 0x05, 0x2a, 0x01, 0, 0, 0]
        );
        assert_eq!(
            vote_withdraw_lamports(&instruction.program_id, &instruction.data),
            Some(5_000_000_000)
        );
        assert_eq!(vote_withdraw_lamports(&VAULT, &instruction.data), None);
        assert_eq!(withdrawable_lamports(10, 3), 7);
        assert_eq!(withdrawable_lamports(2, 3), 0);
    }
}
