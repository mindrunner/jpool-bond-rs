//! Squads v4 multisig: account decoders, the vault `TransactionMessage`
//! encoding, and the `vault_transaction_create` / `proposal_create`
//! instructions. Hand-rolled borsh; layouts, seeds and discriminators follow
//! Squads-Protocol/v4 `programs/squads_multisig_program` and are checked
//! against captured mainnet accounts and transactions in the tests.

use crate::{ChainAccount, bond::Reader};
use anyhow::{Result, bail};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

pub const SQUADS_PROGRAM_ID: Pubkey = pubkey!("SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf");
pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");

pub const SEED_PREFIX: &[u8] = b"multisig";
pub const SEED_MULTISIG: &[u8] = b"multisig";
pub const SEED_TRANSACTION: &[u8] = b"transaction";
pub const SEED_PROPOSAL: &[u8] = b"proposal";
pub const SEED_VAULT: &[u8] = b"vault";

/// sha256("account:<Name>")[..8]
pub const MULTISIG_DISCRIMINATOR: [u8; 8] = [224, 116, 121, 186, 68, 161, 79, 236];
pub const PROPOSAL_DISCRIMINATOR: [u8; 8] = [26, 94, 189, 187, 116, 136, 53, 33];
pub const VAULT_TRANSACTION_DISCRIMINATOR: [u8; 8] = [168, 250, 162, 100, 81, 14, 162, 207];
/// sha256("global:<instruction>")[..8]
pub const VAULT_TRANSACTION_CREATE_DISCRIMINATOR: [u8; 8] = [48, 250, 78, 168, 208, 226, 218, 211];
pub const PROPOSAL_CREATE_DISCRIMINATOR: [u8; 8] = [220, 60, 73, 224, 30, 108, 79, 159];

/// Member permission bits (`Permission` in the program).
pub const PERMISSION_INITIATE: u8 = 1 << 0;
pub const PERMISSION_VOTE: u8 = 1 << 1;
pub const PERMISSION_EXECUTE: u8 = 1 << 2;

pub fn vault_address(multisig: &Pubkey, vault_index: u8) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_PREFIX, multisig.as_ref(), SEED_VAULT, &[vault_index]],
        &SQUADS_PROGRAM_ID,
    )
    .0
}

pub fn transaction_address(multisig: &Pubkey, transaction_index: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            SEED_PREFIX,
            multisig.as_ref(),
            SEED_TRANSACTION,
            &transaction_index.to_le_bytes(),
        ],
        &SQUADS_PROGRAM_ID,
    )
    .0
}

pub fn proposal_address(multisig: &Pubkey, transaction_index: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            SEED_PREFIX,
            multisig.as_ref(),
            SEED_TRANSACTION,
            &transaction_index.to_le_bytes(),
            SEED_PROPOSAL,
        ],
        &SQUADS_PROGRAM_ID,
    )
    .0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub key: Pubkey,
    pub permissions: u8,
}

impl Member {
    pub fn can_initiate(&self) -> bool {
        self.permissions & PERMISSION_INITIATE != 0
    }

