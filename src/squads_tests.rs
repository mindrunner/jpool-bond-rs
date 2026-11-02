use super::*;
use crate::vote::{VOTE_PROGRAM_ID, vote_withdraw_instruction, vote_withdraw_lamports};
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, option_serializer::OptionSerializer,
};
use std::str::FromStr;

const MULTISIG: Pubkey = pubkey!("De838vwWHcbAyNWTTKeKPyviMNHaX91CrD2igGDfZZdM");
const VAULT: Pubkey = pubkey!("8iekq4VYWSqGPYF3wii4jA1RXTKm3hQGNBeyhX95aof8");
const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");
const MEMBERS: [Pubkey; 4] = [
    pubkey!("244pbYAnCETRe2koFVUFiGCjXteBqzVb7ryPNcnMsgGZ"),
    pubkey!("6uMx2nsAzxvuc2mLqcmEq5Yd5STxeGL57yFYAsnnkL87"),
    pubkey!("7y6AbjqsJPm2isDw1Z6iLXf5v2Xq7pHbZDWbecfxnEWR"),
    pubkey!("AA2zqNNXJewzhcThRKMvdsDsLQSFTSVkw95JgF6ZFAAq"),
];
/// (index, vault transaction PDA, proposal PDA, creator) on mainnet.
const TRANSACTIONS: [(u64, Pubkey, Pubkey, Pubkey); 3] = [
    (
        1,
        pubkey!("FhmYQK2AUG89Ji25TRKUfgp8shTC1j8LFQS6zQt7vBvR"),
        pubkey!("3DuqGHo4K2bNh4V4PviRbowb21pEmqHeVDHMsnsN5wTd"),
        MEMBERS[0],
    ),
    (
        2,
        pubkey!("UYSFNLadY3GeV1hM1KBQGsoiYGnv2Wr14zwkb8Q5BKk"),
        pubkey!("BkA6TMdheUTAzYHGZu6Sg4B8a52babzjm5YBA2NZy1XT"),
        MEMBERS[1],
    ),
    (
        3,
        pubkey!("DaayYTpC6YMAzXZLxheXDkje7xeLKTSmm8GACbo6xKxH"),
        pubkey!("DjinZm8bksZRuADWs4SEDALKFC1PXqbsgCqDB6P4MCm8"),
        MEMBERS[0],
    ),
];

fn squads_account(data: &[u8]) -> ChainAccount {
    ChainAccount {
        owner: SQUADS_PROGRAM_ID,
        lamports: 1,
        data: data.to_vec(),
    }
}

pub(crate) fn multisig_fixture() -> ChainAccount {
    squads_account(include_bytes!(
        "../tests/fixtures/mainnet_squads_multisig.bin"
    ))
}

fn vault_transaction_fixture(index: u64) -> ChainAccount {
    squads_account(match index {
        1 => include_bytes!("../tests/fixtures/mainnet_squads_vault_tx_1.bin").as_slice(),
        2 => include_bytes!("../tests/fixtures/mainnet_squads_vault_tx_2.bin").as_slice(),
        3 => include_bytes!("../tests/fixtures/mainnet_squads_vault_tx_3.bin").as_slice(),
        _ => unreachable!(),
    })
}

fn proposal_fixture(index: u64) -> ChainAccount {
    squads_account(match index {
        1 => include_bytes!("../tests/fixtures/mainnet_squads_proposal_1.bin").as_slice(),
        2 => include_bytes!("../tests/fixtures/mainnet_squads_proposal_2.bin").as_slice(),
        3 => include_bytes!("../tests/fixtures/mainnet_squads_proposal_3.bin").as_slice(),
        _ => unreachable!(),
    })
}

fn creation_fixture(index: u64) -> &'static str {
    match index {
        1 => include_str!("../tests/fixtures/mainnet_squads_create_tx_1.json"),
        2 => include_str!("../tests/fixtures/mainnet_squads_create_tx_2.json"),
        3 => include_str!("../tests/fixtures/mainnet_squads_create_tx_3.json"),
        _ => unreachable!(),
    }
}

