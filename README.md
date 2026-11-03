# jpool-bond-rs

Keeps a validator's JPool performance bond (JSOL collateral) sized to JPool's
own bond requirement.
Built to run as a daily one-shot from a systemd timer, but works standalone.

## How it works

Each run derives every account from the config, reads them in one batch, and
values the bond: the JSOL in the bond's token account times the JPool rate
(`total_lamports / pool_token_supply` from the stake pool account).

The requirement comes from JPool's bond API
(`{bond_api_url}/{vote}/bond`, `current.security_required +
current.perf_required`; the response must name our vote account), and never
counts as less than `min_required_sol`. JPool's docs (Validator Docs, "Bonds")
define the security part as 0.5 SOL per 1,000 SOL of the validator's JPool
stake and the performance part from the APY gap.

If the value is at or above `trigger_ratio` x required, nothing happens. If it
is below, the run tops up to `target_ratio` x required, at most
`max_topup_sol`, in one atomic transaction (a failed API read fails the run):

1. create the identity's JSOL ATA (idempotent; it is the staging account),
2. JPool `DepositSol` of the top-up from the identity into that ATA,
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

## Guards

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

Hard limits (not raisable via config): `max_topup_sol` at most 10,
`min_required_sol` at most 50, ratios within 1 <= trigger <= target <= 5,
`identity_reserve_sol` at least 1,
`priority_fee_micro_lamports` at most 1000000, three attempts.

## RPC

Every RPC request (account reads, history, blockhash, simulate, send,
confirm) goes to the single `rpc_url` in the config. There is no default
endpoint in the code and no fallback; the field is required. The tool never
calls `getProgramAccounts`: every address it needs is derived.

The URL usually carries an API key. It is never printed: config parse errors
report only a byte offset, `Debug` output redacts it, and RPC error messages
have the URL and its path and query parts replaced with `<redacted>`.

## Configuration

```toml
rpc_url = "<vaulted premium rpc_url>"
vote_account = "steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr"
identity_pubkey = "EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej"
identity_keypair_path = "/etc/jpool-bond/identity.json"
bond_name = "perf"                    # optional, default "perf"
bond_api_url = "https://api.jpool.one/allocation/validators"  # optional, default
min_required_sol = 5                  # optional, default 5
trigger_ratio = 1.5                   # optional, default 1.5
target_ratio = 2                      # optional, default 2
max_topup_sol = 10                    # optional, default 10
identity_reserve_sol = 10             # optional, default 10
priority_fee_micro_lamports = 1000
metrics_file = "/var/lib/alloy/textcollector/jpool_bond.prom"
```

SOL amounts take integers or decimals. Unknown keys are rejected. Key
material only ever comes from the keypair file.

## Usage

```console
$ jpool-bond-rs plan --config /etc/jpool-bond/config.toml
$ jpool-bond-rs run --config /etc/jpool-bond/config.toml
```

## Metrics

Every run atomically replaces a Prometheus textfile. All series carry a
`vote` label.

| Metric | Meaning |
| --- | --- |
| `jpool_bond_value_sol` | SOL value of the bond's JSOL |
| `jpool_bond_jsol` | JSOL in the bond |
| `jpool_bond_jsol_sol_rate` | SOL per JSOL |
| `jpool_bond_min_value_sol` | value below which a top-up runs (trigger x required) |
| `jpool_bond_required_sol` | requirement used (JPool's, at least `min_required_sol`) |
| `jpool_bond_target_sol` | value a top-up aims for (target x required) |
| `jpool_bond_identity_balance_sol` | identity SOL balance |
| `jpool_bond_last_topup_sol` | SOL topped up by this run, 0 if none |
| `jpool_bond_last_run_success` | 1 on success, 0 on any error or refusal |
| `jpool_bond_last_run_timestamp_seconds` | unix time of the run |

Values the run could not read are left out rather than written as 0. The
metrics path is write-tested before any chain access.

## Manual withdrawal

`withdraw` takes SOL worth of JSOL out of the bond and pays it out as SOL in
one transaction: `bond_withdraw` moves the JSOL to the identity's JSOL account,
then the JPool stake pool's `WithdrawSol` sends the SOL to `--destination`
(default: the config's funding wallet). The validator identity signs; the
bond program accepts it as withdrawal authority while none is set on the
validator bond (the command refuses otherwise). Account order follows the
program's on-chain Anchor IDL. It writes no metrics.

```bash
jpool-bond-rs withdraw --config config.toml --identity-keypair identity.json --sol 7.5 --dry-run
jpool-bond-rs withdraw --config config.toml --identity-keypair identity.json --sol 7.5
```

`--dry-run` simulates the unsigned transaction and never loads the key.

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

## Release

Push a `v*` tag. The release workflow builds
`jpool-bond-rs-${TAG}-x86_64-unknown-linux-gnu.tar.gz` (binary at
`jpool-bond-rs-${TAG}-x86_64-unknown-linux-gnu/jpool-bond-rs`) plus a
`.sha256` file and attaches both to the GitHub release.

## License

Apache-2.0.

