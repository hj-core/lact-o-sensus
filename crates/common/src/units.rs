use std::ops::Add;
use std::ops::Sub;
use std::str::FromStr;

use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;
use strum::Display;
use strum::EnumString;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UnitError {
    #[error("Invalid unit symbol: {0}")]
    InvalidSymbol(String),

    #[error("Dimensional mismatch: operation not permitted across dimensions")]
    DimensionalMismatch,

    #[error("Invalid quantity format: {0}")]
    InvalidQuantity(String),

    #[error("Arithmetic overflow or underflow")]
    ArithmeticError,
}

/// The four physical dimensions authorized by ADR 008.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Dimension {
    Mass,
    Volume,
    Count,
    Anomalous,
}

impl Dimension {
    /// Returns the canonical SI base unit symbol for this dimension.
    pub fn base_unit(&self) -> &'static str {
        match self {
            Dimension::Mass => "g",
            Dimension::Volume => "ml",
            Dimension::Count => "units",
            Dimension::Anomalous => "misc",
        }
    }
}

// --- NewType Enforcement for Physical Domains ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mass(pub Decimal);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume(pub Decimal);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count(pub Decimal);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anomalous(pub Decimal);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalQuantity {
    Mass(Mass),
    Volume(Volume),
    Count(Count),
    Anomalous(Anomalous),
}

impl PhysicalQuantity {
    pub fn value(&self) -> Decimal {
        match self {
            Self::Mass(m) => m.0,
            Self::Volume(v) => v.0,
            Self::Count(c) => c.0,
            Self::Anomalous(a) => a.0,
        }
    }

    pub fn dimension(&self) -> Dimension {
        match self {
            Self::Mass(_) => Dimension::Mass,
            Self::Volume(_) => Dimension::Volume,
            Self::Count(_) => Dimension::Count,
            Self::Anomalous(_) => Dimension::Anomalous,
        }
    }
}

// --- The Dimensional Fence (Arithmetic) ---

impl Add for PhysicalQuantity {
    type Output = Result<PhysicalQuantity, UnitError>;

    fn add(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (Self::Mass(a), Self::Mass(b)) => {
                let res = a.0.checked_add(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Mass(Mass(res)))
            }
            (Self::Volume(a), Self::Volume(b)) => {
                let res = a.0.checked_add(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Volume(Volume(res)))
            }
            (Self::Count(a), Self::Count(b)) => {
                let res = a.0.checked_add(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Count(Count(res)))
            }
            (Self::Anomalous(a), Self::Anomalous(b)) => {
                let res = a.0.checked_add(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Anomalous(Anomalous(res)))
            }
            _ => Err(UnitError::DimensionalMismatch),
        }
    }
}

impl Sub for PhysicalQuantity {
    type Output = Result<PhysicalQuantity, UnitError>;

    fn sub(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (Self::Mass(a), Self::Mass(b)) => {
                let res = a.0.checked_sub(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Mass(Mass(res)))
            }
            (Self::Volume(a), Self::Volume(b)) => {
                let res = a.0.checked_sub(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Volume(Volume(res)))
            }
            (Self::Count(a), Self::Count(b)) => {
                let res = a.0.checked_sub(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Count(Count(res)))
            }
            (Self::Anomalous(a), Self::Anomalous(b)) => {
                let res = a.0.checked_sub(b.0).ok_or(UnitError::ArithmeticError)?;
                Ok(Self::Anomalous(Anomalous(res)))
            }
            _ => Err(UnitError::DimensionalMismatch),
        }
    }
}

// --- Unit Registry ---

#[derive(Debug, Clone)]
pub struct UnitRegistryEntry {
    pub symbol: &'static str,
    pub dimension: Dimension,
    pub multiplier: Decimal,
    /// If true, the multiplier depends on the specific item context (e.g.,
    /// 'pack'). If false, the multiplier is a universal physical constant
    /// (e.g., 'kg').
    pub is_contextual: bool,
}

pub struct UnitRegistry;

impl UnitRegistry {
    /// High-level Orchestrator: Parses a quantity and unit symbol into a
    /// validated, stabilized `PhysicalQuantity`.
    pub fn parse_and_convert(quantity: &str, unit: &str) -> Result<PhysicalQuantity, UnitError> {
        let entry = Self::resolve_symbol(unit)?;
        let base_val = Self::convert_to_base_val(quantity, entry.multiplier)?;
        Ok(Self::construct_quantity(entry.dimension, base_val))
    }

