use anyhow::{Context, Result, bail};
use serde::Deserialize;
use solana_pubkey::Pubkey;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
/// Hard cap on one top-up, not raisable via config.
pub const HARD_MAX_TOPUP_SOL: u64 = 10;
/// Hard cap on the floor, not raisable via config.
pub const HARD_MAX_MIN_VALUE_SOL: u64 = 50;
/// The identity pays vote fees; never let a top-up drain it below this.
pub const HARD_MIN_IDENTITY_RESERVE_SOL: u64 = 1;
/// 1 lamport per compute unit at most; keeps the fee buffer bounded.
pub const HARD_MAX_PRIORITY_FEE_MICRO_LAMPORTS: u64 = 1_000_000;
pub const DEFAULT_BOND_NAME: &str = "perf";
pub const DEFAULT_IDENTITY_RESERVE_SOL: u64 = 10;

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub rpc_url: String,
    pub vote_account: Pubkey,
    pub identity_pubkey: Pubkey,
    pub identity_keypair_path: PathBuf,
    pub bond_name: String,
    pub min_value_lamports: u64,
    pub topup_lamports: u64,
    pub identity_reserve_lamports: u64,
    pub priority_fee_micro_lamports: u64,
    pub metrics_file: PathBuf,
}

// The RPC URL carries an API key; keep it out of every Debug rendering.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("rpc_url", &"<redacted>")
            .field("vote_account", &self.vote_account)
            .field("identity_pubkey", &self.identity_pubkey)
            .field("identity_keypair_path", &self.identity_keypair_path)
            .field("bond_name", &self.bond_name)
            .field("min_value_lamports", &self.min_value_lamports)
            .field("topup_lamports", &self.topup_lamports)
            .field("identity_reserve_lamports", &self.identity_reserve_lamports)
            .field(
                "priority_fee_micro_lamports",
                &self.priority_fee_micro_lamports,
            )
            .field("metrics_file", &self.metrics_file)
            .finish()
    }
}

/// SOL amounts accept integers (`8`) and decimals (`8.5`).
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(untagged)]
enum SolAmount {
    Integer(u64),
    Decimal(f64),
}

