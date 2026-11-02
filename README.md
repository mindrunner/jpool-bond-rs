# jpool-bond-rs

Keeps a validator's JPool performance bond (JSOL collateral) above a floor.
Built to run as a daily one-shot from a systemd timer, but works standalone.

Two modes, chosen by the config file:

- **Direct mode** (v0.1): a hot identity wallet tops up the bond itself.
- **Squads mode** (a `[squads]` table in the config): the tool creates a
  Squads v4 proposal that withdraws from the vote account and tops up the
  bond. The owner approves and executes it in the Squads UI. No hot wallet
  holds the money; the tool's key can only propose.

## Direct mode

### How it works

Each run derives every account from the config, reads them in one batch, and
values the bond: the JSOL in the bond's token account times the JPool rate
(`total_lamports / pool_token_supply` from the stake pool account).

If the value is at or above `min_value_sol`, nothing happens. If it is below,
the run tops up `topup_sol` SOL in one atomic transaction:

1. create the identity's JSOL ATA (idempotent; it is the staging account),
2. JPool `DepositSol` of `topup_sol` from the identity into that ATA,
3. bond program `bond_top_up` with exactly the JSOL that deposit mints.

The minted amount is computed with the stake pool program's own math (SOL
deposit fee, referral fee credited back because the identity's ATA is also the
referrer). The transaction is simulated first, then sent, confirmed, and the
bond's JSOL account is re-read: the run only succeeds if it grew by exactly
the topped-up amount.

`plan` and `run` are separate subcommands:

- `plan` does RPC reads, prints the decision and writes metrics. It never
  loads the keypair, and nothing is signed or sent.
- `run` does the same, then loads `identity_keypair_path` (also when no
  top-up is due, so a broken key shows up early), checks it matches
  `identity_pubkey`, and submits if a top-up is due.

Submission gets up to three attempts with fresh blockhashes. A retry is only
allowed after proving the previous transaction expired unobserved; anything
ambiguous aborts instead of risking a double top-up.

### Guards

Every guard failure is a refusal: the run exits non-zero, the message names
the actual values, and `jpool_bond_last_run_success` is 0.

- `bond_state` and `validator_bond` exist and are owned by the bond program
  `BondQ7KqZreTcW2UbeTNDcLCJQ3aXAtLn2Fm6ftaJDU` with the right discriminators.
- `bond_state` is a Standard bond named `bond_name` with collateral
  `Token(JSOL)`; a top-up also needs status Active.
- `validator_bond.vote_account` is `vote_account` and `validator_bond.state`
  is the derived `bond_state`.
- The bond JSOL account is the ATA of `validator_bond` for the JSOL mint.
- The stake pool is JPool `CtMyWsrUtAwXWiGr9WjHT5fC3p3fgV8cyGpLTo2LJzG1`,
  owned by `SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy`, with the JSOL mint,
  the known reserve and manager fee account, no SOL deposit authority, and
  updated for the current epoch.
- The identity keypair matches `identity_pubkey` (`run` only).
- The identity keeps at least `identity_reserve_sol` after the top-up, the
  fee buffer for all attempts, and ATA rent if the ATA is missing.
- At most one top-up per 20 hours, enforced from chain history: the
  signatures of the bond JSOL account are scanned for a successful
  transaction in the window that the identity signed and that carries a
  `bond_top_up`. No local state, so a crash mid-run cannot cause a second
  top-up.

Hard limits (not raisable via config): `topup_sol` at most 10,
`min_value_sol` at most 50, `identity_reserve_sol` at least 1,
`priority_fee_micro_lamports` at most 1000000, three attempts.

### Configuration

```toml
rpc_url = "<vaulted premium rpc_url>"
vote_account = "steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr"
identity_pubkey = "EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej"
identity_keypair_path = "/etc/jpool-bond/identity.json"
bond_name = "perf"                    # optional, default "perf"
min_value_sol = 8
topup_sol = 5
identity_reserve_sol = 10             # optional, default 10
priority_fee_micro_lamports = 1000
metrics_file = "/var/lib/alloy/textcollector/jpool_bond.prom"
```

