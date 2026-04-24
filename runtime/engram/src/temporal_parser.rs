//! Temporal intent detection — identifies time-related queries for retrieval scoring.

use regex::Regex;
use std::sync::LazyLock;

/// The type of temporal intent detected in a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalIntent {
    Duration,
    Recency,
    Ordering,
    PointInTime,
}

/// Result of temporal intent detection.
#[derive(Debug, Clone)]
pub struct TemporalQuery {
    pub intent: TemporalIntent,
    pub confidence: f32,
}

static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(how\s+many\s+(days?|weeks?|months?|years?|hours?)|how\s+long|duration|elapsed|apart)\b").unwrap()
});

static RECENCY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(recently|last\s+time|most\s+recent|latest|newest|current(ly)?)\b").unwrap()
});

static ORDERING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(before|after|first|last|earlier|later|previous|next|in\s+what\s+order|chronolog|sequence)\b").unwrap()
});

static POINT_IN_TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(when\s+did|what\s+date|what\s+day|on\s+(january|february|march|april|may|june|july|august|september|october|november|december|\d{1,2})|in\s+(january|february|march|april|may|june|july|august|september|october|november|december|20\d{2}))\b").unwrap()
});

/// Detect temporal intent in a query string. Returns `None` for non-temporal queries.
pub fn detect_temporal_intent(query: &str) -> Option<TemporalQuery> {
    if DURATION_RE.is_match(query) {
        return Some(TemporalQuery { intent: TemporalIntent::Duration, confidence: 0.9 });
    }
    if POINT_IN_TIME_RE.is_match(query) {
        return Some(TemporalQuery { intent: TemporalIntent::PointInTime, confidence: 0.85 });
    }
    if ORDERING_RE.is_match(query) {
        return Some(TemporalQuery { intent: TemporalIntent::Ordering, confidence: 0.8 });
    }
    if RECENCY_RE.is_match(query) {
        return Some(TemporalQuery { intent: TemporalIntent::Recency, confidence: 0.75 });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_duration() {
        assert_eq!(detect_temporal_intent("How many days between my dentist visit and the concert?").unwrap().intent, TemporalIntent::Duration);
        assert_eq!(detect_temporal_intent("How long did the renovation take?").unwrap().intent, TemporalIntent::Duration);
    }

    #[test]
    fn detects_recency() {
        assert_eq!(detect_temporal_intent("What did I do most recently?").unwrap().intent, TemporalIntent::Recency);
    }

    #[test]
    fn detects_ordering() {
        assert_eq!(detect_temporal_intent("Did I go to the gym before or after the meeting?").unwrap().intent, TemporalIntent::Ordering);
    }

    #[test]
    fn detects_point_in_time() {
        assert_eq!(detect_temporal_intent("When did I start my new job?").unwrap().intent, TemporalIntent::PointInTime);
    }

    #[test]
    fn returns_none_for_non_temporal() {
        assert!(detect_temporal_intent("What is my favorite color?").is_none());
        assert!(detect_temporal_intent("Where do I live?").is_none());
    }
}