    /// Specialized Orchestrator: Parses a quantity and unit symbol but uses an
    /// EXTERNALLY provided multiplier (e.g. from AI Oracle resolution).
    /// Still verifies the unit dimension via the registry.
    pub fn parse_and_convert_with_multiplier(
        quantity: &str,
        unit: &str,
        multiplier: Decimal,
    ) -> Result<PhysicalQuantity, UnitError> {
        let entry = Self::resolve_symbol(unit)?;
        let base_val = Self::convert_to_base_val(quantity, multiplier)?;
        Ok(Self::construct_quantity(entry.dimension, base_val))
    }

    /// Resolves a unit symbol to its metadata.
    pub fn resolve_symbol(symbol: &str) -> Result<UnitRegistryEntry, UnitError> {
        let normalized = symbol.trim().to_lowercase();

        match normalized.as_str() {
            // --- Mass ---
            "g" => Ok(UnitRegistryEntry {
                symbol: "g",
                dimension: Dimension::Mass,
                multiplier: Decimal::ONE,
                is_contextual: false,
            }),
            "kg" => Ok(UnitRegistryEntry {
                symbol: "kg",
                dimension: Dimension::Mass,
                multiplier: Decimal::from(1000),
                is_contextual: false,
            }),
            "lb" | "lbs" => Ok(UnitRegistryEntry {
                symbol: "lb",
                dimension: Dimension::Mass,
                multiplier: Decimal::from_str("453.59237").unwrap(),
                is_contextual: false,
            }),
            "oz" => Ok(UnitRegistryEntry {
                symbol: "oz",
                dimension: Dimension::Mass,
                multiplier: Decimal::from_str("28.34952").unwrap(),
                is_contextual: false,
            }),

            // --- Volume ---
            "ml" => Ok(UnitRegistryEntry {
                symbol: "ml",
                dimension: Dimension::Volume,
                multiplier: Decimal::ONE,
                is_contextual: false,
            }),
            "l" => Ok(UnitRegistryEntry {
                symbol: "L",
                dimension: Dimension::Volume,
                multiplier: Decimal::from(1000),
                is_contextual: false,
            }),
            "gal" => Ok(UnitRegistryEntry {
                symbol: "gal",
                dimension: Dimension::Volume,
                multiplier: Decimal::from_str("3785.41178").unwrap(),
                is_contextual: false,
            }),
            "fl_oz" => Ok(UnitRegistryEntry {
                symbol: "fl_oz",
                dimension: Dimension::Volume,
                multiplier: Decimal::from_str("29.57353").unwrap(),
                is_contextual: false,
            }),

            // --- Count ---
            "units" | "unit" | "pc" | "pcs" => Ok(UnitRegistryEntry {
                symbol: "units",
                dimension: Dimension::Count,
                multiplier: Decimal::ONE,
                is_contextual: false,
            }),
            "dozens" | "dozen" => Ok(UnitRegistryEntry {
                symbol: "dozens",
                dimension: Dimension::Count,
                multiplier: Decimal::from(12),
                is_contextual: false,
            }),
            "packs" | "pack" => Ok(UnitRegistryEntry {
                symbol: "packs",
                dimension: Dimension::Count,
                multiplier: Decimal::ONE,
                is_contextual: true,
            }),

            // --- Anomalous ---
            "misc" | "handful" | "bunch" => Ok(UnitRegistryEntry {
                symbol: "misc",
                dimension: Dimension::Anomalous,
                multiplier: Decimal::ONE,
                is_contextual: true,
            }),

            _ => Err(UnitError::InvalidSymbol(normalized)),
        }
    }

    /// Performs the conversion to Base SI with mandatory Banker's Rounding.
    fn convert_to_base_val(quantity: &str, multiplier: Decimal) -> Result<Decimal, UnitError> {
        let qty = Decimal::from_str(quantity)
            .map_err(|_| UnitError::InvalidQuantity(quantity.to_string()))?;

        let result = qty
            .checked_mul(multiplier)
            .ok_or(UnitError::ArithmeticError)?;

        // Banker's Rounding is mandatory for SI stabilization (ADR 008)
        Ok(result.round_dp_with_strategy(4, RoundingStrategy::MidpointNearestEven))
    }