SOL amounts take integers or decimals. Unknown keys are rejected, so the
Squads-mode keys below are an error here. Key material only ever comes from
the keypair file.

### Metrics

Every run, in both modes, atomically replaces a Prometheus textfile. All
series carry a `vote` label. Direct mode writes:

| Metric | Meaning |
| --- | --- |
| `jpool_bond_value_sol` | SOL value of the bond's JSOL |
| `jpool_bond_jsol` | JSOL in the bond |
| `jpool_bond_jsol_sol_rate` | SOL per JSOL |
| `jpool_bond_min_value_sol` | configured floor |
| `jpool_bond_identity_balance_sol` | identity SOL balance |
| `jpool_bond_last_topup_sol` | SOL topped up by this run, 0 if none |
| `jpool_bond_last_run_success` | 1 on success, 0 on any error or refusal |
| `jpool_bond_last_run_timestamp_seconds` | unix time of the run |

Values the run could not read are left out rather than written as 0. The
metrics path is write-tested before any chain access.

## Squads mode

### Flow

Each run reads the bond, the JPool stake pool, the multisig, the vote
account and the vault in one batch, then:

1. **Burn estimate.** The bond JSOL account's history over
   `burn_lookback_days` is fetched (`getSignaturesForAddress` plus each
   transaction's pre/post token balances). Every transaction that lowered
   the bond's JSOL counts as a claim; increases (top-ups) are ignored. The
   outflow is divided by the span from the window start to now, or from
   the first observed transaction when the history is shorter, and
   converted to SOL at the current pool rate. With less than
   `burn_min_span_days` of history, `burn_fallback_sol_per_day` is used and
   the output says so.
2. **Runway.** `runway_days = (bond_value_sol - floor_sol) / burn_sol_per_day`
   (0 at or below the floor, infinite with no burn).
3. **Pending scan.** Proposals of the multisig from `transaction_index`
   down over `pending_scan` indexes are read (one `getMultipleAccounts` for
   the Proposal and VaultTransaction PDAs). A proposal is ours when its
   vault transaction carries a JPool `bond_top_up` for our validator bond.
   Ours is pending while Draft or Active (and not stale) or Approved.
4. **Decision.** Nothing happens while `runway_days >= propose_runway_days`.
   Below it, a pending proposal of ours means no new one (the run still
   succeeds and reports its index and age). Otherwise one proposal of
   `X = burn_sol_per_day * topup_days + max(0, floor_sol - bond_value_sol)`
   SOL is built, rounded up to 0.5 SOL, at least 1 SOL, at most 20 SOL.

The proposal is one transaction signed by the proposer key (creator and
rent payer): `vault_transaction_create` plus `proposal_create` (not a
draft, so it is Active right away). The vault transaction it creates runs
atomically once approved and executed:

1. vote program `Withdraw` of X lamports from the vote account to the vault
   (the vault is the vote account's authorized withdrawer),
2. create the vault's JSOL ATA (idempotent; payer and owner are the vault,
   an off-curve PDA),
3. JPool `DepositSol` of X lamports from the vault into the vault's JSOL
   ATA, with that ATA as referral account,
4. bond `bond_top_up` from the vault's JSOL ATA into the bond JSOL account.

Before sending, the four instructions are simulated as a standalone
transaction paid by the vault without signature verification, which checks
them against current chain state (withdraw authority, bond accepting the
vault as payer, pool state). The proposal transaction itself is simulated,
sent, confirmed, and then the proposal and vault transaction accounts are
re-read: the run only succeeds if the proposal is Active and the stored
message is exactly the four planned instructions.

### Why a JSOL margin

The `bond_top_up` amount is fixed when the proposal is created, but the
owner may approve days later. JSOL appreciates against SOL, so by then the
same X SOL mints slightly fewer JSOL than quoted; a `bond_top_up` for the
full quote would fail. The amount is therefore the JSOL the deposit would
mint at proposal time minus `jsol_margin_bps` (default 100 = 1%, at most
500). The difference stays as JSOL dust in the vault's JSOL ATA and can be
swept by a later proposal or by hand.

### Owner setup

1. Create a fresh proposer keypair on the host
   (`solana-keygen new -o /etc/jpool-bond/proposer.json`).
2. In the Squads UI, add its pubkey as a member with **Initiate**
   permission only (no Vote, no Execute). This is a config change that
   needs the usual approvals. It also makes older open proposals stale.
3. Fund the proposer with about 0.05 SOL. Each proposal costs about
   0.0075 SOL of rent (vault transaction and proposal accounts, reclaimable
   in the Squads UI when a rent collector is set) plus fees, and
   `proposer_reserve_sol` (default 0.01) is always kept.
4. When a proposal shows up (Grafana alerts on the metrics below), check it
   in the Squads UI: memo `jpool-bond-rs top-up <X> SOL`, the four
   instructions above. Approve with the threshold and execute.

`plan` prints exactly what a proposal would contain without needing the
proposer to be a member, so it can be reviewed before setup.

### Guards

All direct-mode bond guards apply (program owners and discriminators,
`Token(JSOL)` collateral, Standard bond named `bond_name`, validator bond
vote account and state, bond ATA, JPool mint, decimals and accounts). In
addition, any of these makes the run fail:

- the multisig is not owned by the Squads program
  `SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf` or is not the PDA of its
  create key,
- the vote account's authorized withdrawer is not the vault
  (`["multisig", multisig, "vault", vault_index]`),
- the vault is not an empty system account.

When a proposal is due, each of these refuses it (the run fails with all
reasons listed):

- bond status is not Active, or the stake pool is not updated this epoch,
- one of our proposals was executed in the last 20 hours, or the bond's
  JSOL grew in the last 20 hours (both from chain data),
- the vote account's withdrawable balance (lamports minus the rent-exempt
  minimum for its size) is below X,
