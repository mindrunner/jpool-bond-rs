use super::*;
use crate::{
    burn::BondJsolChange,
    config::{SquadsSettings, format_sol},
    pool::{decode_jpool, deposit_sol_instruction},
    squads::{PROPOSAL_DISCRIMINATOR, SQUADS_PROGRAM_ID, VAULT_TRANSACTION_DISCRIMINATOR},
    vote::VOTE_PROGRAM_ID,
};
use solana_pubkey::pubkey;
use solana_signature::Signature;
use std::{cell::RefCell, path::PathBuf};

const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");
const MULTISIG: Pubkey = pubkey!("De838vwWHcbAyNWTTKeKPyviMNHaX91CrD2igGDfZZdM");
const VAULT: Pubkey = pubkey!("8iekq4VYWSqGPYF3wii4jA1RXTKm3hQGNBeyhX95aof8");
const MEMBER: Pubkey = pubkey!("244pbYAnCETRe2koFVUFiGCjXteBqzVb7ryPNcnMsgGZ");
const BOND_JSOL_ATA: Pubkey = pubkey!("5WLKZYiBdE1hVYihrhgdBzZHE4BvVaucmsypN53HbQLS");
const VALIDATOR_BOND: Pubkey = pubkey!("5vAKRZfvuaxwamQSgDFQsTgQqFSaBt6KueUTK2w6nTkV");
const NOW: i64 = 1_793_614_296;
const DAY: i64 = SECONDS_PER_DAY;
/// Mainnet rent per byte (incl. the 128-byte overhead) when fixtures were taken.
const RENT_PER_BYTE: u64 = 5_080;

fn account(owner: Pubkey, lamports: u64, data: &[u8]) -> ChainAccount {
    ChainAccount {
        owner,
        lamports,
        data: data.to_vec(),
    }
}

fn config(proposer: Pubkey) -> SquadsConfig {
    SquadsConfig {
        rpc_url: "https://rpc.example.test".into(),
        vote_account: VOTE,
        bond_name: "perf".into(),
        priority_fee_micro_lamports: 1_000,
        metrics_file: PathBuf::from("/nonexistent/unused.prom"),
        floor_lamports: 5 * LAMPORTS_PER_SOL,
        propose_runway_days: 10.0,
        topup_days: 30.0,
        critical_runway_days: 3.0,
        burn_lookback_days: 30,
        burn_min_span_days: 3.0,
        burn_fallback_lamports_per_day: 100_000_000,
        squads: SquadsSettings {
            multisig: MULTISIG,
            vault_index: 0,
            proposer_pubkey: proposer,
            proposer_keypair_path: PathBuf::from("/nonexistent/proposer.json"),
            proposer_reserve_lamports: 10_000_000,
            pending_scan: 20,
            jsol_margin_bps: 100,
        },
    }
}

fn snapshot(proposer_lamports: u64) -> SquadsSnapshot {
    let pool = account(
        STAKE_POOL_PROGRAM_ID,
        1,
        include_bytes!("../tests/fixtures/mainnet_jpool_stake_pool.bin"),
    );
    SquadsSnapshot {
        current_epoch: decode_jpool(&JPOOL_STAKE_POOL, &pool)
            .unwrap()
            .last_update_epoch(),
        bond_state: Some(account(
            BOND_PROGRAM_ID,
            1,
            include_bytes!("../tests/fixtures/mainnet_bond_state_perf.bin"),
        )),
        validator_bond: Some(account(
            BOND_PROGRAM_ID,
            1,
            include_bytes!("../tests/fixtures/mainnet_validator_bond.bin"),
        )),
        bond_token_account: Some(account(
            token_program_id(),
            1,
            include_bytes!("../tests/fixtures/mainnet_bond_jsol_ata.bin"),
        )),
        stake_pool: Some(pool),
        jsol_mint: Some(account(
            token_program_id(),
            1,
            include_bytes!("../tests/fixtures/mainnet_jsol_mint.bin"),
        )),
        multisig: Some(account(
            SQUADS_PROGRAM_ID,
            1,
            include_bytes!("../tests/fixtures/mainnet_squads_multisig.bin"),
        )),
        vote_account: Some(account(
            VOTE_PROGRAM_ID,
            59_722_794_811,
            include_bytes!("../tests/fixtures/mainnet_vote_account.bin"),
        )),
        vault: Some(account(SYSTEM_PROGRAM_ID, 2_027_435_128, &[])),
        vault_token_account: None,
        proposer_lamports,
    }
}

