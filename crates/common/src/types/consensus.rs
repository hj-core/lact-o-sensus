use std::fmt;

use serde::Deserialize;
use serde::Serialize;

macro_rules! define_u64_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(val: u64) -> Self {
                Self(val)
            }

            pub fn value(&self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<u64> for $name {
            fn from(val: u64) -> Self {
                Self(val)
            }
        }

        impl From<$name> for u64 {
            fn from(val: $name) -> u64 {
                val.0
            }
        }

        impl std::ops::Add<u64> for $name {
            type Output = Self;

            fn add(self, rhs: u64) -> Self {
                Self(
                    self.0
                        .checked_add(rhs)
                        .expect("Halt Mandate: Arithmetic overflow in domain type"),
                )
            }
        }
    };
}

define_u64_newtype!(LogIndex, "Monotonic index of an entry in the Raft log.");
define_u64_newtype!(Term, "The current election term in the Raft cluster.");
define_u64_newtype!(
    SequenceId,
    "Monotonic sequence identifier for Exactly-Once Semantics (EOS)."
);

// --- Domain-Specific Arithmetic Overrides ---

impl std::ops::Sub<u64> for LogIndex {
    type Output = Self;

    fn sub(self, rhs: u64) -> Self {
        Self(
            self.0
                .checked_sub(rhs)
                .expect("Halt Mandate: LogIndex underflow (protocol violation)"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod log_index {
        use super::*;

        mod new {
            use super::*;
            #[test]
            fn stores_provided_u64() {
                let idx = LogIndex::new(42);
                assert_eq!(idx.value(), 42);
            }
        }

        mod display {
            use super::*;
            #[test]
            fn formats_as_raw_u64_string() {
                let idx = LogIndex::new(42);
                assert_eq!(format!("{}", idx), "42");
            }
        }

        mod conversions {
            use super::*;
            #[test]
            fn supports_lossless_roundtrip_with_u64() {
                let raw = 42u64;
                let idx = LogIndex::from(raw);
                assert_eq!(u64::from(idx), raw);
            }
        }

        mod arithmetic {
            use super::*;
            #[test]
            fn supports_addition_with_u64() {
                let idx = LogIndex::new(10);
                let result = idx + 5;
                assert_eq!(result.value(), 15);
            }

            #[test]
            fn supports_subtraction_with_u64() {
                let idx = LogIndex::new(10);
                let result = idx - 3;
                assert_eq!(result.value(), 7);
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: Arithmetic overflow")]
            fn panics_on_overflow() {
                let idx = LogIndex::new(u64::MAX);
                let _ = idx + 1;
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: LogIndex underflow")]
            fn panics_on_underflow() {
                let idx = LogIndex::new(0);
                let _ = idx - 1;
            }
        }
    }

    mod term {
        use super::*;

        mod new {
            use super::*;
            #[test]
            fn stores_provided_u64() {
                let term = Term::new(5);
                assert_eq!(term.value(), 5);
            }
        }

        mod display {
            use super::*;
            #[test]
            fn formats_as_raw_u64_string() {
                let term = Term::new(5);
                assert_eq!(format!("{}", term), "5");
            }
        }

        mod conversions {
            use super::*;
            #[test]
            fn supports_lossless_roundtrip_with_u64() {
                let raw = 5u64;
                let term = Term::from(raw);
                assert_eq!(u64::from(term), raw);
            }
        }

        mod arithmetic {
            use super::*;
            #[test]
            fn supports_addition_with_u64() {
                let term = Term::new(10);
                let result = term + 1;
                assert_eq!(result.value(), 11);
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: Arithmetic overflow")]
            fn panics_on_overflow() {
                let term = Term::new(u64::MAX);
                let _ = term + 1;
            }
        }
    }

    mod sequence_id {
        use super::*;

        mod new {
            use super::*;
            #[test]
            fn stores_provided_u64() {
                let seq = SequenceId::new(100);
                assert_eq!(seq.value(), 100);
            }
        }

        mod display {
            use super::*;
            #[test]
            fn formats_as_raw_u64_string() {
                let seq = SequenceId::new(100);
                assert_eq!(format!("{}", seq), "100");
            }
        }

        mod conversions {
            use super::*;
            #[test]
            fn supports_lossless_roundtrip_with_u64() {
                let raw = 100u64;
                let seq = SequenceId::from(raw);
                assert_eq!(u64::from(seq), raw);
            }
        }

        mod arithmetic {
            use super::*;
            #[test]
            fn supports_addition_with_u64() {
                let seq = SequenceId::new(10);
                let result = seq + 1;
                assert_eq!(result.value(), 11);
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: Arithmetic overflow")]
            fn panics_on_overflow() {
                let seq = SequenceId::new(u64::MAX);
                let _ = seq + 1;
            }
        }
    }
}
