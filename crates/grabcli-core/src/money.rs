//! Money as integer minor units (centavos). Display adds the ₱ symbol and
//! thousands separators.

use serde::{Deserialize, Serialize};

/// An amount in minor units (1/100 of a peso).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Money(pub i64);

impl Money {
    pub const ZERO: Money = Money(0);

    pub fn minor(self) -> i64 {
        self.0
    }

    /// Major units as an f64 (for JSON `total_php`-style output). Lossy for
    /// huge values but fine for prices.
    pub fn as_major_f64(self) -> f64 {
        self.0 as f64 / 100.0
    }

    /// `₱1,234.50`. Negative amounts use a minus sign before the symbol.
    pub fn peso(self) -> String {
        format!(
            "{}₱{}",
            if self.0 < 0 { "-" } else { "" },
            group(self.0.abs())
        )
    }

    /// `1,234.50` with no currency symbol.
    pub fn plain(self) -> String {
        format!(
            "{}{}",
            if self.0 < 0 { "-" } else { "" },
            group(self.0.abs())
        )
    }

    /// Major-unit number with up to 2 decimals and no trailing zeros
    /// (`440`, `909.5`, `1003.25`) for vault notes.
    pub fn trimmed_major(self) -> String {
        let neg = self.0 < 0;
        let abs = self.0.abs();
        let whole = abs / 100;
        let cents = abs % 100;
        let body = if cents == 0 {
            whole.to_string()
        } else if cents % 10 == 0 {
            format!("{whole}.{}", cents / 10)
        } else {
            format!("{whole}.{cents:02}")
        };
        if neg { format!("-{body}") } else { body }
    }
}

impl core::ops::Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0 + rhs.0)
    }
}

impl core::ops::Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0 - rhs.0)
    }
}

/// Format minor units with a thousands separator and two decimals.
fn group(minor_abs: i64) -> String {
    let whole = minor_abs / 100;
    let cents = minor_abs % 100;
    let digits = whole.to_string();
    let mut out = String::new();
    let bytes = digits.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    format!("{out}.{cents:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peso_formatting() {
        assert_eq!(Money(57100).peso(), "₱571.00");
        assert_eq!(Money(100000).peso(), "₱1,000.00");
        assert_eq!(Money(123456789).peso(), "₱1,234,567.89");
        assert_eq!(Money(-4900).peso(), "-₱49.00");
        assert_eq!(Money(0).peso(), "₱0.00");
        assert_eq!(Money(5).peso(), "₱0.05");
    }

    #[test]
    fn trimmed_major_strips_zeros() {
        assert_eq!(Money(44000).trimmed_major(), "440");
        assert_eq!(Money(90950).trimmed_major(), "909.5");
        assert_eq!(Money(100325).trimmed_major(), "1003.25");
        assert_eq!(Money(81250).trimmed_major(), "812.5");
        assert_eq!(Money(9991).trimmed_major(), "99.91");
    }

    #[test]
    fn arithmetic() {
        assert_eq!(
            (Money(44000) + Money(18000) - Money(4900)).peso(),
            "₱571.00"
        );
    }
}