    /// Helper to wrap a raw Decimal value in the appropriate Dimension NewType.
    fn construct_quantity(dimension: Dimension, val: Decimal) -> PhysicalQuantity {
        match dimension {
            Dimension::Mass => PhysicalQuantity::Mass(Mass(val)),
            Dimension::Volume => PhysicalQuantity::Volume(Volume(val)),
            Dimension::Count => PhysicalQuantity::Count(Count(val)),
            Dimension::Anomalous => PhysicalQuantity::Anomalous(Anomalous(val)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse_and_convert {
        use super::*;

        #[test]
        fn converts_lb_to_g_with_bankers_rounding() {
            let res = UnitRegistry::parse_and_convert("2.0", "lb").unwrap();
            assert_eq!(res.dimension(), Dimension::Mass);
            // 2 * 453.59237 = 907.18474 -> rounded to 4dp = 907.1847
            assert_eq!(res.value().to_string(), "907.1847");
        }

        #[test]
        fn prevents_cumulative_bias_on_midpoint_values() {
            // MidpointNearestEven means 1.5 -> 2.0 and 2.5 -> 2.0 (rounding to 0 dp)
            let val = Decimal::from_str("1.5").unwrap();
            let rounded = val.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
            assert_eq!(rounded.to_string(), "2");

            let val2 = Decimal::from_str("2.5").unwrap();
            let rounded2 = val2.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
            assert_eq!(rounded2.to_string(), "2");
        }

        #[test]
        fn converts_gal_to_ml() {
            let res = UnitRegistry::parse_and_convert("1.0", "gal").unwrap();
            assert_eq!(res.dimension(), Dimension::Volume);
            assert_eq!(res.value().to_string(), "3785.4118"); // 3785.41178 rounded to 4dp
        }
    }

    mod parse_and_convert_with_multiplier {
        use super::*;

        #[test]
        fn applies_custom_multiplier_and_resolves_dimension() {
            // A "pack" of 6 units
            let res =
                UnitRegistry::parse_and_convert_with_multiplier("2", "pack", Decimal::from(6))
                    .unwrap();

            assert_eq!(res.dimension(), Dimension::Count);
            // 2 packs * 6 multiplier = 12 units
            assert_eq!(res.value().to_string(), "12");
        }

        #[test]
        fn applies_bankers_rounding_to_stabilized_result() {
            // Testing 1.23456 * 10 = 12.3456
            let res = UnitRegistry::parse_and_convert_with_multiplier(
                "1.23456",
                "units",
                Decimal::from(10),
            )
            .unwrap();

            // Banker's Rounding to 4dp: 12.3456
            assert_eq!(res.value().to_string(), "12.3456");
        }

        #[test]
        fn fails_on_invalid_unit_symbol() {
            let res =
                UnitRegistry::parse_and_convert_with_multiplier("1", "unknown_unit", Decimal::ONE);
            assert!(matches!(res, Err(UnitError::InvalidSymbol(_))));
        }
    }

    mod resolve_symbol {
        use super::*;

        #[test]
        fn parses_valid_mass_symbols() {
            let kg = UnitRegistry::resolve_symbol("kg").unwrap();
            assert_eq!(kg.dimension, Dimension::Mass);
            assert_eq!(kg.multiplier, Decimal::from(1000));
        }

        #[test]
        fn rejects_invalid_unit_symbols() {
            let res = UnitRegistry::resolve_symbol("invalid_unit");
            assert!(matches!(res, Err(UnitError::InvalidSymbol(_))));
        }
    }

    mod dimensional_fence {
        use super::*;

        #[test]
        fn allows_adding_same_dimension() {
            let q1 = UnitRegistry::parse_and_convert("1", "kg").unwrap(); // 1000g
            let q2 = UnitRegistry::parse_and_convert("500", "g").unwrap(); // 500g

            let sum = (q1 + q2).unwrap();
            assert_eq!(sum.dimension(), Dimension::Mass);
            assert_eq!(sum.value(), Decimal::from(1500));
        }

        #[test]
        fn rejects_adding_mass_and_volume() {
            let q1 = UnitRegistry::parse_and_convert("1", "kg").unwrap(); // Mass
            let q2 = UnitRegistry::parse_and_convert("1", "L").unwrap(); // Volume

            let res = q1 + q2;
            assert!(matches!(res, Err(UnitError::DimensionalMismatch)));
        }

        #[test]
        fn allows_subtracting_same_dimension() {
            let q1 = UnitRegistry::parse_and_convert("2", "L").unwrap(); // 2000ml
            let q2 = UnitRegistry::parse_and_convert("500", "ml").unwrap(); // 500ml

            let diff = (q1 - q2).unwrap();
            assert_eq!(diff.dimension(), Dimension::Volume);
            assert_eq!(diff.value(), Decimal::from(1500));
        }
    }
}
