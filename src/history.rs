//! Chain-history cooldown: finds a recent successful top-up signed by the
//! identity, so the one-per-window rule holds without local state.

use crate::{Addresses, bond::is_bond_top_up};
use anyhow::{Context, Result, bail};
use solana_client::{
    rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient},
    rpc_config::RpcTransactionConfig,
};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::str::FromStr;

const SIGNATURE_PAGE_LIMIT: usize = 100;
/// Refuse rather than scan forever if the bond account is unexpectedly busy.
const MAX_SIGNATURES_SCANNED: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentTopUp {
    pub signature: Signature,
    pub block_time: Option<i64>,
}

/// True when `identity` signed the transaction and it carries a top-level
/// `bond_top_up`. Only static keys can sign or be invoked as programs, so
/// address lookup tables do not matter here.
pub fn is_identity_topup(transaction: &VersionedTransaction, identity: &Pubkey) -> bool {
    let message = &transaction.message;
    let keys = message.static_account_keys();
    let signers = usize::from(message.header().num_required_signatures).min(keys.len());
    if !keys[..signers].contains(identity) {
        return false;
    }
    message.instructions().iter().any(|instruction| {
        keys.get(usize::from(instruction.program_id_index))
            .is_some_and(|program| is_bond_top_up(program, &instruction.data))
    })
}

pub fn find_recent_identity_topup(
    client: &RpcClient,
    addresses: &Addresses,
    since_unix: i64,
) -> Result<Option<RecentTopUp>> {
    let mut before = None;
    let mut scanned = 0_usize;
    loop {
        let page = client
            .get_signatures_for_address_with_config(
                &addresses.bond_token_account,
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
            scanned += 1;
            // Newest first: once past the window, nothing older matters.
            if entry.block_time.is_some_and(|time| time < since_unix) {
                return Ok(None);
            }
            let signature = Signature::from_str(&entry.signature)
                .with_context(|| format!("RPC returned invalid signature {}", entry.signature))?;
            before = Some(signature);
            if entry.err.is_some() {
                continue;
            }
            let confirmed = client
                .get_transaction_with_config(
                    &signature,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        commitment: Some(CommitmentConfig::confirmed()),
                        max_supported_transaction_version: Some(0),
                    },
                )
                .with_context(|| format!("failed to fetch transaction {signature}"))?;
            let failed = confirmed
                .transaction
                .meta
                .as_ref()
                .is_some_and(|meta| meta.err.is_some());
            if failed {
                continue;
            }
            let transaction = confirmed
                .transaction
                .transaction
                .decode()
                .with_context(|| format!("failed to decode transaction {signature}"))?;
            let block_time = entry.block_time.or(confirmed.block_time);
            if block_time.is_some_and(|time| time < since_unix) {
                return Ok(None);
            }
            // A missing block time counts as inside the window.
            if is_identity_topup(&transaction, &addresses.identity) {
                return Ok(Some(RecentTopUp {
                    signature,
                    block_time,
                }));
            }
        }
        if page_len < SIGNATURE_PAGE_LIMIT {
            return Ok(None);
        }
        if scanned >= MAX_SIGNATURES_SCANNED {
            bail!(
                "scanned {scanned} signatures on bond JSOL account {} without leaving the cooldown window; refusing",
                addresses.bond_token_account
            );
        }
    }
}