- the vault's JSOL ATA is missing and the vault cannot pay its rent,
- the proposal transaction exceeds 1232 bytes,
- the proposer is not a member with Initiate permission,
- the proposer balance is below the vault transaction rent + proposal rent
  + fee buffer + `proposer_reserve_sol`,
- the vault transaction simulation fails.

A healthy bond or a pending proposal is a successful run; a refusal is a
failure only when a proposal was due.

### plan and run

- `plan` is read-only and never loads a keypair. It prints the burn
  estimate, runway, pending proposal, decision and, when due, the four
  instructions with amounts, data and accounts, the proposer cost and the
  simulation result.
- `run` does the same, then loads `proposer_keypair_path` (also when
  nothing is due, so a broken key shows up early), checks it matches
  `proposer_pubkey`, and creates the proposal if one is due. Submission
  uses the same retry rules as direct mode.

### Configuration

```toml
rpc_url = "<vaulted premium rpc_url>"
vote_account = "steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr"
bond_name = "perf"                    # optional, default "perf"
priority_fee_micro_lamports = 1000
metrics_file = "/var/lib/alloy/textcollector/jpool_bond.prom"
floor_sol = 5                         # optional, default 5
propose_runway_days = 10              # optional, default 10
topup_days = 30                       # optional, default 30
critical_runway_days = 3              # optional, default 3 (report and metric only)
burn_lookback_days = 30               # optional, default 30, at most 90
burn_min_span_days = 3                # optional, default 3
burn_fallback_sol_per_day = 0.1       # optional, default 0.1

[squads]
multisig = "De838vwWHcbAyNWTTKeKPyviMNHaX91CrD2igGDfZZdM"
vault_index = 0                       # optional, default 0
proposer_pubkey = "<proposer pubkey>"
proposer_keypair_path = "/etc/jpool-bond/proposer.json"
proposer_reserve_sol = 0.01           # optional, default 0.01, at least 0.001
pending_scan = 20                     # optional, default 20, at most 50
jsol_margin_bps = 100                 # optional, default 100, at most 500
```

The `identity_*`, `min_value_sol`, `topup_sol` and `identity_reserve_sol`
keys of direct mode are rejected here, as is any other unknown key. Days
take integers or decimals.

