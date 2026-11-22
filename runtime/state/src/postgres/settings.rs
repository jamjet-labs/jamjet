//! How the Postgres store connects: pool size and timeouts, read from the
//! environment.

use std::time::Duration;

/// Connections the pool holds, at least [`MIN_POOL_SIZE`].
pub const POOL_SIZE_ENV: &str = "JAMJET_PG_POOL_SIZE";
/// Milliseconds a call waits for a free connection before it answers
/// `Unavailable`.
pub const ACQUIRE_TIMEOUT_ENV: &str = "JAMJET_PG_ACQUIRE_TIMEOUT_MS";
/// Milliseconds one statement may run (`statement_timeout`).
pub const STATEMENT_TIMEOUT_ENV: &str = "JAMJET_PG_STATEMENT_TIMEOUT_MS";
/// Milliseconds a statement waits for a lock (`lock_timeout`).
pub const LOCK_TIMEOUT_ENV: &str = "JAMJET_PG_LOCK_TIMEOUT_MS";
/// Milliseconds a session may sit idle inside a transaction before the
/// server ends it (`idle_in_transaction_session_timeout`).
pub const IDLE_IN_TX_TIMEOUT_ENV: &str = "JAMJET_PG_IDLE_IN_TX_TIMEOUT_MS";

/// The smallest pool [`PgSettings::from_env`] accepts: a sweep holding one
/// connection must leave one for claims.
pub const MIN_POOL_SIZE: u32 = 2;

/// The largest timeout Postgres takes, in milliseconds (an `int`).
const MAX_MS: u64 = i32::MAX as u64;

/// Pool size and timeouts for [`super::PgBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgSettings {
    pub pool_size: u32,
    /// sqlx waits 30 s by default; a caller should hear `Unavailable` sooner.
    pub acquire_timeout: Duration,
    pub statement_timeout: Duration,
    pub lock_timeout: Duration,
    pub idle_in_tx_timeout: Duration,
}

impl Default for PgSettings {
    fn default() -> Self {
        Self {
            pool_size: 10,
            acquire_timeout: Duration::from_millis(5_000),
            statement_timeout: Duration::from_millis(30_000),
            lock_timeout: Duration::from_millis(10_000),
            idle_in_tx_timeout: Duration::from_millis(60_000),
        }
    }
}

impl PgSettings {
    /// The settings from `JAMJET_PG_*`, each defaulted when unset. A value
    /// that is set but not valid is an error naming the variable, so a typo
    /// stops startup instead of running on a default nobody chose.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(e) => Err(format!("cannot read {name}: {e}")),
        })
    }

    /// [`Self::from_env`] over any source of values: `lookup` answers a
    /// variable's value, `None` when it is unset.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Result<Option<String>, String>,
    ) -> Result<Self, String> {
        let defaults = Self::default();
        let pool_size = match lookup(POOL_SIZE_ENV)? {
            None => defaults.pool_size,
            Some(raw) => {
                let size = whole_number(POOL_SIZE_ENV, &raw)?;
                if size < u64::from(MIN_POOL_SIZE) {
                    return Err(format!(
                        "{POOL_SIZE_ENV} must be at least {MIN_POOL_SIZE}, got {size}"
                    ));
                }
                u32::try_from(size).map_err(|_| format!("{POOL_SIZE_ENV} is too large: {size}"))?
            }
        };
        let millis = |name: &str, default: Duration| -> Result<Duration, String> {
            match lookup(name)? {
                None => Ok(default),
                Some(raw) => {
                    let ms = whole_number(name, &raw)?;
                    if !(1..=MAX_MS).contains(&ms) {
                        return Err(format!(
                            "{name} must be between 1 and {MAX_MS} milliseconds, got {ms}"
                        ));
                    }
                    Ok(Duration::from_millis(ms))
                }
            }
        };
        Ok(Self {
            pool_size,
            acquire_timeout: millis(ACQUIRE_TIMEOUT_ENV, defaults.acquire_timeout)?,
            statement_timeout: millis(STATEMENT_TIMEOUT_ENV, defaults.statement_timeout)?,
            lock_timeout: millis(LOCK_TIMEOUT_ENV, defaults.lock_timeout)?,
            idle_in_tx_timeout: millis(IDLE_IN_TX_TIMEOUT_ENV, defaults.idle_in_tx_timeout)?,
        })
    }
}

/// `raw` as a whole number, or an error naming `name`.
fn whole_number(name: &str, raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    trimmed
        .parse::<u64>()
        .map_err(|e| format!("invalid {name} `{trimmed}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn with(vars: &[(&str, &str)]) -> Result<PgSettings, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        PgSettings::from_lookup(|name| Ok(vars.get(name).cloned()))
    }

    #[test]
    fn settings_refuse_a_bad_value() {
        for bad in ["abc", "1", "", "-3", "2.5"] {
            let e = with(&[(POOL_SIZE_ENV, bad)]).expect_err(bad);
            assert!(e.contains(POOL_SIZE_ENV), "{bad:?}: {e}");
        }
        for name in [
            ACQUIRE_TIMEOUT_ENV,
            STATEMENT_TIMEOUT_ENV,
            LOCK_TIMEOUT_ENV,
            IDLE_IN_TX_TIMEOUT_ENV,
        ] {
            for bad in ["abc", "0", "2147483648"] {
                let e = with(&[(name, bad)]).expect_err(bad);
                assert!(e.contains(name), "{name}={bad:?}: {e}");
            }
        }
    }

    #[test]
    fn settings_default_what_is_unset_and_take_what_is_set() {
        assert_eq!(with(&[]).unwrap(), PgSettings::default());
        assert_eq!(
            PgSettings::default(),
            PgSettings {
                pool_size: 10,
                acquire_timeout: Duration::from_millis(5_000),
                statement_timeout: Duration::from_millis(30_000),
                lock_timeout: Duration::from_millis(10_000),
                idle_in_tx_timeout: Duration::from_millis(60_000),
            }
        );
        let set = with(&[
            (POOL_SIZE_ENV, " 2 "),
            (ACQUIRE_TIMEOUT_ENV, "500"),
            (STATEMENT_TIMEOUT_ENV, "1"),
            (LOCK_TIMEOUT_ENV, "2147483647"),
            (IDLE_IN_TX_TIMEOUT_ENV, "7"),
        ])
        .unwrap();
        assert_eq!(
            set,
            PgSettings {
                pool_size: 2,
                acquire_timeout: Duration::from_millis(500),
                statement_timeout: Duration::from_millis(1),
                lock_timeout: Duration::from_millis(2_147_483_647),
                idle_in_tx_timeout: Duration::from_millis(7),
            }
        );
    }

    #[test]
    fn an_unreadable_variable_is_an_error() {
        let e = PgSettings::from_lookup(|name| {
            if name == LOCK_TIMEOUT_ENV {
                Err(format!("cannot read {name}: not unicode"))
            } else {
                Ok(None)
            }
        })
        .unwrap_err();
        assert!(e.contains(LOCK_TIMEOUT_ENV), "{e}");
    }
}
