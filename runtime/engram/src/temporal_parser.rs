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

// ---------------------------------------------------------------------------
// Category intent detection
// ---------------------------------------------------------------------------

static PREFERENCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Matches requests for personalized suggestions/recommendations — NOT factual recall
    Regex::new(r"(?i)\b(can\s+you\s+(recommend|suggest)|recommend\s+(some|me)|suggest\s+(some|me|a\b)|tips?\s+for|advice\s+for|ideas?\s+for|activities?\s+for|recipes?\s+for|ways?\s+to|what\s+should\s+i)\b").unwrap()
});

static ASSISTANT_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Matches questions about what the assistant previously said/recommended
    // Past-tense or "did you" — references to what the assistant *already* said/did
    Regex::new(r"(?i)(you\s+(said|told|recommended|suggested|mentioned|provided|gave|listed|explained)|what\s+did\s+you\s+(recommend|suggest|say|tell)|what\s+was\s+the\s+.+?\s+you\s+(recommend|suggest)(ed)?|the\s+.+?\s+you\s+(recommend|suggest)(ed)?|your\s+recommendation|did\s+you\s+(recommend|suggest|say|mention))").unwrap()
});

/// Detect which fact category is most relevant to the query.
///
/// Returns the category name to boost during retrieval, or `None` if
/// no specific category is indicated.
pub fn detect_category_intent(query: &str) -> Option<&'static str> {
    // Assistant-referencing queries take priority (more specific)
    if ASSISTANT_RE.is_match(query) {
        return Some("assistant_recommendation");
    }
    if PREFERENCE_RE.is_match(query) {
        return Some("preference");
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

    #[test]
    fn detects_preference_intent() {
        assert_eq!(detect_category_intent("Can you recommend some evening activities for me?"), Some("preference"));
        assert_eq!(detect_category_intent("What are some tips for meal prep?"), Some("preference"));
        assert_eq!(detect_category_intent("Suggest recipes for dinner"), Some("preference"));
    }

    #[test]
    fn detects_assistant_intent() {
        assert_eq!(detect_category_intent("What sealant did you recommend?"), Some("assistant_recommendation"));
        assert_eq!(detect_category_intent("What was the dish you suggested?"), Some("assistant_recommendation"));
        assert_eq!(detect_category_intent("You told me about a restaurant, what was it?"), Some("assistant_recommendation"));
    }

    #[test]
    fn category_none_for_factual() {
        assert!(detect_category_intent("What is my favorite color?").is_none());
        assert!(detect_category_intent("Where do I live?").is_none());
        assert!(detect_category_intent("How much did I spend on the handbag?").is_none());
    }
}