/// Top-level instructions of a captured transaction with every account
/// resolved (static keys, then lookup-table writable, then readonly) and
/// flags taken from the header (lookup-table accounts are never signers).
fn creation_instructions(index: u64) -> Vec<Instruction> {
    let confirmed: EncodedConfirmedTransactionWithStatusMeta =
        serde_json::from_str(creation_fixture(index)).expect("fixture JSON");
    let meta = confirmed.transaction.meta.as_ref().expect("meta");
    assert!(meta.err.is_none(), "creation tx {index} failed");
    let transaction: VersionedTransaction = confirmed.transaction.transaction.decode().unwrap();
    let message = &transaction.message;
    let header = message.header();
    let static_keys = message.static_account_keys();
    let (loaded_writable, loaded_readonly) = match &meta.loaded_addresses {
        OptionSerializer::Some(loaded) => (
            loaded
                .writable
                .iter()
                .map(|key| Pubkey::from_str(key).unwrap())
                .collect::<Vec<_>>(),
            loaded
                .readonly
                .iter()
                .map(|key| Pubkey::from_str(key).unwrap())
                .collect::<Vec<_>>(),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    let signers = usize::from(header.num_required_signatures);
    let writable_signers = signers - usize::from(header.num_readonly_signed_accounts);
    let writable_static_end =
        static_keys.len() - usize::from(header.num_readonly_unsigned_accounts);
    let meta_for = |index: u8| {
        let index = usize::from(index);
        if index < static_keys.len() {
            AccountMeta {
                pubkey: static_keys[index],
                is_signer: index < signers,
                is_writable: if index < signers {
                    index < writable_signers
                } else {
                    index < writable_static_end
                },
            }
        } else if index - static_keys.len() < loaded_writable.len() {
            AccountMeta::new(loaded_writable[index - static_keys.len()], false)
        } else {
            AccountMeta::new_readonly(
                loaded_readonly[index - static_keys.len() - loaded_writable.len()],
                false,
            )
        }
    };
    message
        .instructions()
        .iter()
        .map(|instruction| {
            let program = meta_for(instruction.program_id_index).pubkey;
            Instruction {
                program_id: program,
                accounts: instruction.accounts.iter().map(|i| meta_for(*i)).collect(),
                data: instruction.data.clone(),
            }
        })
        .collect()
}

fn find(instructions: &[Instruction], discriminator: &[u8; 8]) -> Instruction {
    let found: Vec<_> = instructions
        .iter()
        .filter(|instruction| {
            instruction.program_id == SQUADS_PROGRAM_ID
                && instruction.data.starts_with(discriminator)
        })
        .collect();
    assert_eq!(found.len(), 1);
    found[0].clone()
}

/// Built flags may only be narrower than the message's union flags.
fn assert_accounts_compatible(built: &Instruction, fixture: &Instruction) {
    assert_eq!(built.program_id, fixture.program_id);
    assert_eq!(built.accounts.len(), fixture.accounts.len());
    for (built, fixture) in built.accounts.iter().zip(&fixture.accounts) {
        assert_eq!(built.pubkey, fixture.pubkey);
        assert_eq!(built.is_signer, fixture.is_signer, "{built:?}");
        assert!(!built.is_writable || fixture.is_writable, "{built:?}");
    }
}

#[test]
fn decodes_live_multisig() {
    let multisig = decode_multisig(&MULTISIG, &multisig_fixture()).expect("decode multisig");
    assert_eq!(multisig.threshold, 2);
    assert_eq!(multisig.time_lock, 0);
    assert_eq!(multisig.transaction_index, 3);
    assert_eq!(multisig.stale_transaction_index, 0);
    assert_eq!(multisig.rent_collector, None);
    assert_eq!(multisig.config_authority, Pubkey::default());
    assert_eq!(
        multisig
            .members
            .iter()
            .map(|member| member.key)
            .collect::<Vec<_>>(),
        MEMBERS
    );
    for member in &multisig.members {
        assert_eq!(member.permissions, 7);
        assert!(member.can_initiate());
        assert_eq!(member.describe_permissions(), "Initiate+Vote+Execute");
    }
    assert!(multisig.member(&Pubkey::new_unique()).is_none());
    assert_eq!(vault_address(&MULTISIG, 0), VAULT);
}

#[test]
fn rejects_foreign_multisig_owner_and_address() {
    let mut account = multisig_fixture();
    assert!(decode_multisig(&VAULT, &account).is_err());
    account.owner = Pubkey::new_unique();
    assert!(decode_multisig(&MULTISIG, &account).is_err());
    assert!(decode_proposal(&MULTISIG, &multisig_fixture()).is_err());
}

#[test]
fn decodes_live_proposals_and_vault_transactions() {
    for (index, transaction, proposal, creator) in TRANSACTIONS {
        assert_eq!(transaction_address(&MULTISIG, index), transaction);
        assert_eq!(proposal_address(&MULTISIG, index), proposal);

        let account = proposal_fixture(index);
        let decoded = decode_proposal(&proposal, &account).unwrap();
        assert_eq!(decoded.multisig, MULTISIG);
        assert_eq!(decoded.transaction_index, index);
        assert_eq!(account.data.len(), proposal_account_len(4));
        assert!(decoded.rejected.is_empty() && decoded.cancelled.is_empty());
        match index {
            1 | 2 => {
                assert!(matches!(decoded.status, ProposalStatus::Executed(_)));
                assert_eq!(decoded.approved, MEMBERS[..2]);
                assert!(!decoded.status.is_open());
            }
            _ => {
                assert_eq!(decoded.status, ProposalStatus::Active(1_793_402_472));
                assert_eq!(decoded.approved, [MEMBERS[0]]);
                assert!(decoded.status.is_open());
            }
        }

        let account = vault_transaction_fixture(index);
        let decoded = decode_vault_transaction(&transaction, &account).unwrap();
        assert_eq!(decoded.multisig, MULTISIG);
        assert_eq!(decoded.index, index);
        assert_eq!(decoded.creator, creator);
        assert_eq!(decoded.vault_index, 0);
        assert_eq!(decoded.vault_bump, 255);
        assert!(decoded.ephemeral_signer_bumps.is_empty());
        assert_eq!(decoded.message.account_keys[0], VAULT);
        // The account is allocated with exactly VaultTransaction::size.
        assert_eq!(
            account.data.len(),
            vault_transaction_account_len(0, &decoded.message)
        );
    }
}

#[test]
fn creation_transactions_round_trip_through_our_encoder() {
    for (index, transaction, proposal, creator) in TRANSACTIONS {
        let instructions = creation_instructions(index);
        let create = find(&instructions, &VAULT_TRANSACTION_CREATE_DISCRIMINATOR);
        let args = VaultTransactionCreateArgs::decode(&create.data).unwrap();
        assert_eq!(args.vault_index, 0);
        assert_eq!(args.ephemeral_signers, 0);
        assert_eq!(args.memo, None);
        assert_eq!(args.encode().unwrap(), create.data);

        // TransactionMessage bytes decode and re-encode exactly, and match
        // what the program stored in the VaultTransaction account.
        let message = VaultMessage::decode_transaction_message(&args.transaction_message).unwrap();
        assert_eq!(
            message.encode_transaction_message().unwrap(),
            args.transaction_message
        );
        let stored =
            decode_vault_transaction(&transaction, &vault_transaction_fixture(index)).unwrap();
        assert_eq!(stored.message, message);

        // Our compiler reproduces the SDK's key order and header.
        let compiled = VaultMessage::compile(&VAULT, &message.instructions().unwrap()).unwrap();
        assert_eq!(compiled, message);

        let built =
            vault_transaction_create_instruction(&MULTISIG, index, &creator, &creator, &args)
                .unwrap();
        assert_eq!(built.data, create.data);
        assert_accounts_compatible(&built, &create);
        assert_eq!(built.accounts[1].pubkey, transaction);

        let proposal_ix = find(&instructions, &PROPOSAL_CREATE_DISCRIMINATOR);
        let built = proposal_create_instruction(&MULTISIG, index, &creator, &creator, false);
        assert_eq!(built.data, proposal_ix.data);
        assert_accounts_compatible(&built, &proposal_ix);
        assert_eq!(built.accounts[1].pubkey, proposal);
    }
}

#[test]
fn live_vote_withdraw_proposal_matches_our_builder() {
    let stored = decode_vault_transaction(&TRANSACTIONS[2].1, &vault_transaction_fixture(3))
        .unwrap()
        .message;
    let instructions = stored.instructions().unwrap();
    assert_eq!(instructions.len(), 1);
    assert_eq!(instructions[0].program_id, VOTE_PROGRAM_ID);
    assert_eq!(
        vote_withdraw_lamports(&instructions[0].program_id, &instructions[0].data),
        Some(5_000_000_000)
    );
    let ours = VaultMessage::compile(
        &VAULT,
        &[vote_withdraw_instruction(
            &VOTE,
            &VAULT,
            &VAULT,
            5_000_000_000,
        )],
    )
    .unwrap();
    assert_eq!(ours, stored);
}

#[test]
fn compile_refuses_foreign_signers() {
    let other = Pubkey::new_unique();
    assert!(
        VaultMessage::compile(
            &VAULT,
            &[vote_withdraw_instruction(&VOTE, &VAULT, &other, 1)]
        )
        .is_err()
    );
}

#[test]
fn transaction_message_decoder_rejects_trailing_and_bad_indexes() {
    let message = decode_vault_transaction(&TRANSACTIONS[2].1, &vault_transaction_fixture(3))
        .unwrap()
        .message;
    let mut bytes = message.encode_transaction_message().unwrap();
    bytes.push(0);
    assert!(VaultMessage::decode_transaction_message(&bytes).is_err());
    let mut bad = message.clone();
    bad.instructions[0].account_indexes[0] = 9;
    let bytes = bad.encode_transaction_message().unwrap();
    assert!(VaultMessage::decode_transaction_message(&bytes).is_err());
}
