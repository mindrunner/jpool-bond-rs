use super::*;
use crate::{
    bond::{BOND_PROGRAM_ID, bond_top_up_token_instruction},
    history::is_identity_topup,
    pool::{STAKE_POOL_PROGRAM_ID, deposit_sol_instruction},
    token::{TOKEN_ACCOUNT_LEN, token_program_id},
};
use solana_instruction::AccountMeta;
use solana_pubkey::pubkey;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta;
use std::{cell::RefCell, path::PathBuf};

const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");
const IDENTITY: Pubkey = pubkey!("EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej");
const IDENTITY_JSOL_ATA: Pubkey = pubkey!("7x2txnBegCL3jnhvrVPgnPL9gcssGYedeDGcTGrmxdYe");
const BOND_STATE: Pubkey = pubkey!("Acc4urGx7NqoHw2Whd2nMDrEamh98r3m7CdCzqkzi9Xu");
const VALIDATOR_BOND: Pubkey = pubkey!("5vAKRZfvuaxwamQSgDFQsTgQqFSaBt6KueUTK2w6nTkV");
const BOND_JSOL_ATA: Pubkey = pubkey!("5WLKZYiBdE1hVYihrhgdBzZHE4BvVaucmsypN53HbQLS");
/// Ephemeral funder the JPool SDK used in the fixture's DepositSol.
const FIXTURE_FUNDER: Pubkey = pubkey!("DuGvL9r6o4MBkb374Es7tjbWyb1BWcSFXsk4ML45jWkw");
const FIXTURE_DEPOSIT_LAMPORTS: u64 = 5_205_587_740;
const FIXTURE_TOPUP_JSOL: u64 = 3_763_402_473;
const LIVE_BOND_JSOL: u64 = 11_001_704_341;

fn fixture_transaction() -> VersionedTransaction {
    let raw = include_str!("../tests/fixtures/mainnet_topup_tx_5nGpKven.json");
    let confirmed: EncodedConfirmedTransactionWithStatusMeta =
        serde_json::from_str(raw).expect("fixture JSON");
    assert!(
        confirmed
            .transaction
            .meta
            .as_ref()
            .is_some_and(|meta| meta.err.is_none()),
        "fixture transaction must have succeeded"
    );
    confirmed
        .transaction
        .transaction
        .decode()
        .expect("decode fixture transaction")
}

/// Rebuilds each top-level instruction with account flags from the header.
fn fixture_instructions() -> Vec<Instruction> {
    let transaction = fixture_transaction();
    let message = &transaction.message;
    let keys = message.static_account_keys();
    let header = message.header();
    let signers = usize::from(header.num_required_signatures);
    let writable_signers = signers - usize::from(header.num_readonly_signed_accounts);
    let writable_unsigned_end = keys.len() - usize::from(header.num_readonly_unsigned_accounts);
    let meta = |index: u8| {
        let index = usize::from(index);
        let is_signer = index < signers;
        let is_writable = if is_signer {
            index < writable_signers
        } else {
            index < writable_unsigned_end
        };
        AccountMeta {
            pubkey: keys[index],
            is_signer,
            is_writable,
        }
    };
    message
        .instructions()
        .iter()
        .map(|instruction| Instruction {
            program_id: keys[usize::from(instruction.program_id_index)],
            accounts: instruction
                .accounts
                .iter()
                .map(|index| meta(*index))
                .collect(),
            data: instruction.data.clone(),
        })
        .collect()
}

fn find_instruction(instructions: &[Instruction], program: Pubkey, tag: &[u8]) -> Instruction {
    let matches: Vec<_> = instructions
        .iter()
        .filter(|instruction| {
            instruction.program_id == program && instruction.data.starts_with(tag)
        })
        .collect();
    assert_eq!(matches.len(), 1, "expected one {program} instruction");
    matches[0].clone()
}

#[test]
fn bond_top_up_builder_matches_mainnet_fixture() {
    let instructions = fixture_instructions();
    let fixture = find_instruction(
        &instructions,
        BOND_PROGRAM_ID,
        &bond::BOND_TOP_UP_DISCRIMINATOR,
    );
    assert_eq!(
        u64::from_le_bytes(fixture.data[8..16].try_into().unwrap()),
        FIXTURE_TOPUP_JSOL
    );
    let built = bond_top_up_token_instruction(
        BOND_STATE,
        VALIDATOR_BOND,
        IDENTITY,
        IDENTITY_JSOL_ATA,
        BOND_JSOL_ATA,
        FIXTURE_TOPUP_JSOL,
    );
    assert_eq!(built.program_id, fixture.program_id);
    assert_eq!(built.data, fixture.data);
    assert_eq!(built.accounts, fixture.accounts);
}

