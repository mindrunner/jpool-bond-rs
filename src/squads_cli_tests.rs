use super::*;
use jpool_bond_rs::{
    ChainAccount,
    bond::BOND_PROGRAM_ID,
    burn::{BondHistory, BondJsolChange},
    config::{LAMPORTS_PER_SOL, SquadsSettings},
    metrics::FileMetricsSink,
    pool::{JPOOL_STAKE_POOL, STAKE_POOL_PROGRAM_ID, decode_jpool},
    squads::{SQUADS_PROGRAM_ID, SYSTEM_PROGRAM_ID},
    squads_mode::{ScannedTransaction, SquadsSnapshot},
    token::token_program_id,
    transaction::SimulationResult,
    vote::VOTE_PROGRAM_ID,
};
use solana_pubkey::{Pubkey, pubkey};
use std::cell::{Cell, RefCell};

const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");
const MULTISIG: Pubkey = pubkey!("De838vwWHcbAyNWTTKeKPyviMNHaX91CrD2igGDfZZdM");
const NOW: u64 = 1_793_614_296;
const LABEL: &str = "{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"}";
/// First member key in the multisig account: 8 + 32 + 32 + 2 + 4 + 8 + 8
/// + 1 (rent_collector None) + 1 (bump) + 4 (members length).
const FIRST_MEMBER_OFFSET: usize = 100;

fn account(owner: Pubkey, lamports: u64, data: &[u8]) -> ChainAccount {
    ChainAccount {
        owner,
        lamports,
        data: data.to_vec(),
    }
}

/// Live mainnet accounts; `member` replaces the first multisig member.
struct StaticSource {
    member: Option<Pubkey>,
    proposer_lamports: u64,
}

impl SquadsDataSource for StaticSource {
    fn snapshot(&self, _addresses: &SquadsAddresses) -> Result<SquadsSnapshot> {
        let pool = account(
            STAKE_POOL_PROGRAM_ID,
            1,
            include_bytes!("../tests/fixtures/mainnet_jpool_stake_pool.bin"),
        );
        let mut multisig = include_bytes!("../tests/fixtures/mainnet_squads_multisig.bin").to_vec();
        if let Some(member) = self.member {
            multisig[FIRST_MEMBER_OFFSET..FIRST_MEMBER_OFFSET + 32]
                .copy_from_slice(member.as_ref());
        }
        Ok(SquadsSnapshot {
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
            multisig: Some(account(SQUADS_PROGRAM_ID, 1, &multisig)),
            vote_account: Some(account(
                VOTE_PROGRAM_ID,
                59_722_794_811,
                include_bytes!("../tests/fixtures/mainnet_vote_account.bin"),
            )),
            vault: Some(account(SYSTEM_PROGRAM_ID, 2_027_435_128, &[])),
            vault_token_account: None,
            proposer_lamports: self.proposer_lamports,
        })
    }

    fn rent_exempt_minimum(&self, data_len: usize) -> Result<u64> {
        Ok((data_len as u64 + 128) * 5_080)
    }

    fn scan_transactions(
        &self,
        _multisig: &Pubkey,
        indexes: &[u64],
    ) -> Result<Vec<ScannedTransaction>> {
        let squads = |data: &[u8]| Some(account(SQUADS_PROGRAM_ID, 1, data));
        Ok(indexes
            .iter()
            .map(|index| ScannedTransaction {
                index: *index,
                proposal: match index {
                    3 => squads(include_bytes!(
                        "../tests/fixtures/mainnet_squads_proposal_3.bin"
                    )),
                    _ => None,
                },
                transaction: match index {
                    3 => squads(include_bytes!(
                        "../tests/fixtures/mainnet_squads_vault_tx_3.bin"
                    )),
                    _ => None,
                },
            })
            .collect())
    }

    fn creation_time(&self, _account: &Pubkey) -> Result<Option<i64>> {
        Ok(None)
    }

    fn bond_history(
        &self,
        _addresses: &SquadsAddresses,
        _window_start: i64,
    ) -> Result<BondHistory> {
        let now = NOW as i64;
        Ok(BondHistory {
            changes: (1..=10)
                .map(|day| BondJsolChange {
                    signature: Signature::default(),
                    block_time: Some(now - day * 86_400),
                    delta: -50_000_000,
                })
                .collect(),
            covers_window: true,
            oldest_in_window: Some(now - 10 * 86_400),
        })
    }

