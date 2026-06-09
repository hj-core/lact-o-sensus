//! Universal SI unit registry and stabilization for the Lact-O-Sensus cluster.
//!
//! This module implements the physical quantity normalization mandated by ADR
//! 008. It ensures that all grocery state values (mass, volume, count) are
//! converted to a stable, deterministic internal representation (g, ml, units)
//! using Banker's Rounding (Midpoint-Nearest-Even) to prevent cumulative
//! statistical bias.
//!
//! The "Dimensional Fence" enforces that arithmetic operations are only
//! permitted between quantities of the same physical dimension.

use std::ops::Add;
use std::ops::Sub;
use std::str::FromStr;

use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;
use strum::Display;
use strum::EnumString;
use thiserror::Error;
use tracing::instrument;

/// Errors associated with physical quantity parsing and stabilization.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UnitError {
    /// The provided unit symbol is not recognized by the registry.
    #[error("Invalid unit symbol: {0}")]
    InvalidSymbol(String),

    /// An arithmetic operation was attempted between mismatched dimensions.
    #[error("Dimensional mismatch: operation not permitted across dimensions")]
    DimensionalMismatch,

    /// The provided quantity string could not be parsed as a decimal.
    #[error("Invalid quantity format: {0}")]
    InvalidQuantity(String),

    /// An arithmetic operation resulted in an overflow or underflow.
    #[error("Arithmetic overflow or underflow")]
    ArithmeticError,
}

/// The four physical dimensions authorized by ADR 008.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Dimension {
    /// Quantities of matter (Base: grams).
    Mass,
    /// Quantities of space (Base: milliliters).
    Volume,
    /// Discrete items (Base: units).
    Count,
    /// Unstructured or non-standard measurements.
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

/// A physical quantity representing Mass, stored in grams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mass(pub Decimal);

/// A physical quantity representing Volume, stored in milliliters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume(pub Decimal);

/// A physical quantity representing Count, stored in discrete units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count(pub Decimal);

/// A physical quantity representing unstructured measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anomalous(pub Decimal);

/// A self-validating wrapper for authorized physical quantities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalQuantity {
    /// Mass dimension (g).
    Mass(Mass),
    /// Volume dimension (ml).
    Volume(Volume),
    /// Count dimension (units).
    Count(Count),
    /// Anomalous dimension (misc).
    Anomalous(Anomalous),
}

impl PhysicalQuantity {
    /// Returns the underlying decimal value.
    pub fn value(&self) -> Decimal {
        match self {
            Self::Mass(m) => m.0,
            Self::Volume(v) => v.0,
            Self::Count(c) => c.0,
            Self::Anomalous(a) => a.0,
        }
    }

    /// Returns the dimension of this quantity.
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

    /// Performs dimension-aware addition. Returns an error on mismatch.
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

