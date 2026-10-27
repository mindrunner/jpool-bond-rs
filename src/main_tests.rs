use super::*;
use jpool_bond_rs::{
    ChainAccount, ChainSnapshot,
    bond::BOND_PROGRAM_ID,
    config::LAMPORTS_PER_SOL,
    history::RecentTopUp,
    pool::{JPOOL_STAKE_POOL, STAKE_POOL_PROGRAM_ID, decode_jpool},
    token::token_program_id,
};
use solana_pubkey::{Pubkey, pubkey};
use std::cell::{Cell, RefCell};

const VOTE: Pubkey = pubkey!("steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr");

#[test]
fn parses_explicit_plan_and_run_commands() {
    for name in ["plan", "run"] {
        let cli =
            Cli::try_parse_from(["jpool-bond-rs", name, "--config", "/etc/jpool/config.toml"])
                .expect("parse command");
        match cli.command {
            Command::Plan { config } | Command::Run { config } => {
                assert_eq!(config, PathBuf::from("/etc/jpool/config.toml"));
            }
        }
    }
    assert!(Cli::try_parse_from(["jpool-bond-rs", "--config", "/etc/jpool/config.toml"]).is_err());
}

fn account(owner: Pubkey, data: &[u8]) -> ChainAccount {
    ChainAccount {
        owner,
        lamports: 1,
        data: data.to_vec(),
    }
}

/// Live mainnet accounts; the identity JSOL ATA is absent so any identity
/// works.
fn live_snapshot() -> ChainSnapshot {
    let pool = account(
        STAKE_POOL_PROGRAM_ID,
        include_bytes!("../tests/fixtures/mainnet_jpool_stake_pool.bin"),
    );
    ChainSnapshot {
        current_epoch: decode_jpool(&JPOOL_STAKE_POOL, &pool)
            .unwrap()
            .last_update_epoch(),
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
        identity_token_account: None,
        identity_lamports: 78 * LAMPORTS_PER_SOL,
        token_account_rent_lamports: Some(2_039_280),
    }
}

struct StaticSource {
    recent: Option<RecentTopUp>,
}

impl ChainDataSource for StaticSource {
    fn snapshot(&self, _addresses: &Addresses) -> Result<ChainSnapshot> {
        Ok(live_snapshot())
    }

    fn recent_identity_topup(
        &self,
        _addresses: &Addresses,
        _since_unix: i64,
    ) -> Result<Option<RecentTopUp>> {
        Ok(self.recent.clone())
    }
}

#[derive(Default)]
struct RecordingExecutor {
    submitted: RefCell<Vec<Vec<Instruction>>>,
    verify_calls: Cell<usize>,
}

impl Executor for RecordingExecutor {
    fn submit(&self, payer: &Keypair, instructions: &[Instruction]) -> Result<Signature> {
        self.submitted.borrow_mut().push(instructions.to_vec());
        assert!(instructions[2].accounts[2].pubkey == payer.pubkey());
        Ok(Signature::from([9; 64]))
    }