    pub fn describe_permissions(&self) -> String {
        let names: Vec<&str> = [
            (PERMISSION_INITIATE, "Initiate"),
            (PERMISSION_VOTE, "Vote"),
            (PERMISSION_EXECUTE, "Execute"),
        ]
        .iter()
        .filter(|(bit, _)| self.permissions & bit != 0)
        .map(|(_, name)| *name)
        .collect();
        if names.is_empty() {
            "none".into()
        } else {
            names.join("+")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Multisig {
    pub create_key: Pubkey,
    pub config_authority: Pubkey,
    pub threshold: u16,
    pub time_lock: u32,
    pub transaction_index: u64,
    pub stale_transaction_index: u64,
    pub rent_collector: Option<Pubkey>,
    pub bump: u8,
    pub members: Vec<Member>,
}

impl Multisig {
    pub fn member(&self, key: &Pubkey) -> Option<&Member> {
        self.members.iter().find(|member| member.key == *key)
    }
}

fn check_squads_account(
    what: &str,
    address: &Pubkey,
    account: &ChainAccount,
    discriminator: &[u8; 8],
) -> Result<()> {
    if account.owner != SQUADS_PROGRAM_ID {
        bail!(
            "{what} {address} is owned by {}, expected Squads program {SQUADS_PROGRAM_ID}",
            account.owner
        );
    }
    if account.data.get(..8) != Some(discriminator.as_slice()) {
        bail!("{what} {address} has the wrong account discriminator");
    }
    Ok(())
}

fn option_pubkey(reader: &mut Reader<'_>, what: &str) -> Result<Option<Pubkey>> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(reader.pubkey()?)),
        tag => bail!("{what} has invalid option tag {tag}"),
    }
}

fn pubkey_vec(reader: &mut Reader<'_>) -> Result<Vec<Pubkey>> {
    let len = usize::try_from(reader.u32()?)?;
    if len > reader.remaining() / 32 {
        bail!("{} vector length {len} exceeds the data", reader.what);
    }
    (0..len).map(|_| reader.pubkey()).collect()
}

fn byte_vec_u32(reader: &mut Reader<'_>) -> Result<Vec<u8>> {
    let len = usize::try_from(reader.u32()?)?;
    Ok(reader.take(len)?.to_vec())
}

pub fn decode_multisig(address: &Pubkey, account: &ChainAccount) -> Result<Multisig> {
    check_squads_account("multisig", address, account, &MULTISIG_DISCRIMINATOR)?;
    let mut reader = Reader::new(&account.data, 8, "multisig");
    let create_key = reader.pubkey()?;
    let config_authority = reader.pubkey()?;
    let threshold = reader.u16()?;
    let time_lock = reader.u32()?;
    let transaction_index = reader.u64()?;
    let stale_transaction_index = reader.u64()?;
    let rent_collector = option_pubkey(&mut reader, "multisig rent_collector")?;
    let bump = reader.u8()?;
    let members_len = usize::try_from(reader.u32()?)?;
    if members_len > reader.remaining() / 33 {
        bail!("multisig {address} member count {members_len} exceeds the data");
    }
    let members = (0..members_len)
        .map(|_| {
            Ok(Member {
                key: reader.pubkey()?,
                permissions: reader.u8()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let (expected, expected_bump) = Pubkey::find_program_address(
        &[SEED_PREFIX, SEED_MULTISIG, create_key.as_ref()],
        &SQUADS_PROGRAM_ID,
    );
    if expected != *address || expected_bump != bump {
        bail!("multisig {address} is not the Squads PDA of its create_key {create_key}");
    }
    Ok(Multisig {
        create_key,
        config_authority,
        threshold,
        time_lock,
        transaction_index,
        stale_transaction_index,
        rent_collector,
        bump,
        members,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalStatus {
    Draft(i64),
    Active(i64),
    Rejected(i64),
    Approved(i64),
    /// Deprecated in the program; kept so old accounts still decode.
    Executing,
    Executed(i64),
    Cancelled(i64),
}

impl ProposalStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Draft(_) => "Draft",
            Self::Active(_) => "Active",
            Self::Rejected(_) => "Rejected",
            Self::Approved(_) => "Approved",
            Self::Executing => "Executing",
            Self::Executed(_) => "Executed",
            Self::Cancelled(_) => "Cancelled",
        }
    }

    pub fn timestamp(&self) -> Option<i64> {
        match self {
            Self::Draft(time)
            | Self::Active(time)
            | Self::Rejected(time)
            | Self::Approved(time)
            | Self::Executed(time)
            | Self::Cancelled(time) => Some(*time),
            Self::Executing => None,
        }
    }

    /// Not yet settled: Draft, Active, Approved (and the legacy Executing).
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::Draft(_) | Self::Active(_) | Self::Approved(_) | Self::Executing
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub multisig: Pubkey,
    pub transaction_index: u64,
    pub status: ProposalStatus,
    pub bump: u8,
    pub approved: Vec<Pubkey>,
    pub rejected: Vec<Pubkey>,
    pub cancelled: Vec<Pubkey>,
}

pub fn decode_proposal(address: &Pubkey, account: &ChainAccount) -> Result<Proposal> {
    check_squads_account("proposal", address, account, &PROPOSAL_DISCRIMINATOR)?;
    let mut reader = Reader::new(&account.data, 8, "proposal");
    let multisig = reader.pubkey()?;
    let transaction_index = reader.u64()?;
    let status = match reader.u8()? {
        0 => ProposalStatus::Draft(reader.i64()?),
        1 => ProposalStatus::Active(reader.i64()?),
        2 => ProposalStatus::Rejected(reader.i64()?),
        3 => ProposalStatus::Approved(reader.i64()?),
        4 => ProposalStatus::Executing,
        5 => ProposalStatus::Executed(reader.i64()?),
        6 => ProposalStatus::Cancelled(reader.i64()?),
        tag => bail!("proposal {address} has unknown status tag {tag}"),
    };
    Ok(Proposal {
        multisig,
        transaction_index,
        status,
        bump: reader.u8()?,
        approved: pubkey_vec(&mut reader)?,
        rejected: pubkey_vec(&mut reader)?,
        cancelled: pubkey_vec(&mut reader)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub account_indexes: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressTableLookup {
    pub account_key: Pubkey,
    pub writable_indexes: Vec<u8>,
    pub readonly_indexes: Vec<u8>,
}

/// The message a vault transaction executes. The instruction argument
/// (`TransactionMessage`) encodes vectors as SmallVec (u8 lengths, u16 for
/// instruction data); the stored account (`VaultTransactionMessage`) uses
/// plain borsh Vec (u32 lengths). Both decode to this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultMessage {
    pub num_signers: u8,
    pub num_writable_signers: u8,
    pub num_writable_non_signers: u8,
    pub account_keys: Vec<Pubkey>,
    pub instructions: Vec<CompiledInstruction>,
    pub address_table_lookups: Vec<AddressTableLookup>,
}

fn small_len(len: usize, max: usize, what: &str) -> Result<usize> {
    if len > max {
        bail!("{what} has {len} entries, at most {max} fit the Squads encoding");
    }
    Ok(len)
}

impl VaultMessage {
    /// Compiles instructions the way the Squads SDK does (web3.js
    /// `compileToV0Message` with the vault as payer): the vault first, then
    /// keys in first-use order with each program id before its accounts,
    /// grouped as writable signers, readonly signers, writable and readonly
    /// non-signers. The vault must be the only signer.
    pub fn compile(vault: &Pubkey, instructions: &[Instruction]) -> Result<Self> {
        let mut keys: Vec<(Pubkey, bool, bool)> = vec![(*vault, true, true)];
        let mut upsert = |key: &Pubkey, signer: bool, writable: bool| {
            if let Some(entry) = keys.iter_mut().find(|entry| entry.0 == *key) {
                entry.1 |= signer;
                entry.2 |= writable;
            } else {
                keys.push((*key, signer, writable));
            }
        };
        for instruction in instructions {
            upsert(&instruction.program_id, false, false);
            for meta in &instruction.accounts {
                upsert(&meta.pubkey, meta.is_signer, meta.is_writable);
            }
        }
        let group = |signer: bool, writable: bool| {
            keys.iter()
                .filter(move |entry| entry.1 == signer && entry.2 == writable)
                .map(|entry| entry.0)
        };
        let writable_signers: Vec<Pubkey> = group(true, true).collect();
        let readonly_signers: Vec<Pubkey> = group(true, false).collect();
        let writable_non_signers: Vec<Pubkey> = group(false, true).collect();
        let readonly_non_signers: Vec<Pubkey> = group(false, false).collect();
        if writable_signers.as_slice() != [*vault] || !readonly_signers.is_empty() {
            bail!("the vault {vault} must be the only signer of a vault transaction");
        }
        let account_keys: Vec<Pubkey> = writable_signers
            .iter()
            .chain(&readonly_signers)
            .chain(&writable_non_signers)
            .chain(&readonly_non_signers)
            .copied()
            .collect();
        small_len(
            account_keys.len(),
            u8::MAX as usize,
            "vault message account keys",
        )?;
        let index_of = |key: &Pubkey| -> Result<u8> {
            let index = account_keys
                .iter()
                .position(|candidate| candidate == key)
                .ok_or_else(|| anyhow::anyhow!("key {key} missing from compiled message"))?;
            Ok(u8::try_from(index)?)
        };
        let mut compiled = Vec::with_capacity(instructions.len());
        for instruction in instructions {
            if instruction.accounts.iter().any(|meta| {
                meta.pubkey == instruction.program_id && (meta.is_signer || meta.is_writable)
            }) {
                bail!(
                    "program {} is passed as a writable or signer account",
                    instruction.program_id
                );
            }
            small_len(instruction.accounts.len(), u8::MAX as usize, "instruction")?;
            small_len(
                instruction.data.len(),
                u16::MAX as usize,
                "instruction data",
            )?;
            compiled.push(CompiledInstruction {
                program_id_index: index_of(&instruction.program_id)?,
                account_indexes: instruction
                    .accounts
                    .iter()
                    .map(|meta| index_of(&meta.pubkey))
                    .collect::<Result<_>>()?,
                data: instruction.data.clone(),
            });
        }
        small_len(
            compiled.len(),
            u8::MAX as usize,
            "vault message instructions",
        )?;
        let message = Self {
            num_signers: 1,
            num_writable_signers: 1,
            num_writable_non_signers: u8::try_from(writable_non_signers.len())?,
            account_keys,
            instructions: compiled,
            address_table_lookups: Vec::new(),
        };
        for instruction in &message.instructions {
            let index = usize::from(instruction.program_id_index);
            if message.is_signer_index(index) || message.is_writable_index(index) {
                bail!(
                    "program {} would be a signer or writable in the vault message",
                    message.account_keys[index]
                );
            }
        }
        message.validate()?;
        Ok(message)
    }

    pub fn is_signer_index(&self, index: usize) -> bool {
        index < usize::from(self.num_signers)
    }

    /// Mirrors `VaultTransactionMessage::is_static_writable_index`.
    pub fn is_writable_index(&self, index: usize) -> bool {
        if index >= self.account_keys.len() {
            return false;
        }
        let signers = usize::from(self.num_signers);
        if index < usize::from(self.num_writable_signers) {
            return true;
        }
        index >= signers && index - signers < usize::from(self.num_writable_non_signers)
    }

    /// The executable instructions, with account flags from the header.
    /// Messages with address lookup tables are not resolvable offline.
    pub fn instructions(&self) -> Result<Vec<Instruction>> {
        if !self.address_table_lookups.is_empty() {
            bail!("vault message uses address lookup tables");
        }
        self.instructions
            .iter()
            .map(|instruction| {
                let key = |index: u8| {
                    self.account_keys
                        .get(usize::from(index))
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("account index {index} out of range"))
                };
                Ok(Instruction {
                    program_id: key(instruction.program_id_index)?,
                    accounts: instruction
                        .account_indexes
                        .iter()
                        .map(|index| {
                            Ok(AccountMeta {
                                pubkey: key(*index)?,
                                is_signer: self.is_signer_index(usize::from(*index)),
                                is_writable: self.is_writable_index(usize::from(*index)),
                            })
                        })
                        .collect::<Result<_>>()?,
                    data: instruction.data.clone(),
                })
            })
            .collect()
    }

    /// The key at `index` when it is static, None when it comes from a lookup table.
    pub fn static_key(&self, index: u8) -> Option<Pubkey> {
        self.account_keys.get(usize::from(index)).copied()
    }

    /// Squads `TransactionMessage` bytes, the `vault_transaction_create` argument.
    pub fn encode_transaction_message(&self) -> Result<Vec<u8>> {
        let mut out = vec![
            self.num_signers,
            self.num_writable_signers,
            self.num_writable_non_signers,
        ];
        out.push(u8::try_from(self.account_keys.len())?);
        for key in &self.account_keys {
            out.extend_from_slice(key.as_ref());
        }
        out.push(u8::try_from(self.instructions.len())?);
        for instruction in &self.instructions {
            out.push(instruction.program_id_index);
            out.push(u8::try_from(instruction.account_indexes.len())?);
            out.extend_from_slice(&instruction.account_indexes);
            out.extend_from_slice(&u16::try_from(instruction.data.len())?.to_le_bytes());
            out.extend_from_slice(&instruction.data);
        }
        out.push(u8::try_from(self.address_table_lookups.len())?);
        for lookup in &self.address_table_lookups {
            out.extend_from_slice(lookup.account_key.as_ref());
            out.push(u8::try_from(lookup.writable_indexes.len())?);
            out.extend_from_slice(&lookup.writable_indexes);
            out.push(u8::try_from(lookup.readonly_indexes.len())?);
            out.extend_from_slice(&lookup.readonly_indexes);
        }
        Ok(out)
    }

    /// Decodes `TransactionMessage` bytes; trailing bytes are an error.
    pub fn decode_transaction_message(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes, 0, "transaction message");
        let num_signers = reader.u8()?;
        let num_writable_signers = reader.u8()?;
        let num_writable_non_signers = reader.u8()?;
        let account_keys = (0..reader.u8()?)
            .map(|_| reader.pubkey())
            .collect::<Result<Vec<_>>>()?;
        let instructions = (0..reader.u8()?)
            .map(|_| {
                let program_id_index = reader.u8()?;
                let accounts = usize::from(reader.u8()?);
                let account_indexes = reader.take(accounts)?.to_vec();
                let data_len = usize::from(reader.u16()?);
                Ok(CompiledInstruction {
                    program_id_index,
                    account_indexes,
                    data: reader.take(data_len)?.to_vec(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let address_table_lookups = (0..reader.u8()?)
            .map(|_| {
                let account_key = reader.pubkey()?;
                let writable = usize::from(reader.u8()?);
                let writable_indexes = reader.take(writable)?.to_vec();
                let readonly = usize::from(reader.u8()?);
                Ok(AddressTableLookup {
                    account_key,
                    writable_indexes,
                    readonly_indexes: reader.take(readonly)?.to_vec(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if reader.remaining() != 0 {
            bail!(
                "transaction message has {} trailing bytes",
                reader.remaining()
            );
        }
        let message = Self {
            num_signers,
            num_writable_signers,
            num_writable_non_signers,
            account_keys,
            instructions,
            address_table_lookups,
        };
        message.validate()?;
        Ok(message)
    }

    /// The stored `VaultTransactionMessage` (borsh Vec encoding).
    fn decode_stored(reader: &mut Reader<'_>) -> Result<Self> {
        let num_signers = reader.u8()?;
        let num_writable_signers = reader.u8()?;
        let num_writable_non_signers = reader.u8()?;
        let account_keys = pubkey_vec(reader)?;
        let count = usize::try_from(reader.u32()?)?;
        if count > reader.remaining() {
            bail!("vault message instruction count {count} exceeds the data");
        }
        let instructions = (0..count)
            .map(|_| {
                Ok(CompiledInstruction {
                    program_id_index: reader.u8()?,
                    account_indexes: byte_vec_u32(reader)?,
                    data: byte_vec_u32(reader)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let count = usize::try_from(reader.u32()?)?;
        if count > reader.remaining() / 32 {
            bail!("vault message lookup count {count} exceeds the data");
        }
        let address_table_lookups = (0..count)
            .map(|_| {
                Ok(AddressTableLookup {
                    account_key: reader.pubkey()?,
                    writable_indexes: byte_vec_u32(reader)?,
                    readonly_indexes: byte_vec_u32(reader)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let message = Self {
            num_signers,
            num_writable_signers,
            num_writable_non_signers,
            account_keys,
            instructions,
            address_table_lookups,
        };
        message.validate()?;
        Ok(message)
    }

    /// Length of the stored (`VaultTransactionMessage`) encoding.
    pub fn stored_len(&self) -> usize {
        3 + 4
            + 32 * self.account_keys.len()
            + 4
            + self
                .instructions
                .iter()
                .map(|instruction| {
                    1 + 4 + instruction.account_indexes.len() + 4 + instruction.data.len()
                })
                .sum::<usize>()
            + 4
            + self
                .address_table_lookups
                .iter()
                .map(|lookup| {
                    32 + 4 + lookup.writable_indexes.len() + 4 + lookup.readonly_indexes.len()
                })
                .sum::<usize>()
    }

    /// The bounds checks `VaultTransactionMessage::try_from` applies.
    fn validate(&self) -> Result<()> {
        let keys = self.account_keys.len();
        let all_keys = keys
            + self
                .address_table_lookups
                .iter()
                .map(|lookup| lookup.writable_indexes.len() + lookup.readonly_indexes.len())
                .sum::<usize>();
        if usize::from(self.num_signers) > keys
            || self.num_writable_signers > self.num_signers
            || usize::from(self.num_writable_non_signers)
                > keys.saturating_sub(usize::from(self.num_signers))
        {
            bail!("vault message header does not fit its account keys");
        }
        for instruction in &self.instructions {
            if usize::from(instruction.program_id_index) >= all_keys
                || instruction
                    .account_indexes
                    .iter()
                    .any(|index| usize::from(*index) >= all_keys)
            {
                bail!("vault message instruction references an unknown account");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultTransaction {
    pub multisig: Pubkey,
    pub creator: Pubkey,
    pub index: u64,
    pub bump: u8,
    pub vault_index: u8,
    pub vault_bump: u8,
    pub ephemeral_signer_bumps: Vec<u8>,
    pub message: VaultMessage,
}

pub fn decode_vault_transaction(
    address: &Pubkey,
    account: &ChainAccount,
) -> Result<VaultTransaction> {
    check_squads_account(
        "vault transaction",
        address,
        account,
        &VAULT_TRANSACTION_DISCRIMINATOR,
    )?;
    let mut reader = Reader::new(&account.data, 8, "vault transaction");
    Ok(VaultTransaction {
        multisig: reader.pubkey()?,
        creator: reader.pubkey()?,
        index: reader.u64()?,
        bump: reader.u8()?,
        vault_index: reader.u8()?,
        vault_bump: reader.u8()?,
        ephemeral_signer_bumps: byte_vec_u32(&mut reader)?,
        message: VaultMessage::decode_stored(&mut reader)?,
    })
}

/// Space `vault_transaction_create` allocates (`VaultTransaction::size`).
pub fn vault_transaction_account_len(ephemeral_signers: u8, message: &VaultMessage) -> usize {
    8 + 32 + 32 + 8 + 1 + 1 + 1 + 4 + usize::from(ephemeral_signers) + message.stored_len()
}

/// Space `proposal_create` allocates (`Proposal::size`).
pub fn proposal_account_len(members: usize) -> usize {
    8 + 32 + 8 + 1 + 8 + 1 + 3 * (4 + members * 32)
}

/// Decoded `VaultTransactionCreateArgs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultTransactionCreateArgs {
    pub vault_index: u8,
    pub ephemeral_signers: u8,
    pub transaction_message: Vec<u8>,
    pub memo: Option<String>,
}

impl VaultTransactionCreateArgs {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut data = VAULT_TRANSACTION_CREATE_DISCRIMINATOR.to_vec();
        data.push(self.vault_index);
        data.push(self.ephemeral_signers);
        data.extend_from_slice(&u32::try_from(self.transaction_message.len())?.to_le_bytes());
        data.extend_from_slice(&self.transaction_message);
        match &self.memo {
            None => data.push(0),
            Some(memo) => {
                data.push(1);
                data.extend_from_slice(&u32::try_from(memo.len())?.to_le_bytes());
                data.extend_from_slice(memo.as_bytes());
            }
        }
        Ok(data)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        if !data.starts_with(&VAULT_TRANSACTION_CREATE_DISCRIMINATOR) {
            bail!("not a vault_transaction_create instruction");
        }
        let mut reader = Reader::new(data, 8, "vault_transaction_create args");
        let vault_index = reader.u8()?;
        let ephemeral_signers = reader.u8()?;
        let transaction_message = byte_vec_u32(&mut reader)?;
        let memo = match reader.u8()? {
            0 => None,
            1 => Some(
                String::from_utf8(byte_vec_u32(&mut reader)?)
                    .map_err(|_| anyhow::anyhow!("memo is not UTF-8"))?,
            ),
            tag => bail!("invalid memo option tag {tag}"),
        };
        if reader.remaining() != 0 {
            bail!("vault_transaction_create args have trailing bytes");
        }
        Ok(Self {
            vault_index,
            ephemeral_signers,
            transaction_message,
            memo,
        })
    }
}

/// `vault_transaction_create`: accounts multisig (w), transaction (w),
/// creator (s), rent_payer (s, w), system program.
pub fn vault_transaction_create_instruction(
    multisig: &Pubkey,
    transaction_index: u64,
    creator: &Pubkey,
    rent_payer: &Pubkey,
    args: &VaultTransactionCreateArgs,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: SQUADS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*multisig, false),
            AccountMeta::new(transaction_address(multisig, transaction_index), false),
            AccountMeta::new_readonly(*creator, true),
            AccountMeta::new(*rent_payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: args.encode()?,
    })
}

/// `proposal_create`: accounts multisig, proposal (w), creator (s),
/// rent_payer (s, w), system program; args transaction_index u64, draft bool.
pub fn proposal_create_instruction(
    multisig: &Pubkey,
    transaction_index: u64,
    creator: &Pubkey,
    rent_payer: &Pubkey,
    draft: bool,
) -> Instruction {
    let mut data = PROPOSAL_CREATE_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&transaction_index.to_le_bytes());
    data.push(u8::from(draft));
    Instruction {
        program_id: SQUADS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*multisig, false),
            AccountMeta::new(proposal_address(multisig, transaction_index), false),
            AccountMeta::new_readonly(*creator, true),
            AccountMeta::new(*rent_payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

#[cfg(test)]
#[path = "squads_tests.rs"]
mod tests;