    fn simulate_vault_transaction(
        &self,
        _vault: &Pubkey,
        _instructions: &[Instruction],
    ) -> Result<SimulationResult> {
        Ok(SimulationResult {
            units_consumed: Some(100_000),
            error: None,
            logs: Vec::new(),
        })
    }
}

#[derive(Default)]
struct RecordingExecutor {
    submitted: RefCell<Vec<Vec<Instruction>>>,
    verify_calls: Cell<usize>,
}

impl SquadsExecutor for RecordingExecutor {
    fn submit(&self, payer: &Keypair, instructions: &[Instruction]) -> Result<Signature> {
        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[0].accounts[2].pubkey, payer.pubkey());
        self.submitted.borrow_mut().push(instructions.to_vec());
        Ok(Signature::from([9; 64]))
    }

    fn verify(&self, _addresses: &SquadsAddresses, plan: &ProposalPlan) -> Result<()> {
        assert_eq!(plan.transaction_index, 4);
        self.verify_calls.set(self.verify_calls.get() + 1);
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: SquadsConfig,
}

fn fixture(propose_runway_days: f64, keypair: Option<&Keypair>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let keypair_path = dir.path().join("proposer.json");
    let proposer = match keypair {
        Some(keypair) => {
            std::fs::write(&keypair_path, format!("{:?}", keypair.to_bytes().to_vec())).unwrap();
            keypair.pubkey()
        }
        None => Pubkey::new_unique(),
    };
    let config = SquadsConfig {
        rpc_url: "https://rpc.example.test".into(),
        vote_account: VOTE,
        bond_name: "perf".into(),
        priority_fee_micro_lamports: 1_000,
        metrics_file: dir.path().join("jpool_bond.prom"),
        floor_lamports: 5 * LAMPORTS_PER_SOL,
        propose_runway_days,
        topup_days: 30.0,
        critical_runway_days: 3.0,
        burn_lookback_days: 30,
        burn_min_span_days: 3.0,
        burn_fallback_lamports_per_day: 100_000_000,
        squads: SquadsSettings {
            multisig: MULTISIG,
            vault_index: 0,
            proposer_pubkey: proposer,
            proposer_keypair_path: keypair_path,
            proposer_reserve_lamports: 10_000_000,
            pending_scan: 20,
            jsol_margin_bps: 100,
        },
    };
    Fixture { _dir: dir, config }
}

fn metrics(fixture: &Fixture) -> String {
    std::fs::read_to_string(&fixture.config.metrics_file).unwrap()
}

fn run(
    mode: Mode,
    fixture: &Fixture,
    source: &StaticSource,
    executor: &RecordingExecutor,
) -> Result<()> {
    execute_squads(
        mode,
        &fixture.config,
        &FileMetricsSink,
        source,
        executor,
        NOW,
    )
}

#[test]
fn healthy_plan_succeeds_without_a_keypair_and_writes_metrics() {
    let fixture = fixture(10.0, None);
    let executor = RecordingExecutor::default();
    let source = StaticSource {
        member: None,
        proposer_lamports: 0,
    };
    run(Mode::Plan, &fixture, &source, &executor).expect("healthy plan");
    let body = metrics(&fixture);
    for line in [
        format!("jpool_bond_last_run_success{LABEL} 1"),
        format!("jpool_bond_proposal_pending{LABEL} 0"),
        format!("jpool_bond_floor_sol{LABEL} 5"),
        format!("jpool_bond_propose_runway_days{LABEL} 10.000"),
        format!("jpool_bond_critical_runway_days{LABEL} 3.000"),
        format!("jpool_bond_proposer_balance_sol{LABEL} 0"),
        format!("jpool_bond_jsol{LABEL} 11.001704341"),
    ] {
        assert!(body.contains(&line), "missing {line} in\n{body}");
    }
    assert!(body.contains("jpool_bond_burn_sol_per_day{"));
    assert!(body.contains("jpool_bond_runway_days{"));
    assert!(body.contains("jpool_bond_vote_withdrawable_sol{"));
    assert!(!body.contains("jpool_bond_proposal_index"));
}