fn fixture_scan() -> Vec<ScannedTransaction> {
    let squads = |data: &[u8]| Some(account(SQUADS_PROGRAM_ID, 1, data));
    vec![
        ScannedTransaction {
            index: 3,
            proposal: squads(include_bytes!(
                "../tests/fixtures/mainnet_squads_proposal_3.bin"
            )),
            transaction: squads(include_bytes!(
                "../tests/fixtures/mainnet_squads_vault_tx_3.bin"
            )),
        },
        ScannedTransaction {
            index: 2,
            proposal: squads(include_bytes!(
                "../tests/fixtures/mainnet_squads_proposal_2.bin"
            )),
            transaction: squads(include_bytes!(
                "../tests/fixtures/mainnet_squads_vault_tx_2.bin"
            )),
        },
        ScannedTransaction {
            index: 1,
            proposal: squads(include_bytes!(
                "../tests/fixtures/mainnet_squads_proposal_1.bin"
            )),
            transaction: squads(include_bytes!(
                "../tests/fixtures/mainnet_squads_vault_tx_1.bin"
            )),
        },
    ]
}

/// The live bond history shape: daily claims of ~0.048 JSOL since
/// 2026-10-27, top-ups before, first transaction 15.5 days ago.
fn live_history() -> BondHistory {
    let change = |age: i64, delta: i128| BondJsolChange {
        signature: Signature::default(),
        block_time: Some(NOW - age),
        delta,
    };
    BondHistory {
        changes: vec![
            change(DAY / 4, -47_356_682),
            change(DAY + DAY / 4, -48_266_326),
            change(2 * DAY + DAY / 4, -49_176_705),
            change(3 * DAY + DAY / 4, -47_761_619),
            change(4 * DAY + DAY / 4, -50_850_582),
            change(5 * DAY + DAY / 4, -49_976_670),
            change(5 * DAY + DAY / 2, 3_763_402_473),
            change(15 * DAY + DAY / 2, 3_619_150_934),
            change(15 * DAY + DAY / 2 + 60, 3_619_150_934),
        ],
        covers_window: false,
        oldest_in_window: Some(NOW - 15 * DAY - DAY / 2 - 60),
    }
}

struct StaticSource {
    snapshot: SquadsSnapshot,
    scan: Vec<ScannedTransaction>,
    history: BondHistory,
    simulation_error: Option<String>,
    simulations: RefCell<Vec<Vec<Instruction>>>,
    scanned_indexes: RefCell<Vec<u64>>,
}

impl StaticSource {
    fn live(proposer_lamports: u64) -> Self {
        Self {
            snapshot: snapshot(proposer_lamports),
            scan: fixture_scan(),
            history: live_history(),
            simulation_error: None,
            simulations: RefCell::new(Vec::new()),
            scanned_indexes: RefCell::new(Vec::new()),
        }
    }
}

impl SquadsDataSource for StaticSource {
    fn snapshot(&self, _addresses: &SquadsAddresses) -> Result<SquadsSnapshot> {
        Ok(self.snapshot.clone())
    }

    fn rent_exempt_minimum(&self, data_len: usize) -> Result<u64> {
        Ok((data_len as u64 + 128) * RENT_PER_BYTE)
    }

    fn scan_transactions(
        &self,
        _multisig: &Pubkey,
        indexes: &[u64],
    ) -> Result<Vec<ScannedTransaction>> {
        self.scanned_indexes.borrow_mut().extend_from_slice(indexes);
        Ok(self
            .scan
            .iter()
            .filter(|entry| indexes.contains(&entry.index))
            .cloned()
            .collect())
    }

    fn creation_time(&self, _account: &Pubkey) -> Result<Option<i64>> {
        Ok(Some(NOW - 3_600))
    }

    fn bond_history(
        &self,
        _addresses: &SquadsAddresses,
        _window_start: i64,
    ) -> Result<BondHistory> {
        Ok(self.history.clone())
    }

    fn simulate_vault_transaction(
        &self,
        vault: &Pubkey,
        instructions: &[Instruction],
    ) -> Result<SimulationResult> {
        assert_eq!(*vault, VAULT);
        self.simulations.borrow_mut().push(instructions.to_vec());
        Ok(SimulationResult {
            units_consumed: Some(120_000),
            error: self.simulation_error.clone(),
            logs: Vec::new(),
        })
    }
}

