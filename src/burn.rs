//! Burn-rate estimate from the bond JSOL account's on-chain history: JSOL
//! leaving the bond (JPool claims) per day over a lookback window.

use anyhow::{Context, Result, bail};
use solana_client::{
    rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient},
    rpc_config::RpcTransactionConfig,
};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding, UiTransactionTokenBalance,
    option_serializer::OptionSerializer,
};
use std::str::FromStr;

pub const SECONDS_PER_DAY: i64 = 86_400;
const SIGNATURE_PAGE_LIMIT: usize = 1_000;
/// Refuse rather than fetch without bound if the account is unexpectedly
/// busy (claims are daily; 90 days of history is ~100 transactions).
pub const MAX_HISTORY_TRANSACTIONS: usize = 1_000;

/// Net change of the bond's JSOL in one successful transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BondJsolChange {
    pub signature: Signature,
    pub block_time: Option<i64>,
    pub delta: i128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BondHistory {
    /// Successful transactions inside the window, newest first.
    pub changes: Vec<BondJsolChange>,
    /// The newest successful transaction before the window start was a
    /// claim: claiming was already under way, so the window is full.
    pub claim_before_window: bool,
}

impl BondHistory {
    /// The newest JSOL increase (a top-up by anyone) at or after `since`.
    pub fn increase_since(&self, since: i64) -> Option<&BondJsolChange> {
        self.changes
            .iter()
            .find(|change| change.delta > 0 && change.block_time.is_none_or(|time| time >= since))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BurnSource {
    Observed,
    /// Too little history; the configured fallback rate is used.
    Fallback(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BurnEstimate {
    /// Transactions that reduced the bond's JSOL.
    pub claims: usize,
    pub outflow_jsol: u64,
    pub span_seconds: i64,
    /// SOL per day in lamports, at the current pool rate (or the fallback).
    pub lamports_per_day: f64,
    pub source: BurnSource,
}

impl BurnEstimate {
    pub fn span_days(&self) -> f64 {
        self.span_seconds as f64 / SECONDS_PER_DAY as f64
    }
}

/// Sums the JSOL decreases (claims) inside the window and divides by the
/// span from the first claim to now, so quiet history before claims started
/// does not dilute the rate (slightly overestimating it, the safe side).
/// When claims already ran before the window, the span is the full window.
/// Fewer than two claims, or less than `min_span_seconds` since the first
/// one, fall back to `fallback_lamports_per_day`. The per-day JSOL is
/// converted to SOL with `jsol_to_lamports`.
pub fn estimate_burn(
    history: &BondHistory,
    window_start: i64,
    now: i64,
    min_span_seconds: i64,
    fallback_lamports_per_day: u64,
    jsol_to_lamports: impl Fn(u64) -> Result<u64>,
) -> Result<BurnEstimate> {
    let mut claims = 0_usize;
    let mut outflow: u128 = 0;
    let mut first_claim: Option<i64> = None;
    for change in history.changes.iter().filter(|change| change.delta < 0) {
        claims += 1;
        outflow += change.delta.unsigned_abs();
        if let Some(time) = change.block_time {
            first_claim = Some(first_claim.map_or(time, |first| first.min(time)));
        }
    }
    let outflow_jsol = u64::try_from(outflow).context("JSOL outflow overflows")?;
    let span_start = if history.claim_before_window {
        Some(window_start)
    } else {
        first_claim.map(|time| time.max(window_start))
    };
    let span_seconds = span_start.map(|start| (now - start).max(0)).unwrap_or(0);
    let fallback = |reason: String| BurnEstimate {
        claims,
        outflow_jsol,
        span_seconds,
        lamports_per_day: fallback_lamports_per_day as f64,
        source: BurnSource::Fallback(reason),
    };
    if claims < 2 {
        return Ok(fallback(format!(
            "{claims} claim(s) in the window, at least 2 needed"
        )));
    }
    if span_seconds < min_span_seconds {
        return Ok(fallback(format!(
            "claims span {:.2} days, less than the required {:.2}",
            span_seconds as f64 / SECONDS_PER_DAY as f64,
            min_span_seconds as f64 / SECONDS_PER_DAY as f64
        )));
    }
    let outflow_lamports = jsol_to_lamports(outflow_jsol)?;
    Ok(BurnEstimate {
        claims,
        outflow_jsol,
        span_seconds,
        lamports_per_day: outflow_lamports as f64 * SECONDS_PER_DAY as f64 / span_seconds as f64,
        source: BurnSource::Observed,
    })
}

fn option<T>(value: OptionSerializer<T>) -> Option<T> {
    match value {
        OptionSerializer::Some(value) => Some(value),
        _ => None,
    }
}

/// Net JSOL change of `token_account` in a confirmed transaction, from its
/// pre/post token balances; None when the transaction failed. Account
/// indexes cover static keys, then lookup-table writable and readonly keys.
pub fn bond_jsol_delta(
    confirmed: &EncodedConfirmedTransactionWithStatusMeta,
    token_account: &Pubkey,
    mint: &Pubkey,
) -> Result<Option<i128>> {
    let meta = confirmed
        .transaction
        .meta
        .as_ref()
        .context("transaction has no status meta")?;
    if meta.err.is_some() {
        return Ok(None);
    }
    let transaction = confirmed
        .transaction
        .transaction
        .decode()
        .context("failed to decode transaction")?;
    let mut keys: Vec<Pubkey> = transaction.message.static_account_keys().to_vec();
    if let Some(loaded) = option(meta.loaded_addresses.clone()) {
        for key in loaded.writable.iter().chain(&loaded.readonly) {
            keys.push(Pubkey::from_str(key).context("invalid loaded address")?);
        }
    }
    let Some(index) = keys.iter().position(|key| key == token_account) else {
        bail!("transaction does not reference token account {token_account}");
    };
    let amount = |balances: Option<Vec<UiTransactionTokenBalance>>| -> Result<u128> {
        let Some(balance) = balances
            .unwrap_or_default()
            .into_iter()
            .find(|balance| usize::from(balance.account_index) == index)
        else {
            // No entry: the account did not exist (or held no token state).
            return Ok(0);
        };
        if balance.mint != mint.to_string() {
            bail!(
                "token account {token_account} has mint {} in transaction metadata, expected {mint}",
                balance.mint
            );
        }
        balance
            .ui_token_amount
            .amount
            .parse::<u128>()
            .context("invalid token amount in transaction metadata")
    };
    let pre = amount(option(meta.pre_token_balances.clone()))?;
    let post = amount(option(meta.post_token_balances.clone()))?;
    Ok(Some(post as i128 - pre as i128))
}

fn fetch_transaction(
    client: &RpcClient,
    signature: &Signature,
) -> Result<EncodedConfirmedTransactionWithStatusMeta> {
    client
        .get_transaction_with_config(
            signature,
            RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        )
        .with_context(|| format!("failed to fetch transaction {signature}"))
}

/// Walks the token account's signatures back to `window_start`, fetching
/// each successful transaction in the window for its JSOL delta, plus the
/// newest successful one before the window to tell whether claims were
/// already running at the window start.
pub fn fetch_bond_history(
    client: &RpcClient,
    token_account: &Pubkey,
    mint: &Pubkey,
    window_start: i64,
) -> Result<BondHistory> {
    let mut history = BondHistory::default();
    let mut before = None;
    let mut fetched = 0_usize;
    loop {
        let page = client
            .get_signatures_for_address_with_config(
                token_account,
                GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(SIGNATURE_PAGE_LIMIT),
                    commitment: Some(CommitmentConfig::confirmed()),
                },
            )
            .context("failed to fetch bond JSOL account signatures")?;
        let page_len = page.len();
        for entry in page {
            let signature = Signature::from_str(&entry.signature)
                .with_context(|| format!("RPC returned invalid signature {}", entry.signature))?;
            before = Some(signature);
            if entry.err.is_some() {
                continue;
            }
            fetched += 1;
            if fetched > MAX_HISTORY_TRANSACTIONS {
                bail!(
                    "bond JSOL account {token_account} has more than {MAX_HISTORY_TRANSACTIONS} transactions in the burn window; refusing"
                );
            }
            let confirmed = fetch_transaction(client, &signature)?;
            let delta = bond_jsol_delta(&confirmed, token_account, mint)
                .with_context(|| format!("transaction {signature}"))?;
            let block_time = entry.block_time.or(confirmed.block_time);
            // Newest first: the first successful transaction before the
            // window decides whether claims predate it, then we stop.
            if block_time.is_some_and(|time| time < window_start) {
                if let Some(delta) = delta {
                    history.claim_before_window = delta < 0;
                    return Ok(history);
                }
                continue;
            }
            if let Some(delta) = delta {
                history.changes.push(BondJsolChange {
                    signature,
                    block_time,
                    delta,
                });
            }
        }
        if page_len < SIGNATURE_PAGE_LIMIT {
            return Ok(history);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::pubkey;

    const BOND_JSOL_ATA: Pubkey = pubkey!("5WLKZYiBdE1hVYihrhgdBzZHE4BvVaucmsypN53HbQLS");
    const JSOL: Pubkey = pubkey!("7Q2afV64in6N6SeZsAAB81TJzwDoD6zpqmHkzi9Dcavn");
    const DAY: i64 = SECONDS_PER_DAY;

    fn fixture(raw: &str) -> EncodedConfirmedTransactionWithStatusMeta {
        serde_json::from_str(raw).expect("fixture JSON")
    }

    #[test]
    fn deltas_from_live_claim_and_topup() {
        let claim = fixture(include_str!(
            "../tests/fixtures/mainnet_bond_claim_tx_5KqraPFM.json"
        ));
        assert_eq!(
            bond_jsol_delta(&claim, &BOND_JSOL_ATA, &JSOL).unwrap(),
            Some(-47_356_682)
        );
        let topup = fixture(include_str!(
            "../tests/fixtures/mainnet_topup_tx_5nGpKven.json"
        ));
        assert_eq!(
            bond_jsol_delta(&topup, &BOND_JSOL_ATA, &JSOL).unwrap(),
            Some(3_763_402_473)
        );
        assert!(bond_jsol_delta(&claim, &BOND_JSOL_ATA, &Pubkey::new_unique()).is_err());
        assert!(bond_jsol_delta(&claim, &Pubkey::new_unique(), &JSOL).is_err());
    }

    fn change(time: i64, delta: i128) -> BondJsolChange {
        BondJsolChange {
            signature: Signature::default(),
            block_time: Some(time),
            delta,
        }
    }

    fn rate_1_5(jsol: u64) -> Result<u64> {
        Ok(jsol * 3 / 2)
    }

    fn day_claims(now: i64, days: &[i64], delta: i128) -> Vec<BondJsolChange> {
        days.iter()
            .map(|day| change(now - day * DAY - DAY / 4, delta))
            .collect()
    }

    #[test]
    fn recent_claims_after_old_topups_span_from_the_first_claim() {
        // Today's shape: top-ups 15 days ago, six daily claims of ~0.048
        // JSOL over the last ~5.3 days, one top-up between.
        let now = 100 * DAY;
        let mut changes = day_claims(now, &[0, 1, 2, 3, 4, 5], -48_000_000);
        changes.push(change(now - 5 * DAY - DAY / 2, 3_763_402_473));
        changes.push(change(now - 15 * DAY, 3_619_150_934));
        changes.push(change(now - 15 * DAY - 60, 3_619_150_934));
        let history = BondHistory {
            changes,
            claim_before_window: false,
        };
        let rate = |jsol: u64| Ok(jsol * 1_384_341_089 / 1_000_000_000);
        let estimate = estimate_burn(&history, now - 30 * DAY, now, 3 * DAY, 1, rate).unwrap();
        assert_eq!(estimate.source, BurnSource::Observed);
        assert_eq!(estimate.claims, 6);
        assert_eq!(estimate.outflow_jsol, 288_000_000);
        assert_eq!(estimate.span_seconds, 5 * DAY + DAY / 4);
        // 0.288 JSOL * 1.3843 / 5.25 days = 0.0759 SOL/day: about the
        // 0.067 SOL/day per-claim rate, slightly above (safe side), and
        // far from the 0.026 a span from the first top-up would give.
        let sol_per_day = estimate.lamports_per_day / 1e9;
        assert!((0.065..0.08).contains(&sol_per_day), "{sol_per_day}");
        assert!(history.increase_since(now - 6 * DAY).is_some());
        assert!(history.increase_since(now - 5 * DAY).is_none());
    }

    #[test]
    fn claims_before_the_window_use_the_full_window() {
        let now = 100 * DAY;
        let history = BondHistory {
            changes: day_claims(now, &(0..30).collect::<Vec<_>>(), -10_000_000),
            claim_before_window: true,
        };
        let estimate = estimate_burn(&history, now - 30 * DAY, now, 3 * DAY, 1, rate_1_5).unwrap();
        assert_eq!(estimate.source, BurnSource::Observed);
        assert_eq!(estimate.span_seconds, 30 * DAY);
        // 30 * 0.01 JSOL * 1.5 over 30 days = 0.015 SOL/day.
        assert!((estimate.lamports_per_day - 15_000_000.0).abs() < 1e-6);
    }

    #[test]
    fn fewer_than_two_claims_or_a_short_span_use_the_fallback() {
        let now = 100 * DAY;
        let one_claim = BondHistory {
            changes: vec![
                change(now - 10 * DAY, -300_000_000),
                change(now - 20 * DAY, 5_000_000_000),
            ],
            claim_before_window: false,
        };
        let estimate = estimate_burn(
            &one_claim,
            now - 30 * DAY,
            now,
            3 * DAY,
            100_000_000,
            rate_1_5,
        )
        .unwrap();
        assert!(matches!(estimate.source, BurnSource::Fallback(_)));
        assert_eq!(estimate.lamports_per_day, 100_000_000.0);
        assert_eq!(estimate.claims, 1);

        let recent = BondHistory {
            changes: day_claims(now, &[0, 1], -300_000_000),
            claim_before_window: false,
        };
        let estimate = estimate_burn(&recent, now - 30 * DAY, now, 3 * DAY, 7, rate_1_5).unwrap();
        assert!(matches!(estimate.source, BurnSource::Fallback(_)));
        assert_eq!(estimate.claims, 2);
        assert_eq!(estimate.lamports_per_day, 7.0);

        let empty = estimate_burn(
            &BondHistory::default(),
            now - 30 * DAY,
            now,
            3 * DAY,
            7,
            rate_1_5,
        )
        .unwrap();
        assert_eq!(empty.span_seconds, 0);
        assert_eq!(empty.lamports_per_day, 7.0);
    }
}