impl SolAmount {
    fn to_lamports(self, field: &str) -> Result<u64> {
        match self {
            Self::Integer(sol) => sol
                .checked_mul(LAMPORTS_PER_SOL)
                .with_context(|| format!("{field} overflows")),
            Self::Decimal(sol) => {
                if !sol.is_finite() || sol < 0.0 {
                    bail!("{field} must be a finite non-negative number, got {sol}");
                }
                let lamports = (sol * LAMPORTS_PER_SOL as f64).round();
                if lamports > u64::MAX as f64 {
                    bail!("{field} overflows");
                }
                Ok(lamports as u64)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    rpc_url: String,
    vote_account: String,
    identity_pubkey: String,
    identity_keypair_path: PathBuf,
    #[serde(default = "default_bond_name")]
    bond_name: String,
    min_value_sol: SolAmount,
    topup_sol: SolAmount,
    #[serde(default = "default_identity_reserve")]
    identity_reserve_sol: SolAmount,
    priority_fee_micro_lamports: u64,
    metrics_file: PathBuf,
}

fn default_bond_name() -> String {
    DEFAULT_BOND_NAME.to_string()
}

const fn default_identity_reserve() -> SolAmount {
    SolAmount::Integer(DEFAULT_IDENTITY_RESERVE_SOL)
}

/// Best-effort fields for failure metrics when the config itself is invalid.
pub struct MetricsHint {
    pub metrics_file: PathBuf,
    pub vote_account: Option<Pubkey>,
}

impl Config {
    pub fn metrics_hint(path: impl AsRef<Path>) -> Option<MetricsHint> {
        let raw = fs::read_to_string(path).ok()?;
        let value = toml::from_str::<toml::Value>(&raw).ok()?;
        let metrics_file = PathBuf::from(value.get("metrics_file")?.as_str()?);
        let vote_account = value
            .get("vote_account")
            .and_then(|vote| vote.as_str())
            .and_then(|vote| Pubkey::from_str(vote).ok());
        Some(MetricsHint {
            metrics_file,
            vote_account,
        })
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        // toml's error text quotes the offending line, which may be the
        // rpc_url with its API key; report only the location.
        let raw: RawConfig = toml::from_str(&raw).map_err(|error| {
            let location = error
                .span()
                .map(|span| format!(" at byte offset {}", span.start))
                .unwrap_or_default();
            anyhow::anyhow!(
                "invalid config {}{location}: {}",
                path.display(),
                error.message()
            )
        })?;
        let config = Self {
            rpc_url: raw.rpc_url,
            vote_account: Pubkey::from_str(&raw.vote_account).context("invalid vote_account")?,
            identity_pubkey: Pubkey::from_str(&raw.identity_pubkey)
                .context("invalid identity_pubkey")?,
            identity_keypair_path: raw.identity_keypair_path,
            bond_name: raw.bond_name,
            min_value_lamports: raw.min_value_sol.to_lamports("min_value_sol")?,
            topup_lamports: raw.topup_sol.to_lamports("topup_sol")?,
            identity_reserve_lamports: raw
                .identity_reserve_sol
                .to_lamports("identity_reserve_sol")?,
            priority_fee_micro_lamports: raw.priority_fee_micro_lamports,
            metrics_file: raw.metrics_file,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !(self.rpc_url.starts_with("https://") || self.rpc_url.starts_with("http://")) {
            bail!("rpc_url must be an http(s) URL");
        }
        if self.identity_keypair_path.as_os_str().is_empty() {
            bail!("identity_keypair_path must not be empty");
        }
        if self.metrics_file.as_os_str().is_empty() {
            bail!("metrics_file must not be empty");
        }
        // PDA seeds are limited to 32 bytes each.
        if self.bond_name.is_empty() || self.bond_name.len() > 32 {
            bail!(
                "bond_name must be 1..=32 bytes, got {} bytes",
                self.bond_name.len()
            );
        }
        if self.topup_lamports == 0 || self.topup_lamports > HARD_MAX_TOPUP_SOL * LAMPORTS_PER_SOL {
            bail!(
                "topup_sol must be in (0, {HARD_MAX_TOPUP_SOL}], got {}",
                format_sol(self.topup_lamports)
            );
        }
        if self.min_value_lamports == 0
            || self.min_value_lamports > HARD_MAX_MIN_VALUE_SOL * LAMPORTS_PER_SOL
        {
            bail!(
                "min_value_sol must be in (0, {HARD_MAX_MIN_VALUE_SOL}], got {}",
                format_sol(self.min_value_lamports)
            );
        }
        if self.identity_reserve_lamports < HARD_MIN_IDENTITY_RESERVE_SOL * LAMPORTS_PER_SOL {
            bail!(
                "identity_reserve_sol must be at least {HARD_MIN_IDENTITY_RESERVE_SOL}, got {}",
                format_sol(self.identity_reserve_lamports)
            );
        }
        if self.priority_fee_micro_lamports > HARD_MAX_PRIORITY_FEE_MICRO_LAMPORTS {
            bail!(
                "priority_fee_micro_lamports must be at most {HARD_MAX_PRIORITY_FEE_MICRO_LAMPORTS}, got {}",
                self.priority_fee_micro_lamports
            );
        }
        Ok(())
    }
}

/// Removes the RPC URL and its secret-looking parts (path segments and
/// query values, where providers put API keys) from a message.
pub fn redact_rpc_url(message: &str, rpc_url: &str) -> String {
    let mut needles: Vec<String> = vec![rpc_url.to_string()];
    let trimmed = rpc_url.trim_end_matches('/');
    needles.push(trimmed.to_string());
    let without_scheme = rpc_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(rpc_url);
    let tail_start = without_scheme
        .find(['/', '?', '#'])
        .unwrap_or(without_scheme.len());
    let tail = &without_scheme[tail_start..];
    for part in tail.split(['/', '?', '&', '=', '#']) {
        if part.len() >= 6 {
            needles.push(part.to_string());
        }
    }
    let mut redacted = message.to_string();
    needles.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
    for needle in needles.iter().filter(|needle| !needle.is_empty()) {
        redacted = redacted.replace(needle.as_str(), "<redacted>");
    }
    redacted
}

/// Renders lamports as SOL with all nine decimals, trailing zeros trimmed.
pub fn format_sol(lamports: u64) -> String {
    format_base_units(lamports)
}

/// Renders 9-decimal base units (SOL or JSOL) without float rounding.
pub fn format_base_units(amount: u64) -> String {
    let whole = amount / LAMPORTS_PER_SOL;
    let fraction = amount % LAMPORTS_PER_SOL;
    if fraction == 0 {
        return whole.to_string();
    }
    let fraction = format!("{fraction:09}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID: &str = r#"
rpc_url = "https://rpc.example.test"
vote_account = "steakxfubt37xYdvuXz7BV5Uhhhk1FyJx9zGHfDcTVr"
identity_pubkey = "EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej"
identity_keypair_path = "/secure/identity.json"
min_value_sol = 8
topup_sol = 5
priority_fee_micro_lamports = 1000
metrics_file = "/var/lib/alloy/textcollector/jpool_bond.prom"
"#;

    fn load(raw: &str) -> Result<Config> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(raw.as_bytes())?;
        Config::load(file.path())
    }

    #[test]
    fn applies_defaults() {
        let config = load(VALID).expect("valid config");
        assert_eq!(config.bond_name, "perf");
        assert_eq!(config.min_value_lamports, 8 * LAMPORTS_PER_SOL);
        assert_eq!(config.topup_lamports, 5 * LAMPORTS_PER_SOL);
        assert_eq!(config.identity_reserve_lamports, 10 * LAMPORTS_PER_SOL);
    }

    #[test]
    fn accepts_decimal_sol_amounts() {
        let config = load(&VALID.replace("topup_sol = 5", "topup_sol = 2.5")).unwrap();
        assert_eq!(config.topup_lamports, 2_500_000_000);
    }

    #[test]
    fn rpc_url_is_required() {
        let without = VALID.replace("rpc_url = \"https://rpc.example.test\"\n", "");
        let error = load(&without).expect_err("rpc_url is required");
        assert!(format!("{error:#}").contains("rpc_url"));
    }

    #[test]
    fn enforces_hard_caps() {
        for (from, to) in [
            ("topup_sol = 5", "topup_sol = 10.000000001"),
            ("topup_sol = 5", "topup_sol = 0"),
            ("min_value_sol = 8", "min_value_sol = 51"),
            ("min_value_sol = 8", "min_value_sol = -1.0"),
            (
                "priority_fee_micro_lamports = 1000",
                "priority_fee_micro_lamports = 1000001",
            ),
            ("https://rpc.example.test", "file:///tmp/rpc"),
        ] {
            let invalid = VALID.replace(from, to);
            assert!(load(&invalid).is_err(), "accepted {to}");
        }
        assert!(load(&VALID.replace("topup_sol = 5", "topup_sol = 10")).is_ok());
        assert!(load(&VALID.replace("min_value_sol = 8", "min_value_sol = 50")).is_ok());
        assert!(load(&format!("{VALID}identity_reserve_sol = 0.5\n")).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_bad_pubkeys() {
        assert!(load(&format!("{VALID}private_key = [1, 2, 3]\n")).is_err());
        assert!(
            load(&VALID.replace("EUDis6LJeJzDHTEBgfHGQyjHp63XZkGkx4E69xunC2Ej", "nope")).is_err()
        );
    }

    #[test]
    fn parse_errors_do_not_echo_the_rpc_url() {
        let secret = "https://rpc.example.test/?api-key=SECRETKEY";
        let broken = VALID
            .replace("https://rpc.example.test", secret)
            .replace("topup_sol = 5", "topup_sol = \"five\"");
        let error = format!("{:#}", load(&broken).expect_err("broken config"));
        assert!(!error.contains("SECRETKEY"), "{error}");
        let debug = format!(
            "{:?}",
            load(&VALID.replace("https://rpc.example.test", secret))
        );
        assert!(!debug.contains("SECRETKEY"), "{debug}");
    }

    #[test]
    fn redacts_rpc_url_and_key_parts() {
        let url = "https://rpc.example.test/v1/0123456789abcdef?api-key=SECRETKEY123";
        let message = format!(
            "error sending request for url ({url}): x; also https://rpc.example.test/v1/0123456789abcdef/?api-key=SECRETKEY123"
        );
        let redacted = redact_rpc_url(&message, url);
        assert!(!redacted.contains("SECRETKEY123"), "{redacted}");
        assert!(!redacted.contains("0123456789abcdef"), "{redacted}");
        assert_eq!(redact_rpc_url("plain", url), "plain");
    }

    #[test]
    fn formats_base_units_exactly() {
        assert_eq!(format_base_units(11_001_704_341), "11.001704341");
        assert_eq!(format_base_units(5_000_000_000), "5");
        assert_eq!(format_base_units(1), "0.000000001");
    }
}
