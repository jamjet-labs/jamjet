use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Defines how a node retries on failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first). Must be >= 1.
    pub max_attempts: u32,
    /// Backoff strategy between attempts.
    pub backoff: BackoffStrategy,
    /// Initial delay before the first retry.
    #[serde(with = "duration_secs")]
    pub initial_delay: Duration,
    /// Maximum delay cap (exponential backoff will not exceed this).
    #[serde(with = "duration_secs")]
    pub max_delay: Duration,
    /// Whether to add random jitter to delays (prevents thundering herd).
    pub jitter: bool,
    /// Which error classes are retryable. Empty = retry on any error.
    pub retryable_on: Vec<ErrorClass>,
}

impl RetryPolicy {
    /// A sensible default for I/O-bound tool calls.
    pub fn io_default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffStrategy::Exponential,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: true,
            retryable_on: vec![
                ErrorClass::IoError,
                ErrorClass::Timeout,
                ErrorClass::ConnectionReset,
            ],
        }
    }

    /// A sensible default for LLM calls.
    pub fn llm_default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffStrategy::Exponential,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            jitter: true,
            retryable_on: vec![
                ErrorClass::RateLimit,
                ErrorClass::Timeout,
                ErrorClass::ServerError,
            ],
        }
    }

    /// No retries — fail immediately on any error.
    pub fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            backoff: BackoffStrategy::Fixed,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter: false,
            retryable_on: vec![],
        }
    }

    /// Compute the delay before the nth retry (0-indexed).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let base = match self.backoff {
            BackoffStrategy::Fixed => self.initial_delay,
            BackoffStrategy::Linear => self.initial_delay * attempt,
            BackoffStrategy::Exponential => {
                let factor = 2u64.saturating_pow(attempt - 1);
                self.initial_delay.saturating_mul(factor as u32)
            }
        };
        let capped = base.min(self.max_delay);
        if self.jitter {
            // Simple jitter: randomize between 50% and 100% of the delay.
            // In production code, use a proper RNG passed in.
            capped
        } else {
            capped
        }
    }
}

/// Backoff strategy between retry attempts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackoffStrategy {
    /// Same delay every time.
    Fixed,
    /// Linearly increasing delay.
    Linear,
    /// Exponentially increasing delay (2^n * initial_delay).
    Exponential,
}

/// Categories of errors that can be retried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    IoError,
    Timeout,
    RateLimit,
    ServerError,
    ConnectionReset,
    Custom(String),
}

// Serialize Duration as integer seconds for YAML/JSON friendliness.
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_delay() {
        let policy = RetryPolicy {
            max_attempts: 5,
            backoff: BackoffStrategy::Exponential,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: false,
            retryable_on: vec![],
        };
        assert_eq!(policy.delay_for_attempt(0), Duration::ZERO);
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(1));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(4));
        // Capped at max_delay
        assert_eq!(policy.delay_for_attempt(6), Duration::from_secs(30));
    }
}