#[test]
fn plan_never_loads_the_keypair_even_when_a_proposal_is_due() {
    let fixture = fixture(1000.0, None);
    let executor = RecordingExecutor::default();
    let member = fixture.config.squads.proposer_pubkey;
    let source = StaticSource {
        member: Some(member),
        proposer_lamports: 50_000_000,
    };
    run(Mode::Plan, &fixture, &source, &executor).expect("plan succeeds without a keypair");
    assert!(executor.submitted.borrow().is_empty());
    assert!(metrics(&fixture).contains(&format!("jpool_bond_last_run_success{LABEL} 1")));
}

#[test]
fn due_proposal_by_a_non_member_is_refused_with_failure_metrics() {
    let keypair = Keypair::new();
    let fixture = fixture(1000.0, Some(&keypair));
    let executor = RecordingExecutor::default();
    let source = StaticSource {
        member: None,
        proposer_lamports: 50_000_000,
    };
    let error = run(Mode::Run, &fixture, &source, &executor).expect_err("refused");
    assert!(error.to_string().starts_with("refused:"), "{error}");
    assert!(error.to_string().contains("is not a member"), "{error}");
    assert!(executor.submitted.borrow().is_empty());
    let body = metrics(&fixture);
    assert!(body.contains(&format!("jpool_bond_last_run_success{LABEL} 0")));
    assert!(body.contains("jpool_bond_runway_days{"));
}

#[test]
fn run_checks_the_proposer_key_even_when_nothing_is_due() {
    let fixture = fixture(10.0, None);
    let executor = RecordingExecutor::default();
    let source = StaticSource {
        member: None,
        proposer_lamports: 0,
    };
    let error = run(Mode::Run, &fixture, &source, &executor).expect_err("missing key");
    assert!(
        error
            .to_string()
            .contains("failed to read proposer keypair")
    );
    assert!(metrics(&fixture).contains(&format!("jpool_bond_last_run_success{LABEL} 0")));

    let keypair = Keypair::new();
    let mut fixture = self::fixture(10.0, Some(&keypair));
    fixture.config.squads.proposer_pubkey = Pubkey::new_unique();
    let error = run(Mode::Run, &fixture, &source, &executor).expect_err("wrong key");
    assert!(
        error
            .to_string()
            .contains("expected squads.proposer_pubkey")
    );
}

#[test]
fn run_creates_one_proposal_verifies_it_and_records_metrics() {
    let keypair = Keypair::new();
    let fixture = fixture(1000.0, Some(&keypair));
    let executor = RecordingExecutor::default();
    let source = StaticSource {
        member: Some(keypair.pubkey()),
        proposer_lamports: 50_000_000,
    };
    run(Mode::Run, &fixture, &source, &executor).expect("proposal");
    assert_eq!(executor.submitted.borrow().len(), 1);
    assert_eq!(executor.verify_calls.get(), 1);
    let body = metrics(&fixture);
    for line in [
        format!("jpool_bond_last_run_success{LABEL} 1"),
        format!("jpool_bond_proposal_pending{LABEL} 1"),
        format!("jpool_bond_proposal_index{LABEL} 4"),
        format!("jpool_bond_proposal_age_seconds{LABEL} 0"),
        format!("jpool_bond_last_proposal_created_timestamp_seconds{LABEL} {NOW}"),
    ] {
        assert!(body.contains(&line), "missing {line} in\n{body}");
    }
    assert!(body.contains("jpool_bond_proposal_amount_sol{"));
    assert!(!body.contains("jpool_bond_proposer_balance_sol"));
}

#[test]
fn invalid_squads_config_writes_squads_failure_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let metrics_file = dir.path().join("jpool_bond.prom");
    std::fs::write(
        &config_path,
        format!(
            "metrics_file = \"{}\"\nvote_account = \"{VOTE}\"\n[squads]\nmultisig = \"{MULTISIG}\"\n",
            metrics_file.display()
        ),
    )
    .unwrap();
    let error = crate::run_cli(
        crate::Cli {
            command: crate::Command::Plan {
                config: config_path,
            },
        },
        &FileMetricsSink,
    )
    .expect_err("missing required fields");
    assert!(format!("{error:#}").contains("missing field"), "{error:#}");
    let body = std::fs::read_to_string(&metrics_file).unwrap();
    assert!(body.contains(&format!("jpool_bond_last_run_success{LABEL} 0")));
    assert!(!body.contains("jpool_bond_last_topup_sol"));
}