#[test]
fn deposit_sol_builder_matches_mainnet_fixture() {
    let instructions = fixture_instructions();
    // StakePoolInstruction::DepositSol is borsh tag 14.
    let fixture = find_instruction(&instructions, STAKE_POOL_PROGRAM_ID, &[14]);
    assert_eq!(
        u64::from_le_bytes(fixture.data[1..9].try_into().unwrap()),
        FIXTURE_DEPOSIT_LAMPORTS
    );
    let built = deposit_sol_instruction(
        &FIXTURE_FUNDER,
        &IDENTITY_JSOL_ATA,
        FIXTURE_DEPOSIT_LAMPORTS,
    );
    assert_eq!(built.program_id, fixture.program_id);
    assert_eq!(built.data, fixture.data);
    assert_eq!(built.accounts, fixture.accounts);
}

#[test]
fn create_ata_idempotent_matches_mainnet_fixture() {
    let instructions = fixture_instructions();
    let fixture = find_instruction(
        &instructions,
        spl_associated_token_account_interface::program::id(),
        &[1],
    );
    let built = create_associated_token_account_idempotent(
        &IDENTITY,
        &IDENTITY,
        &jsol_mint(),
        &token_program_id(),
    );
    assert_eq!(built.program_id, fixture.program_id);
    assert_eq!(built.data, fixture.data);
    // Message flags are the union over all instructions (the identity also
    // signs, DepositSol writes the mint), so they can only be wider here.
    assert_eq!(built.accounts.len(), fixture.accounts.len());
    for (built, fixture) in built.accounts.iter().zip(&fixture.accounts) {
        assert_eq!(built.pubkey, fixture.pubkey);
        assert!(!built.is_signer || fixture.is_signer, "{built:?}");
        assert!(!built.is_writable || fixture.is_writable, "{built:?}");
    }
}

#[test]
fn derived_addresses_match_fixture_and_spec() {
    let addresses = Addresses::derive(&config(8, IDENTITY));
    assert_eq!(addresses.bond_state, BOND_STATE);
    assert_eq!(addresses.validator_bond, VALIDATOR_BOND);
    assert_eq!(addresses.bond_token_account, BOND_JSOL_ATA);
    assert_eq!(addresses.identity_token_account, IDENTITY_JSOL_ATA);
}

#[test]
fn history_detects_identity_signed_topup() {
    let transaction = fixture_transaction();
    assert!(is_identity_topup(&transaction, &IDENTITY));
    // A non-signer key that appears in the transaction does not count.
    assert!(!is_identity_topup(&transaction, &BOND_JSOL_ATA));
    assert!(!is_identity_topup(&transaction, &Pubkey::new_unique()));
}

fn config(min_value_sol: u64, identity: Pubkey) -> Config {
    Config {
        rpc_url: "https://rpc.example.test".into(),
        vote_account: VOTE,
        identity_pubkey: identity,
        identity_keypair_path: PathBuf::from("/nonexistent/identity.json"),
        bond_name: "perf".into(),
        min_value_lamports: min_value_sol * config::LAMPORTS_PER_SOL,
        topup_lamports: 5 * config::LAMPORTS_PER_SOL,
        identity_reserve_lamports: 10 * config::LAMPORTS_PER_SOL,
        priority_fee_micro_lamports: 1_000,
        metrics_file: PathBuf::from("/nonexistent/unused.prom"),
    }
}

fn account(owner: Pubkey, data: &[u8]) -> ChainAccount {
    ChainAccount {
        owner,
        lamports: 1,
        data: data.to_vec(),
    }
}

fn token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> ChainAccount {
    let mut data = vec![0; TOKEN_ACCOUNT_LEN];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    account(token_program_id(), &data)
}

