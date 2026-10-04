//! The topic grammar, which the message store owns so that it can match
//! consumer group patterns inside the transaction that records a message.

pub use caudra_storage::topics::{
    INVALID_PATTERN, INVALID_TOPIC, MAX_PATTERNS, MISSING_PATTERN, TOO_MANY_PATTERNS, add_patterns,
    parse_pattern, parse_topic, pattern_matches, remove_patterns, validate_patterns,
};