    fn verify(&self, _addresses: &Addresses, before_jsol: u64, topup_jsol: u64) -> Result<u64> {
        self.verify_calls.set(self.verify_calls.get() + 1);
        Ok(before_jsol + topup_jsol)
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    config: Config,
}

fn fixture(min_value_sol: u64, keypair: Option<&Keypair>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let keypair_path = dir.path().join("identity.json");
    let identity = match keypair {
        Some(keypair) => {
            std::fs::write(&keypair_path, format!("{:?}", keypair.to_bytes().to_vec())).unwrap();
            keypair.pubkey()
        }
        None => Pubkey::new_unique(),
    };
    let config = Config {
        rpc_url: "https://rpc.example.test".into(),
        vote_account: VOTE,
        identity_pubkey: identity,
        identity_keypair_path: keypair_path,
        bond_name: "perf".into(),
        min_value_lamports: min_value_sol * LAMPORTS_PER_SOL,
        topup_lamports: 5 * LAMPORTS_PER_SOL,
        identity_reserve_lamports: 10 * LAMPORTS_PER_SOL,
        priority_fee_micro_lamports: 1_000,
        metrics_file: dir.path().join("jpool_bond.prom"),
    };
    Fixture { dir, config }
}

fn metrics(fixture: &Fixture) -> String {
    std::fs::read_to_string(&fixture.config.metrics_file).unwrap()
}

#[test]
fn plan_never_loads_the_keypair_even_when_topup_is_due() {
    let fixture = fixture(20, None);
    let executor = RecordingExecutor::default();
    execute(
        Mode::Plan,
        &fixture.config,
        &FileMetricsSink,
        &StaticSource { recent: None },
        &executor,
        1_800_000_000,
    )
    .expect("plan succeeds without a keypair");
    assert!(executor.submitted.borrow().is_empty());
    let body = metrics(&fixture);
    assert!(body.contains(
        "jpool_bond_last_run_success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 1"
    ));
    assert!(body.contains(
        "jpool_bond_jsol{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 11.001704341"
    ));
    drop(fixture.dir);
}

#[test]
fn run_checks_the_keypair_even_when_nothing_is_due() {
    let fixture = fixture(8, None);
    let executor = RecordingExecutor::default();
    let error = execute(
        Mode::Run,
        &fixture.config,
        &FileMetricsSink,
        &StaticSource { recent: None },
        &executor,
        1_800_000_000,
    )
    .expect_err("missing keypair");
    assert!(
        error
            .to_string()
            .contains("failed to read identity keypair")
    );
    assert!(metrics(&fixture).contains(
        "jpool_bond_last_run_success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 0"
    ));
}

#[test]
fn run_rejects_a_keypair_for_another_identity() {
    let keypair = Keypair::new();
    let mut fixture = fixture(20, Some(&keypair));
    fixture.config.identity_pubkey = Pubkey::new_unique();
    let executor = RecordingExecutor::default();
    let error = execute(
        Mode::Run,
        &fixture.config,
        &FileMetricsSink,
        &StaticSource { recent: None },
        &executor,
        1_800_000_000,
    )
    .expect_err("mismatched keypair");
    assert!(error.to_string().contains("expected identity_pubkey"));
    assert!(executor.submitted.borrow().is_empty());
}

#[test]
fn run_submits_once_verifies_and_records_topup() {
    let keypair = Keypair::new();
    let fixture = fixture(20, Some(&keypair));
    let executor = RecordingExecutor::default();
    execute(
        Mode::Run,
        &fixture.config,
        &FileMetricsSink,
        &StaticSource { recent: None },
        &executor,
        1_800_000_000,
    )
    .expect("top-up");
    assert_eq!(executor.submitted.borrow().len(), 1);
    assert_eq!(executor.verify_calls.get(), 1);
    let body = metrics(&fixture);
    assert!(body.contains(
        "jpool_bond_last_topup_sol{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 5"
    ));
    assert!(body.contains(
        "jpool_bond_last_run_success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 1"
    ));
}

#[test]
fn refusal_exits_with_error_and_never_submits() {
    let keypair = Keypair::new();
    let fixture = fixture(20, Some(&keypair));
    let executor = RecordingExecutor::default();
    let source = StaticSource {
        recent: Some(RecentTopUp {
            signature: Signature::from([1; 64]),
            block_time: Some(1_800_000_000 - 60),
        }),
    };
    let error = execute(
        Mode::Run,
        &fixture.config,
        &FileMetricsSink,
        &source,
        &executor,
        1_800_000_000,
    )
    .expect_err("cooldown refusal");
    assert!(error.to_string().starts_with("refused:"), "{error}");
    assert!(executor.submitted.borrow().is_empty());
    let body = metrics(&fixture);
    assert!(body.contains(
        "jpool_bond_last_run_success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 0"
    ));
    // Values stay visible on refusal.
    assert!(body.contains("jpool_bond_value_sol{"));
}

#[test]
fn invalid_config_writes_failure_metrics_from_hint() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let metrics_file = dir.path().join("jpool_bond.prom");
    std::fs::write(
        &config_path,
        format!(
            "metrics_file = \"{}\"\nvote_account = \"{VOTE}\"\n",
            metrics_file.display()
        ),
    )
    .unwrap();
    let error = run_cli(
        Cli {
            command: Command::Plan {
                config: config_path,
            },
        },
        &FileMetricsSink,
    )
    .expect_err("missing required fields");
    assert!(format!("{error:#}").contains("rpc_url"), "{error:#}");
    let body = std::fs::read_to_string(&metrics_file).unwrap();
    assert!(body.contains(
        "jpool_bond_last_run_success{vote=\"steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr\"} 0"
    ));
}
