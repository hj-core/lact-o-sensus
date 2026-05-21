use std::fmt;
use std::ops::Add;
use std::ops::AddAssign;
use std::ops::Sub;

use rand::RngExt;

/// Monotonic, absolute "system time" for the deterministic consensus engine.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(u64);

impl Tick {
    pub const ZERO: Self = Self(0);

    pub fn new(val: u64) -> Self {
        Self(val)
    }

    pub fn increment(&mut self) {
        self.0 += 1;
    }
}

impl Add<TickDuration> for Tick {
    type Output = Self;

    fn add(self, rhs: TickDuration) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign<TickDuration> for Tick {
    fn add_assign(&mut self, rhs: TickDuration) {
        self.0 += rhs.0;
    }
}

impl Sub<Tick> for Tick {
    type Output = TickDuration;

    fn sub(self, rhs: Tick) -> Self::Output {
        TickDuration(self.0.saturating_sub(rhs.0))
    }
}

/// A unitless measurement of time duration expressed in ticks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TickDuration(u64);

impl TickDuration {
    pub const ZERO: Self = Self(0);

    pub fn new(val: u64) -> Self {
        Self(val)
    }
}

impl Add for TickDuration {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for TickDuration {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl fmt::Display for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tick({})", self.0)
    }
}

impl std::fmt::Display for TickDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ticks", self.0)
    }
}

/// Sterile collection of unitless tick thresholds derived from configuration.
///
/// Shield's the Logical Orchestrator from environment-aware timing units (ms)
/// and OS-level non-determinism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickThresholds {
    pub heartbeat_interval: TickDuration,
    pub min_election: TickDuration,
    pub max_election: TickDuration,
}

impl TickThresholds {
    /// Generates a random election timeout within the [min, max] range.
    pub fn generate_election_timeout(&self, rng: &mut impl RngExt) -> TickDuration {
        TickDuration::new(rng.random_range(self.min_election.0..=self.max_election.0))
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use super::*;

    mod tick {
        use super::*;

        mod arithmetic {
            use super::*;

            #[test]
            fn should_increment_monotonically() {
                let mut tick = Tick::new(10);
                tick.increment();
                assert_eq!(tick, Tick::new(11));
            }

            #[test]
            fn should_support_addition_with_duration() {
                let tick = Tick::new(100);
                let duration = TickDuration::new(50);
                let result = tick + duration;
                assert_eq!(result, Tick::new(150));
            }

            #[test]
            fn should_support_subtraction_returning_duration() {
                let t1 = Tick::new(100);
                let t2 = Tick::new(70);
                let diff = t1 - t2;
                assert_eq!(diff, TickDuration::new(30));
            }

            #[test]
            fn should_saturate_on_subtraction_underflow() {
                let t1 = Tick::new(50);
                let t2 = Tick::new(100);
                let diff = t1 - t2;
                assert_eq!(diff, TickDuration::new(0));
            }
        }
    }

    mod tick_duration {
        use super::*;

        #[test]
        fn should_support_addition() {
            let d1 = TickDuration::new(10);
            let d2 = TickDuration::new(20);
            assert_eq!(d1 + d2, TickDuration::new(30));
        }

        #[test]
        fn should_support_subtraction_with_saturation() {
            let d1 = TickDuration::new(10);
            let d2 = TickDuration::new(20);
            assert_eq!(d1 - d2, TickDuration::new(0));
        }
    }

    mod tick_thresholds {
        use super::*;

        #[test]
        fn should_generate_timeout_within_bounds() {
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(100),
                max_election: TickDuration::new(200),
            };
            let mut rng = StdRng::seed_from_u64(42);

            for _ in 0..100 {
                let timeout = thresholds.generate_election_timeout(&mut rng);
                assert!(timeout.0 >= 100);
                assert!(timeout.0 <= 200);
            }
        }
    }
}
