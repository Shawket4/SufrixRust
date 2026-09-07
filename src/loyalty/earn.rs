//! What a sale is worth in points.
//!
//! Pure and dependency-free on purpose: this is the one piece of the program a
//! customer will argue about at the counter, and it is exercised from three
//! call sites (live checkout, `/sync/replay` of an offline till, and the
//! dashboard's "what would this earn?" preview). Keeping it a function of plain
//! integers means the same sale is worth the same points down every path.
//!
//! Money is piastres everywhere, as it is throughout the schema. The dashboard
//! renders EGP with the existing `piastresToEgp` / `fmtMoney` helpers.

/// The parts of an order the rule may be applied to. All piastres.
#[derive(Debug, Clone, Copy)]
pub struct OrderAmounts {
    pub subtotal: i32,
    pub discount_amount: i32,
    pub tax_amount: i32,
}

/// What a scope collects. One or the other, never both — a card that counted two
/// things at once needs two progress lines and two answers to "how close am I".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Points from money spent, by the rule below.
    Points,
    /// One stamp per sale, whatever its size ("5 orders, free coffee").
    Visits,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Points => "points",
            Mode::Visits => "visits",
        }
    }

    /// Unknown values read as points — the default, and the mode a program
    /// starts in. A settings row can only hold the two the CHECK allows.
    pub fn parse(s: &str) -> Self {
        match s {
            "visits" => Mode::Visits,
            _ => Mode::Points,
        }
    }
}

/// The earn rule, resolved for the branch that made the sale.
#[derive(Debug, Clone, Copy)]
pub struct EarnRule {
    pub mode: Mode,
    /// One point per this many piastres. 1000 = a point per 10 EGP.
    /// Ignored in [`Mode::Visits`].
    pub piastres_per_point: i32,
    /// Earn on what the customer actually paid rather than the list value.
    pub on_discounted: bool,
    /// Add tax to the basis. Off by default — tax is remitted, not revenue.
    pub include_tax: bool,
}

/// The piastres the rule applies to, before conversion.
///
/// Tips are absent by construction: they are the staff's money, not a sale, and
/// there is no toggle that lets them earn.
pub fn basis_piastres(a: OrderAmounts, r: EarnRule) -> i32 {
    let mut basis = a.subtotal;
    if r.on_discounted {
        basis -= a.discount_amount;
    }
    if r.include_tax {
        basis += a.tax_amount;
    }
    // A discount larger than the subtotal (a comped order) must not earn
    // negative points, and must not panic the checkout path.
    basis.max(0)
}

/// Points earned by a sale. Rounds DOWN: a customer never earns a point for
/// money they did not spend, and rounding up would let a stream of tiny sales
/// mint points out of nothing.
///
/// Returns 0 when the program is off for the branch, when the sale is too small
/// to reach one point, or on a nonsensical rate (defensive: the column is
/// `CHECK (> 0)`, but a zero here would divide by zero at a till).
pub fn points_for(a: OrderAmounts, r: EarnRule) -> i32 {
    match r.mode {
        // A stamp is a stamp: one per sale, however large. There is deliberately
        // no minimum — a shop that wants one is asking for a different feature,
        // and a silent floor would be a rule customers could not see.
        Mode::Visits => 1,
        Mode::Points => {
            if r.piastres_per_point <= 0 {
                return 0;
            }
            basis_piastres(a, r) / r.piastres_per_point
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: EarnRule = EarnRule {
        mode: Mode::Points,
        piastres_per_point: 1000, // a point per 10 EGP
        on_discounted: true,
        include_tax: false,
    };
    const STAMPS: EarnRule = EarnRule {
        mode: Mode::Visits,
        ..RULE
    };

    fn amounts(subtotal: i32, discount: i32, tax: i32) -> OrderAmounts {
        OrderAmounts {
            subtotal,
            discount_amount: discount,
            tax_amount: tax,
        }
    }

    #[test]
    fn earns_a_point_per_ten_pounds() {
        // 130 EGP subtotal, no discount → 13 points.
        assert_eq!(points_for(amounts(13_000, 0, 1_820), RULE), 13);
    }

    #[test]
    fn rounds_down_never_up() {
        // 19.99 EGP → 1 point, not 2. The 9.99 left over is not a point.
        assert_eq!(points_for(amounts(1_999, 0, 0), RULE), 1);
        // Below the first point earns nothing at all.
        assert_eq!(points_for(amounts(999, 0, 0), RULE), 0);
    }

    #[test]
    fn discount_toggle_changes_the_basis() {
        let a = amounts(10_000, 4_000, 0);
        assert_eq!(points_for(a, RULE), 6); // paid 60 EGP
        let list_price = EarnRule {
            on_discounted: false,
            ..RULE
        };
        assert_eq!(points_for(a, list_price), 10); // menu value, 100 EGP
    }

    #[test]
    fn tax_toggle_changes_the_basis() {
        let a = amounts(10_000, 0, 1_400);
        assert_eq!(points_for(a, RULE), 10);
        let with_tax = EarnRule {
            include_tax: true,
            ..RULE
        };
        assert_eq!(points_for(a, with_tax), 11);
    }

    #[test]
    fn a_comped_order_earns_nothing_and_never_goes_negative() {
        // Discount exceeding the subtotal must not mint negative points.
        assert_eq!(basis_piastres(amounts(5_000, 9_000, 0), RULE), 0);
        assert_eq!(points_for(amounts(5_000, 9_000, 0), RULE), 0);
    }

    #[test]
    fn a_zero_rate_earns_nothing_rather_than_dividing_by_zero() {
        let broken = EarnRule {
            piastres_per_point: 0,
            ..RULE
        };
        assert_eq!(points_for(amounts(10_000, 0, 0), broken), 0);
    }

    #[test]
    fn a_stamp_is_one_per_sale_whatever_the_size() {
        // The whole point of the visits mode: the bill does not matter.
        assert_eq!(points_for(amounts(500, 0, 0), STAMPS), 1);
        assert_eq!(points_for(amounts(500_000, 0, 0), STAMPS), 1);
        // Even a fully comped order is a visit — the customer came in.
        assert_eq!(points_for(amounts(5_000, 9_000, 0), STAMPS), 1);
    }

    #[test]
    fn the_points_rate_is_ignored_in_visits_mode() {
        let odd = EarnRule {
            piastres_per_point: 1,
            ..STAMPS
        };
        assert_eq!(points_for(amounts(13_000, 0, 0), odd), 1);
    }

    #[test]
    fn tips_can_never_earn() {
        // Tips are not part of `OrderAmounts` at all — the only way a tip could
        // earn is if someone added a field here, which this test exists to make
        // a deliberate act rather than an accident.
        let generous = amounts(10_000, 0, 0);
        assert_eq!(points_for(generous, RULE), 10);
    }
}