fn live_snapshot(identity_lamports: u64) -> ChainSnapshot {
    let pool = account(
        STAKE_POOL_PROGRAM_ID,
        include_bytes!("../tests/fixtures/mainnet_jpool_stake_pool.bin"),
    );
    let last_update_epoch = pool::decode_jpool(&JPOOL_STAKE_POOL, &pool)
        .unwrap()
        .last_update_epoch();
    ChainSnapshot {
        current_epoch: last_update_epoch,
        bond_state: Some(account(
            BOND_PROGRAM_ID,
            include_bytes!("../tests/fixtures/mainnet_bond_state_perf.bin"),
        )),
        validator_bond: Some(account(
            BOND_PROGRAM_ID,
            include_bytes!("../tests/fixtures/mainnet_validator_bond.bin"),
        )),
        bond_token_account: Some(account(
            token_program_id(),
            include_bytes!("../tests/fixtures/mainnet_bond_jsol_ata.bin"),
        )),
        stake_pool: Some(pool),
        jsol_mint: Some(account(
            token_program_id(),
            include_bytes!("../tests/fixtures/mainnet_jsol_mint.bin"),
        )),
        identity_token_account: Some(token_account(jsol_mint(), IDENTITY, 0)),
        identity_lamports,
        token_account_rent_lamports: None,
    }
}

fn assess_live(min_value_sol: u64, snapshot: &ChainSnapshot) -> Result<Assessment> {
    let config = config(min_value_sol, IDENTITY);
    assess(&config, &Addresses::derive(&config), snapshot)
}

#[test]
fn live_bond_above_floor_needs_no_topup() {
    let assessment = assess_live(8, &live_snapshot(78 * config::LAMPORTS_PER_SOL)).unwrap();
    assert_eq!(assessment.bond_jsol, LIVE_BOND_JSOL);
    assert!(matches!(assessment.decision, Decision::NoTopUp));
    assert!(assessment.bond_value_lamports > 8 * config::LAMPORTS_PER_SOL);
    assert!(render_assessment(&assessment).contains("decision:           no top-up"));
}

#[test]
fn live_bond_below_raised_floor_builds_exact_topup() {
    let snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    let assessment = assess_live(20, &snapshot).unwrap();
    let Decision::TopUp(plan) = &assessment.decision else {
        panic!("expected a top-up, got {:?}", assessment.decision);
    };
    let pool =
        pool::decode_jpool(&JPOOL_STAKE_POOL, snapshot.stake_pool.as_ref().unwrap()).unwrap();
    let expected = pool
        .deposit_sol_quote(5 * config::LAMPORTS_PER_SOL)
        .unwrap();
    assert_eq!(plan.topup_jsol, expected.minted_to_destination);
    assert_eq!(plan.instructions.len(), 3);
    assert_eq!(
        plan.instructions[1],
        deposit_sol_instruction(&IDENTITY, &IDENTITY_JSOL_ATA, 5 * config::LAMPORTS_PER_SOL)
    );
    assert_eq!(
        plan.instructions[2],
        bond_top_up_token_instruction(
            BOND_STATE,
            VALIDATOR_BOND,
            IDENTITY,
            IDENTITY_JSOL_ATA,
            BOND_JSOL_ATA,
            expected.minted_to_destination
        )
    );
    assert!(render_assessment(&assessment).contains("decision:           top-up 5 SOL"));
}

#[test]
fn identity_balance_guard_names_the_values() {
    // 5 top-up + 10 reserve + fee buffer > 15 SOL.
    let assessment = assess_live(20, &live_snapshot(15 * config::LAMPORTS_PER_SOL)).unwrap();
    let Decision::Refused(reason) = assessment.decision else {
        panic!("expected refusal");
    };
    assert!(reason.contains("balance 15 SOL"), "{reason}");
    assert!(reason.contains("reserve 10"), "{reason}");
}

#[test]
fn stale_pool_refuses_topup() {
    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    snapshot.current_epoch += 1;
    let assessment = assess_live(20, &snapshot).unwrap();
    assert!(
        matches!(assessment.decision, Decision::Refused(reason) if reason.contains("last_update_epoch"))
    );
}

#[test]
fn missing_identity_ata_requires_rent_estimate_and_counts_it() {
    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    snapshot.identity_token_account = None;
    assert!(assess_live(20, &snapshot).is_err());
    snapshot.token_account_rent_lamports = Some(2_039_280);
    let assessment = assess_live(20, &snapshot).unwrap();
    let Decision::TopUp(plan) = assessment.decision else {
        panic!("expected top-up");
    };
    assert_eq!(plan.ata_rent_lamports, 2_039_280);
}

