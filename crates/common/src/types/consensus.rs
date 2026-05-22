use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::types::errors::ArithmeticError;

macro_rules! define_u64_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Default,
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(val: u64) -> Self {
                Self(val)
            }

            pub fn as_u64(&self) -> u64 {
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
            type Output = Result<Self, ArithmeticError>;

            fn add(self, rhs: u64) -> Self::Output {
                self.0
                    .checked_add(rhs)
                    .map(Self)
                    .ok_or(ArithmeticError::Overflow {
                        type_name: stringify!($name),
                    })
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
    type Output = Result<Self, ArithmeticError>;

    fn sub(self, rhs: u64) -> Self::Output {
        self.0
            .checked_sub(rhs)
            .map(Self)
            .ok_or(ArithmeticError::Underflow {
                type_name: "LogIndex",
            })
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
                assert_eq!(idx.as_u64(), 42);
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
                let result = (idx + 5).unwrap();
                assert_eq!(result.as_u64(), 15);
            }

            #[test]
            fn supports_subtraction_with_u64() {
                let idx = LogIndex::new(10);
                let result = (idx - 3).unwrap();
                assert_eq!(result.as_u64(), 7);
            }

            #[test]
            fn returns_error_on_overflow() {
                let idx = LogIndex::new(u64::MAX);
                let result = idx + 1;
                assert_eq!(
                    result,
                    Err(ArithmeticError::Overflow {
                        type_name: "LogIndex"
                    })
                );
            }

            #[test]
            fn returns_error_on_underflow() {
                let idx = LogIndex::new(0);
                let result = idx - 1;
                assert_eq!(
                    result,
                    Err(ArithmeticError::Underflow {
                        type_name: "LogIndex"
                    })
                );
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
                assert_eq!(term.as_u64(), 5);
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
                let result = (term + 1).unwrap();
                assert_eq!(result.as_u64(), 11);
            }

            #[test]
            fn returns_error_on_overflow() {
                let term = Term::new(u64::MAX);
                let result = term + 1;
                assert_eq!(result, Err(ArithmeticError::Overflow { type_name: "Term" }));
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
                assert_eq!(seq.as_u64(), 100);
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
                let result = (seq + 1).unwrap();
                assert_eq!(result.as_u64(), 11);
            }

            #[test]
            fn returns_error_on_overflow() {
                let seq = SequenceId::new(u64::MAX);
                let result = seq + 1;
                assert_eq!(
                    result,
                    Err(ArithmeticError::Overflow {
                        type_name: "SequenceId"
                    })
                );
            }
        }
    }
}