    /// Performs dimension-aware subtraction. Returns an error on mismatch.
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

/// Metadata for a unit symbol supported by the registry.
#[derive(Debug, Clone)]
pub struct UnitRegistryEntry {
    /// The canonical symbol for the unit.
    pub symbol: &'static str,
    /// The dimension this unit belongs to.
    pub dimension: Dimension,
    /// The conversion factor to reach the SI base unit.
    pub multiplier: Decimal,
    /// Whether the multiplier is item-contextual rather than physical.
    pub is_contextual: bool,
}

/// A centralized registry for physical unit definitions and conversions.
pub struct UnitRegistry;

// --- Physical Conversion Constants (ADR 008) ---
// Raw conversion values sourced from NIST/International Standards.
const MULTIPLIER_LB: Decimal = Decimal::from_parts(45359237, 0, 0, false, 5); // 453.59237 (lb -> g)
const MULTIPLIER_OZ: Decimal = Decimal::from_parts(2834952, 0, 0, false, 5); // 28.34952 (oz -> g)
const MULTIPLIER_GAL: Decimal = Decimal::from_parts(378541178, 0, 0, false, 5); // 3785.41178 (gal -> ml)
const MULTIPLIER_FL_OZ: Decimal = Decimal::from_parts(2957353, 0, 0, false, 5); // 29.57353 (fl_oz -> ml)

impl UnitRegistry {
    /// High-level Orchestrator: Parses a quantity and unit symbol into a
    /// validated, stabilized `PhysicalQuantity`.
    #[instrument(
        name = "physical_stabilization",
        target = "clinical::fsm",
        skip_all,
        fields(raw_qty = %quantity, raw_unit = %unit)
    )]
    pub fn parse_and_convert(quantity: &str, unit: &str) -> Result<PhysicalQuantity, UnitError> {
        let entry = Self::resolve_symbol(unit)?;
        let base_val = Self::convert_to_base_val(quantity, entry.multiplier)?;
        Ok(Self::construct_quantity(entry.dimension, base_val))
    }

    /// Specialized Orchestrator: Parses a quantity and unit symbol but uses an
    /// EXTERNALLY provided multiplier (e.g. from AI Oracle resolution).
    /// Still verifies the unit dimension via the registry.
    #[instrument(
        name = "contextual_stabilization",
        target = "clinical::fsm",
        skip_all,
        fields(raw_qty = %quantity, raw_unit = %unit)
    )]
    pub fn parse_and_convert_with_multiplier(
        quantity: &str,
        unit: &str,
        multiplier: Decimal,
    ) -> Result<PhysicalQuantity, UnitError> {
        let entry = Self::resolve_symbol(unit)?;
        let base_val = Self::convert_to_base_val(quantity, multiplier)?;
        Ok(Self::construct_quantity(entry.dimension, base_val))
    }

    /// Resolves a unit symbol to its metadata. Returns an error if unknown.
    #[instrument(
        name = "unit_resolution",
        target = "clinical::fsm",
        skip_all,
        fields(symbol = %symbol)
    )]
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
                multiplier: MULTIPLIER_LB,
                is_contextual: false,
            }),
            "oz" => Ok(UnitRegistryEntry {
                symbol: "oz",
                dimension: Dimension::Mass,
                multiplier: MULTIPLIER_OZ,
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
                multiplier: MULTIPLIER_GAL,
                is_contextual: false,
            }),
            "fl_oz" => Ok(UnitRegistryEntry {
                symbol: "fl_oz",
                dimension: Dimension::Volume,
                multiplier: MULTIPLIER_FL_OZ,
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
    #[instrument(
        name = "base_conversion",
        target = "clinical::fsm",
        level = "debug",
        skip_all,
        fields(qty = %quantity)
    )]
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

    /// Reverse-converts a base SI quantity back to a display unit.
    ///
    /// This enables the QueryState response to present quantities in the user's
    /// preferred unit (e.g., returning `"lb"` instead of `"g"`).
    /// Contextual units (e.g., `"pack"`) cannot be reversed because the
    /// AI-provided multiplier is not stored.
    #[instrument(
        name = "display_conversion",
        target = "clinical::fsm",
        skip_all,
        fields(base_qty = %base_quantity, display_unit = %display_unit)
    )]
    pub fn convert_to_display_value(base_quantity: &str, display_unit: &str) -> Option<String> {
        let entry = Self::resolve_symbol(display_unit).ok()?;
        if entry.is_contextual {
            return None;
        }
        let base = Decimal::from_str(base_quantity).ok()?;
        let result = base
            .checked_div(entry.multiplier)
            .map(|r| r.round_dp_with_strategy(4, RoundingStrategy::MidpointNearestEven))?;
        Some(result.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse_and_convert {
        use super::*;

        mod with_si_base_units {
            use super::*;
            #[test]
            fn returns_stabilized_quantity_when_unit_is_mass() {
                let res = UnitRegistry::parse_and_convert("2.0", "lb").unwrap();
                assert_eq!(res.dimension(), Dimension::Mass);
                // 2 * 453.59237 = 907.18474 -> rounded to 4dp = 907.1847
                assert_eq!(res.value().to_string(), "907.1847");
            }

            #[test]
            fn returns_stabilized_quantity_when_unit_is_volume() {
                let res = UnitRegistry::parse_and_convert("1.0", "gal").unwrap();
                assert_eq!(res.dimension(), Dimension::Volume);
                assert_eq!(res.value().to_string(), "3785.4118"); // 3785.41178 rounded to 4dp
            }

            #[test]
            fn returns_stabilized_quantity_when_unit_is_ounces() {
                let res = UnitRegistry::parse_and_convert("1.0", "oz").unwrap();
                assert_eq!(res.dimension(), Dimension::Mass);
                // 1 * 28.34952 = 28.34952 -> rounded to 4dp = 28.3495
                assert_eq!(res.value().to_string(), "28.3495");
            }

            #[test]
            fn returns_stabilized_quantity_when_unit_is_fluid_ounces() {
                let res = UnitRegistry::parse_and_convert("1.0", "fl_oz").unwrap();
                assert_eq!(res.dimension(), Dimension::Volume);
                // 1 * 29.57353 = 29.57353 -> rounded to 4dp = 29.5735
                assert_eq!(res.value().to_string(), "29.5735");
            }
        }

        mod with_bankers_rounding {
            use super::*;
            #[test]
            fn rounds_to_nearest_even_on_midpoint_values() {
                // MidpointNearestEven means 1.5 -> 2.0 and 2.5 -> 2.0 (rounding to 0 dp)
                let val = Decimal::from_str("1.5").unwrap();
                let rounded = val.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
                assert_eq!(rounded.to_string(), "2");

                let val2 = Decimal::from_str("2.5").unwrap();
                let rounded2 =
                    val2.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
                assert_eq!(rounded2.to_string(), "2");
            }
        }
    }

    mod parse_and_convert_with_multiplier {
        use super::*;

        mod with_contextual_multiplier {
            use super::*;
            #[test]
            fn returns_count_when_resolving_pack_size() {
                // A "pack" of 6 units
                let res =
                    UnitRegistry::parse_and_convert_with_multiplier("2", "pack", Decimal::from(6))
                        .unwrap();

                assert_eq!(res.dimension(), Dimension::Count);
                // 2 packs * 6 multiplier = 12 units
                assert_eq!(res.value().to_string(), "12");
            }
        }

        mod with_fractional_input {
            use super::*;
            #[test]
            fn applies_rounding_to_stabilized_result() {
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
        }

        mod with_invalid_input {
            use super::*;
            #[test]
            fn returns_error_when_symbol_is_unknown() {
                let res = UnitRegistry::parse_and_convert_with_multiplier(
                    "1",
                    "unknown_unit",
                    Decimal::ONE,
                );
                assert!(matches!(res, Err(UnitError::InvalidSymbol(_))));
            }
        }
    }

    mod resolve_symbol {
        use super::*;

        mod with_valid_symbols {
            use super::*;
            #[test]
            fn returns_metadata_when_symbol_is_mass() {
                let kg = UnitRegistry::resolve_symbol("kg").unwrap();
                assert_eq!(kg.dimension, Dimension::Mass);
                assert_eq!(kg.multiplier, Decimal::from(1000));
            }
        }

        mod with_invalid_symbols {
            use super::*;
            #[test]
            fn returns_error_when_symbol_is_malformed() {
                let res = UnitRegistry::resolve_symbol("invalid_unit");
                assert!(matches!(res, Err(UnitError::InvalidSymbol(_))));
            }
        }
    }

    mod convert_to_display_value {
        use super::*;
        #[test]
        fn converts_base_si_to_display_unit_for_mass() {
            // 907.1847 g -> lb
            let result = UnitRegistry::convert_to_display_value("907.1847", "lb");
            assert_eq!(result, Some("2.0000".to_string()));
        }

        #[test]
        fn converts_base_si_to_display_unit_for_volume() {
            // 3785.4118 ml -> gal
            let result = UnitRegistry::convert_to_display_value("3785.4118", "gal");
            assert_eq!(result, Some("1.0000".to_string()));
        }

        #[test]
        fn returns_none_for_contextual_units() {
            let result = UnitRegistry::convert_to_display_value("12", "pack");
            assert!(result.is_none());
        }

        #[test]
        fn returns_none_for_unknown_display_unit() {
            let result = UnitRegistry::convert_to_display_value("100", "blorgs");
            assert!(result.is_none());
        }
    }

    mod dimensional_fence {
        use super::*;

        mod addition {
            use super::*;
            #[test]
            fn returns_success_when_dimensions_match() {
                let q1 = UnitRegistry::parse_and_convert("1", "kg").unwrap(); // 1000g
                let q2 = UnitRegistry::parse_and_convert("500", "g").unwrap(); // 500g

                let sum = (q1 + q2).unwrap();
                assert_eq!(sum.dimension(), Dimension::Mass);
                assert_eq!(sum.value(), Decimal::from(1500));
            }

            #[test]
            fn returns_error_when_dimensions_mismatch() {
                let q1 = UnitRegistry::parse_and_convert("1", "kg").unwrap(); // Mass
                let q2 = UnitRegistry::parse_and_convert("1", "L").unwrap(); // Volume

                let res = q1 + q2;
                assert!(matches!(res, Err(UnitError::DimensionalMismatch)));
            }
        }

        mod subtraction {
            use super::*;
            #[test]
            fn returns_success_when_dimensions_match() {
                let q1 = UnitRegistry::parse_and_convert("2", "L").unwrap(); // 2000ml
                let q2 = UnitRegistry::parse_and_convert("500", "ml").unwrap(); // 500ml

                let diff = (q1 - q2).unwrap();
                assert_eq!(diff.dimension(), Dimension::Volume);
                assert_eq!(diff.value(), Decimal::from(1500));
            }
        }
    }
}