#[test]
fn native_collateral_is_refused() {
    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    let mut data = include_bytes!("../tests/fixtures/mainnet_bond_state_perf.bin").to_vec();
    // name "perf": collateral tag at 8 + 4 + 4 + 1; drop the mint payload.
    let tag = 17;
    assert_eq!(data[tag], 2);
    data[tag] = 0;
    data.drain(tag + 1..tag + 33);
    snapshot.bond_state = Some(account(BOND_PROGRAM_ID, &data));
    let error = assess_live(8, &snapshot).unwrap_err().to_string();
    assert!(error.contains("collateral is Native"), "{error}");
}

#[test]
fn foreign_owner_and_wrong_vote_are_refused() {
    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    snapshot.validator_bond.as_mut().unwrap().owner = Pubkey::new_unique();
    assert!(
        assess_live(8, &snapshot)
            .unwrap_err()
            .to_string()
            .contains("owned by")
    );

    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    snapshot.validator_bond.as_mut().unwrap().data[72..104].copy_from_slice(IDENTITY.as_ref());
    let error = assess_live(8, &snapshot).unwrap_err().to_string();
    assert!(
        error.contains(&format!("vote account is {IDENTITY}")),
        "{error}"
    );

    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    snapshot.validator_bond.as_mut().unwrap().data[8..40].copy_from_slice(IDENTITY.as_ref());
    let error = assess_live(8, &snapshot).unwrap_err().to_string();
    assert!(error.contains("belongs to bond_state"), "{error}");
}

#[test]
fn foreign_stake_pool_owner_is_refused() {
    let mut snapshot = live_snapshot(78 * config::LAMPORTS_PER_SOL);
    snapshot.stake_pool.as_mut().unwrap().owner = Pubkey::new_unique();
    assert!(assess_live(8, &snapshot).is_err());
}

struct StaticSource {
    snapshot: ChainSnapshot,
    recent: Option<RecentTopUp>,
    cooldown_queries: RefCell<Vec<i64>>,
}

impl ChainDataSource for StaticSource {
    fn snapshot(&self, _addresses: &Addresses) -> Result<ChainSnapshot> {
        Ok(self.snapshot.clone())
    }

    fn recent_identity_topup(
        &self,
        _addresses: &Addresses,
        since_unix: i64,
    ) -> Result<Option<RecentTopUp>> {
        self.cooldown_queries.borrow_mut().push(since_unix);
        Ok(self.recent.clone())
    }
}

#[test]
fn cooldown_from_chain_history_refuses_second_topup() {
    let now = 1_800_000_000;
    let source = StaticSource {
        snapshot: live_snapshot(78 * config::LAMPORTS_PER_SOL),
        recent: Some(RecentTopUp {
            signature: solana_signature::Signature::from([7; 64]),
            block_time: Some(now - 3_600),
        }),
        cooldown_queries: RefCell::new(Vec::new()),
    };
    let assessment = plan(&source, &config(20, IDENTITY), now).unwrap();
    assert!(
        matches!(&assessment.decision, Decision::Refused(reason) if reason.contains("3600s ago"))
    );
    assert_eq!(
        *source.cooldown_queries.borrow(),
        vec![now - TOPUP_COOLDOWN_SECONDS]
    );
    assert!(assessment.blocked_topup.is_some());
    let rendered = render_assessment(&assessment);
    assert!(
        rendered.contains("decision:           top-up 5 SOL"),
        "{rendered}"
    );
    assert!(
        rendered.contains("refused:            identity"),
        "{rendered}"
    );
}

#[test]
fn cooldown_is_not_queried_when_no_topup_is_due() {
    let source = StaticSource {
        snapshot: live_snapshot(78 * config::LAMPORTS_PER_SOL),
        recent: None,
        cooldown_queries: RefCell::new(Vec::new()),
    };
    let assessment = plan(&source, &config(8, IDENTITY), 1_800_000_000).unwrap();
    assert!(matches!(assessment.decision, Decision::NoTopUp));
    assert!(source.cooldown_queries.borrow().is_empty());
}

#[test]
fn verification_requires_exact_increase() {
    assert!(check_topup_increase(10, 15, 5).is_ok());
    assert!(check_topup_increase(10, 14, 5).is_err());
    assert!(check_topup_increase(10, 16, 5).is_err());
    assert!(check_topup_increase(10, 10, 5).is_err());
}

#[test]
fn fee_buffer_covers_all_attempts() {
    // (1_000 * 1_400_000 / 1e6 + 5_000) * 3
    assert_eq!(fee_buffer_lamports(1_000).unwrap(), 19_200);
}