/// Stored-format (borsh Vec) encoding of a vault message, for synthetic
/// accounts.
fn stored_message(message: &VaultMessage) -> Vec<u8> {
    let mut out = vec![
        message.num_signers,
        message.num_writable_signers,
        message.num_writable_non_signers,
    ];
    out.extend_from_slice(&(message.account_keys.len() as u32).to_le_bytes());
    for key in &message.account_keys {
        out.extend_from_slice(key.as_ref());
    }
    out.extend_from_slice(&(message.instructions.len() as u32).to_le_bytes());
    for instruction in &message.instructions {
        out.push(instruction.program_id_index);
        out.extend_from_slice(&(instruction.account_indexes.len() as u32).to_le_bytes());
        out.extend_from_slice(&instruction.account_indexes);
        out.extend_from_slice(&(instruction.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&instruction.data);
    }
    out.extend_from_slice(&0_u32.to_le_bytes());
    out
}

fn vault_transaction_account(index: u64, creator: Pubkey, message: &VaultMessage) -> ChainAccount {
    let mut data = VAULT_TRANSACTION_DISCRIMINATOR.to_vec();
    data.extend_from_slice(MULTISIG.as_ref());
    data.extend_from_slice(creator.as_ref());
    data.extend_from_slice(&index.to_le_bytes());
    data.extend_from_slice(&[255, 0, 255]);
    data.extend_from_slice(&0_u32.to_le_bytes());
    data.extend_from_slice(&stored_message(message));
    assert_eq!(data.len(), vault_transaction_account_len(0, message));
    account(SQUADS_PROGRAM_ID, 1, &data)
}

/// status tag: 0 Draft, 1 Active, 3 Approved, 5 Executed, 6 Cancelled.
fn proposal_account(index: u64, status_tag: u8, timestamp: i64) -> ChainAccount {
    let mut data = PROPOSAL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(MULTISIG.as_ref());
    data.extend_from_slice(&index.to_le_bytes());
    data.push(status_tag);
    data.extend_from_slice(&timestamp.to_le_bytes());
    data.push(255);
    for _ in 0..3 {
        data.extend_from_slice(&0_u32.to_le_bytes());
    }
    data.resize(proposal_account_len(4), 0);
    account(SQUADS_PROGRAM_ID, 1, &data)
}

fn our_topup_message(validator_bond: Pubkey, lamports: u64) -> VaultMessage {
    let vault_ata = associated_token_address(&VAULT, &jsol_mint());
    VaultMessage::compile(
        &VAULT,
        &[
            vote_withdraw_instruction(&VOTE, &VAULT, &VAULT, lamports),
            deposit_sol_instruction(&VAULT, &vault_ata, lamports),
            bond_top_up_token_instruction(
                find_bond_state_address("perf"),
                validator_bond,
                VAULT,
                vault_ata,
                BOND_JSOL_ATA,
                1,
            ),
        ],
    )
    .unwrap()
}

/// Replaces index 3 with one of our top-up proposals.
fn with_our_proposal(source: &mut StaticSource, status_tag: u8, timestamp: i64) {
    source.scan[0] = ScannedTransaction {
        index: 3,
        proposal: Some(proposal_account(3, status_tag, timestamp)),
        transaction: Some(vault_transaction_account(
            3,
            MEMBER,
            &our_topup_message(VALIDATOR_BOND, 2_500_000_000),
        )),
    };
}

fn assess(source: &StaticSource, config: &SquadsConfig) -> SquadsAssessment {
    plan_squads(source, config, NOW).expect("plan")
}

fn forced(proposer: Pubkey) -> SquadsConfig {
    let mut config = config(proposer);
    config.propose_runway_days = 1000.0;
    config
}

#[test]
fn derives_live_addresses() {
    let addresses = SquadsAddresses::derive(&config(MEMBER));
    assert_eq!(addresses.vault, VAULT);
    assert_eq!(addresses.validator_bond, VALIDATOR_BOND);
    assert_eq!(addresses.bond_token_account, BOND_JSOL_ATA);
    assert_eq!(
        addresses.vault_token_account,
        associated_token_address(&VAULT, &jsol_mint())
    );
}

#[test]
fn proposal_amount_rounds_up_and_is_bounded() {
    let sol = LAMPORTS_PER_SOL;
    // 0.066 SOL/day * 30 = 1.98 -> 2.0
    assert_eq!(
        proposal_lamports(66_000_000.0, 30.0, 5 * sol, 14 * sol),
        2 * sol
    );
    // 0.026 * 30 = 0.78 -> 1.0 (minimum)
    assert_eq!(
        proposal_lamports(26_000_000.0, 30.0, 5 * sol, 14 * sol),
        sol
    );
    // 0.1 * 30 = 3 plus a 1.2 SOL deficit -> 4.2 -> 4.5
    assert_eq!(
        proposal_lamports(100_000_000.0, 30.0, 5 * sol, 3_800_000_000),
        4_500_000_000
    );
    assert_eq!(proposal_lamports(5e9, 30.0, 5 * sol, 0), 20 * sol);
    assert_eq!(proposal_lamports(0.0, 30.0, 5 * sol, 14 * sol), sol);
    assert_eq!(runway_days(14 * sol, 5 * sol, 0.0), f64::INFINITY);
    assert_eq!(runway_days(4 * sol, 5 * sol, 1.0), 0.0);
    assert!((runway_days(15 * sol, 5 * sol, 1e9) - 10.0).abs() < 1e-9);
}

#[test]
fn live_bond_is_healthy_and_reports_burn_and_notes() {
    let source = StaticSource::live(0);
    let assessment = assess(&source, &config(Pubkey::new_unique()));
    assert!(matches!(assessment.decision, SquadsDecision::Healthy));
    assert_eq!(assessment.burn.source, BurnSource::Observed);
    assert_eq!(assessment.burn.claims, 6);
    assert!(assessment.runway_days > 100.0, "{}", assessment.runway_days);
    assert!(assessment.pending.is_none());
    assert!(
        assessment.ours.is_empty(),
        "the live index 3 is a plain withdraw"
    );
    assert_eq!(*source.scanned_indexes.borrow(), vec![3, 2, 1]);
    assert!(source.simulations.borrow().is_empty());
    assert_eq!(
        assessment.vote_withdrawable_lamports,
        59_722_794_811 - (3_762 + 128) * RENT_PER_BYTE
    );
    let rendered = render_squads_assessment(&assessment, NOW);
    assert!(
        rendered.contains("decision:           no proposal"),
        "{rendered}"
    );
    assert!(
        rendered.contains("note:               proposer"),
        "{rendered}"
    );
    assert!(rendered.contains("burn:               0.0"), "{rendered}");
}

#[test]
fn forced_proposal_shows_instructions_and_refuses_non_member() {
    let source = StaticSource::live(0);
    let assessment = assess(&source, &forced(Pubkey::new_unique()));
    let SquadsDecision::Refused(reason) = &assessment.decision else {
        panic!("expected refusal, got {:?}", assessment.decision);
    };
    assert!(reason.contains("is not a member of multisig"), "{reason}");
    assert!(reason.contains("balance 0 SOL"), "{reason}");
    let plan = assessment.blocked_proposal.as_ref().expect("plan shown");
    assert_eq!(plan.transaction_index, 4);
    assert_eq!(plan.topup_lamports, LAMPORTS_PER_SOL);
    assert_eq!(plan.vault_instructions.len(), 4);
    assert_eq!(
        vote_withdraw_lamports(
            &plan.vault_instructions[0].program_id,
            &plan.vault_instructions[0].data
        ),
        Some(LAMPORTS_PER_SOL)
    );
    assert_eq!(
        plan.topup_jsol,
        plan.quote.minted_to_destination * 9_900 / 10_000
    );
    assert_eq!(
        plan.vault_instructions[3].data[8..16],
        plan.topup_jsol.to_le_bytes()
    );
    // The vault side passed, so the vault transaction was simulated.
    assert_eq!(source.simulations.borrow().len(), 1);
    assert_eq!(source.simulations.borrow()[0], plan.vault_instructions);
    assert!(plan.transaction_size <= MAX_TRANSACTION_SIZE);
    let rendered = render_squads_assessment(&assessment, NOW);
    for needle in [
        "decision:           propose 1 SOL",
        "  1. Vote Withdraw 1 SOL",
        "  2. ATA CreateIdempotent",
        "  3. JPool DepositSol 1 SOL",
        "  4. bond_top_up",
        "vault simulation:   ok",
        "refused:            proposer",
    ] {
        assert!(rendered.contains(needle), "missing {needle} in\n{rendered}");
    }
}

#[test]
fn member_with_balance_gets_an_exact_proposal() {
    let source = StaticSource::live(50_000_000);
    let assessment = assess(&source, &forced(MEMBER));
    let SquadsDecision::Propose(plan) = &assessment.decision else {
        panic!("expected a proposal, got {:?}", assessment.decision);
    };
    assert_eq!(plan.memo, "jpool-bond-rs top-up 1 SOL");
    let args = VaultTransactionCreateArgs::decode(&plan.instructions[0].data).unwrap();
    assert_eq!(args.vault_index, 0);
    assert_eq!(args.ephemeral_signers, 0);
    assert_eq!(args.memo.as_deref(), Some("jpool-bond-rs top-up 1 SOL"));
    assert_eq!(
        VaultMessage::decode_transaction_message(&args.transaction_message).unwrap(),
        plan.message
    );
    assert_eq!(plan.message.num_signers, 1);
    assert_eq!(plan.message.account_keys[0], VAULT);
    assert_eq!(plan.instructions[0].accounts[1].pubkey, plan.transaction);
    assert_eq!(plan.instructions[1].accounts[1].pubkey, plan.proposal);
    assert_eq!(plan.instructions[1].data[8..16], 4_u64.to_le_bytes());
    assert_eq!(plan.instructions[1].data[16], 0, "not a draft");
    assert_eq!(
        plan.required_proposer_lamports,
        plan.vault_transaction_rent + plan.proposal_rent + plan.fee_buffer_lamports + 10_000_000
    );
    assert!(is_our_topup(&plan.message, &VALIDATOR_BOND));
    assert_eq!(message_amount(&plan.message), Some(LAMPORTS_PER_SOL));

    // Post-submit verification accepts exactly what was planned.
    let addresses = assessment.addresses;
    let created = vault_transaction_account(4, MEMBER, &plan.message);
    let active = proposal_account(4, 1, NOW);
    check_created_proposal(&addresses, plan, &active, &created).expect("verified");
    assert!(
        check_created_proposal(&addresses, plan, &proposal_account(4, 0, NOW), &created).is_err()
    );
    let other = vault_transaction_account(4, MEMBER, &our_topup_message(VALIDATOR_BOND, 1));
    assert!(check_created_proposal(&addresses, plan, &active, &other).is_err());
    let foreign_creator = vault_transaction_account(4, VAULT, &plan.message);
    assert!(check_created_proposal(&addresses, plan, &active, &foreign_creator).is_err());
}

#[test]
fn open_proposal_of_ours_blocks_a_new_one() {
    for status_tag in [0, 1, 3] {
        let mut source = StaticSource::live(50_000_000);
        with_our_proposal(&mut source, status_tag, NOW - 7_200);
        let assessment = assess(&source, &forced(MEMBER));
        assert!(
            matches!(assessment.decision, SquadsDecision::Pending),
            "{status_tag}: {:?}",
            assessment.decision
        );
        let pending = assessment.pending.as_ref().unwrap();
        assert_eq!(pending.index, 3);
        assert_eq!(pending.amount_lamports, Some(2_500_000_000));
        assert_eq!(pending.created_at, Some(NOW - 3_600));
        assert!(source.simulations.borrow().is_empty());
        let rendered = render_squads_assessment(&assessment, NOW);
        assert!(rendered.contains("#3 is still open"), "{rendered}");
        assert!(rendered.contains("age 3600s"), "{rendered}");
    }
}

#[test]
fn stale_or_settled_proposals_are_not_pending() {
    let mut source = StaticSource::live(50_000_000);
    with_our_proposal(&mut source, 1, NOW - 7_200);
    // stale_transaction_index sits at 8 + 32 + 32 + 2 + 4 + 8.
    let data = &mut source.snapshot.multisig.as_mut().unwrap().data;
    data[86..94].copy_from_slice(&3_u64.to_le_bytes());
    let assessment = assess(&source, &forced(MEMBER));
    assert!(assessment.pending.is_none());
    assert!(matches!(assessment.decision, SquadsDecision::Propose(_)));

    let mut source = StaticSource::live(50_000_000);
    with_our_proposal(&mut source, 6, NOW - 7_200);
    let assessment = assess(&source, &forced(MEMBER));
    assert!(assessment.pending.is_none());
    assert_eq!(assessment.ours.len(), 1);
    assert_eq!(assessment.last_created_at(), Some(NOW - 3_600));
}

#[test]
fn recent_execution_or_increase_refuses_by_cooldown() {
    let mut source = StaticSource::live(50_000_000);
    with_our_proposal(&mut source, 5, NOW - 3_600);
    let assessment = assess(&source, &forced(MEMBER));
    assert!(
        matches!(&assessment.decision, SquadsDecision::Refused(reason) if reason.contains("our proposal #3 was executed")),
        "{:?}",
        assessment.decision
    );

    let mut source = StaticSource::live(50_000_000);
    source.history.changes.insert(
        0,
        BondJsolChange {
            signature: Signature::default(),
            block_time: Some(NOW - 600),
            delta: 1_000_000_000,
        },
    );
    let assessment = assess(&source, &forced(MEMBER));
    assert!(
        matches!(&assessment.decision, SquadsDecision::Refused(reason) if reason.contains("the bond JSOL grew")),
        "{:?}",
        assessment.decision
    );
    assert!(source.simulations.borrow().is_empty());
}

#[test]
fn vote_withdrawable_and_simulation_guards_refuse() {
    let mut source = StaticSource::live(50_000_000);
    source.snapshot.vote_account.as_mut().unwrap().lamports = 500_000_000;
    let assessment = assess(&source, &forced(MEMBER));
    assert!(
        matches!(&assessment.decision, SquadsDecision::Refused(reason) if reason.contains("withdrawable")),
        "{:?}",
        assessment.decision
    );
    assert!(source.simulations.borrow().is_empty());

    let mut source = StaticSource::live(50_000_000);
    source.simulation_error = Some("custom program error: 0x1".into());
    let assessment = assess(&source, &forced(MEMBER));
    assert!(
        matches!(&assessment.decision, SquadsDecision::Refused(reason) if reason.contains("vault transaction simulation failed")),
        "{:?}",
        assessment.decision
    );
}

#[test]
fn floor_deficit_and_fallback_burn_size_the_proposal() {
    let mut source = StaticSource::live(50_000_000);
    source.history = BondHistory::default();
    let mut config = config(MEMBER);
    config.floor_lamports = 20 * LAMPORTS_PER_SOL;
    let assessment = assess(&source, &config);
    assert!(matches!(assessment.burn.source, BurnSource::Fallback(_)));
    assert_eq!(assessment.runway_days, 0.0);
    let SquadsDecision::Propose(plan) = &assessment.decision else {
        panic!("expected a proposal, got {:?}", assessment.decision);
    };
    // 0.1 * 30 = 3 SOL plus the deficit to 20 SOL, rounded up to 0.5.
    let deficit = 20 * LAMPORTS_PER_SOL - assessment.bond_value_lamports;
    let expected =
        (3 * LAMPORTS_PER_SOL + deficit).div_ceil(PROPOSAL_STEP_LAMPORTS) * PROPOSAL_STEP_LAMPORTS;
    assert_eq!(plan.topup_lamports, expected);
    let rendered = render_squads_assessment(&assessment, NOW);
    assert!(rendered.contains("critical:"), "{rendered}");
    assert!(rendered.contains(&format!("propose {} SOL", format_sol(expected))));
}

#[test]
fn wrong_withdrawer_and_foreign_multisig_are_errors() {
    let mut source = StaticSource::live(0);
    source.snapshot.vote_account.as_mut().unwrap().data[36..68].copy_from_slice(MEMBER.as_ref());
    let error = plan_squads(&source, &config(MEMBER), NOW).unwrap_err();
    assert!(
        error.to_string().contains("expected Squads vault"),
        "{error}"
    );

    let mut source = StaticSource::live(0);
    source.snapshot.multisig.as_mut().unwrap().owner = Pubkey::new_unique();
    assert!(plan_squads(&source, &config(MEMBER), NOW).is_err());
}

#[test]
fn foreign_bond_topups_are_not_ours() {
    let message = our_topup_message(Pubkey::new_unique(), 1);
    assert!(!is_our_topup(&message, &VALIDATOR_BOND));
    assert!(is_our_topup(
        &our_topup_message(VALIDATOR_BOND, 1),
        &VALIDATOR_BOND
    ));
}