Hard limits: proposal at most 20 SOL and at least 1 SOL, `floor_sol` at
most 50, `jsol_margin_bps` at most 500, `pending_scan` at most 50.

### Metrics

| Metric | Meaning |
| --- | --- |
| `jpool_bond_value_sol` | SOL value of the bond's JSOL |
| `jpool_bond_jsol` | JSOL in the bond |
| `jpool_bond_jsol_sol_rate` | SOL per JSOL |
| `jpool_bond_burn_sol_per_day` | estimated burn |
| `jpool_bond_runway_days` | days to the floor (`+Inf` with no burn) |
| `jpool_bond_floor_sol` | configured floor |
| `jpool_bond_propose_runway_days` | configured proposal threshold |
| `jpool_bond_critical_runway_days` | configured critical threshold |
| `jpool_bond_proposal_pending` | 1 while one of our proposals is open |
| `jpool_bond_proposal_index` | its transaction index (if pending) |
| `jpool_bond_proposal_age_seconds` | its age (if pending) |
| `jpool_bond_proposal_amount_sol` | SOL it moves (if pending) |
| `jpool_bond_last_proposal_created_timestamp_seconds` | creation time of our latest proposal in the scan window |
| `jpool_bond_vote_withdrawable_sol` | vote account balance above rent |
| `jpool_bond_proposer_balance_sol` | proposer SOL balance |
| `jpool_bond_last_run_success` | 1 on success, 0 on error or refusal of a due proposal |
| `jpool_bond_last_run_timestamp_seconds` | unix time of the run |

Telegram alerts come from Grafana rules on these series, not from the tool.
Typical rules: `proposal_pending == 1` (approve it),
`runway_days < critical_runway_days`, `last_run_success == 0`,
`proposer_balance_sol < 0.02`.

## RPC

Every RPC request (account reads, history, blockhash, simulate, send,
confirm), in both modes, goes to the single `rpc_url` in the config. There is no default
endpoint in the code and no fallback; the field is required. The tool never
calls `getProgramAccounts`: every address it needs is derived.

The URL usually carries an API key. It is never printed: config parse errors
report only a byte offset, `Debug` output redacts it, and RPC error messages
have the URL and its path and query parts replaced with `<redacted>`.

## Usage

```console
$ jpool-bond-rs plan --config /etc/jpool-bond/config.toml
$ jpool-bond-rs run --config /etc/jpool-bond/config.toml
```

The mode comes from the config file; the commands are the same.

## Development

Rust is pinned in `rust-toolchain.toml`.

```console
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test --all-targets
```

Tests use captured mainnet fixtures in `tests/fixtures/` and never contact a
cluster: the live bond, validator bond and stake pool accounts, and the
mainnet top-up transaction
`5nGpKvenzu9xA5iwbVcmchwKdV6yvnWfTvjHxF6F3pDaGD9JLUqxJHJLEAFCsiCn5LokMy7cjKSx4Cc8LWQn3qqo`,
against which the `DepositSol` and `bond_top_up` builders are checked byte
for byte.

The Squads v4 encoding is hand-rolled borsh (layouts, seeds and
discriminators from Squads-Protocol/v4 `programs/squads_multisig_program`)
and checked against the owner's multisig: the multisig account (threshold
2 of 4, member list), the Proposal and VaultTransaction accounts of
transactions 1 to 3, and their creation transactions. The tests decode all
of them, re-encode each `TransactionMessage` byte for byte, rebuild the
`vault_transaction_create` and `proposal_create` instructions exactly,
compile the stored messages back from their instructions, and match the
live vote `Withdraw` proposal against the vote withdraw builder. A
`BondClaim` transaction checks the burn history's balance delta parsing.

## Release

Push a `v*` tag. The release workflow builds
`jpool-bond-rs-${TAG}-x86_64-unknown-linux-gnu.tar.gz` (binary at
`jpool-bond-rs-${TAG}-x86_64-unknown-linux-gnu/jpool-bond-rs`) plus a
`.sha256` file and attaches both to the GitHub release.

## License

Apache-2.0.
