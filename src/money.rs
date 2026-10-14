//! Money is always integer cents (`i64`). No floats anywhere in this path.
//! The client never sends a total. We compute it here with overflow checks.

use serde::{Deserialize, Serialize};

use crate::error::AppError;

const MAX_LINE_ITEMS: usize = 100;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LineItemInput {
    pub description: String,
    /// JSON like `2.5` fails to deserialize into an integer, so fractional
    /// quantities and amounts are rejected with 400 before reaching this code.
    pub quantity: i32,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PricedLineItem {
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

pub fn price_line_items(items: &[LineItemInput]) -> Result<(Vec<PricedLineItem>, i64), AppError> {
    if items.is_empty() {
        return Err(AppError::bad_request("invalid_request", "line_items must not be empty."));
    }
    if items.len() > MAX_LINE_ITEMS {
        return Err(AppError::bad_request(
            "invalid_request",
            format!("An invoice can have at most {MAX_LINE_ITEMS} line items."),
        ));
    }

    let mut total: i64 = 0;
    let mut priced = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let description = item.description.trim();
        if description.is_empty() || description.len() > 500 {
            return Err(AppError::bad_request(
                "invalid_request",
                format!("line_items[{i}].description must be 1 to 500 characters."),
            ));
        }
        if item.quantity <= 0 {
            return Err(AppError::bad_request(
                "invalid_request",
                format!("line_items[{i}].quantity must be greater than 0."),
            ));
        }
        if item.unit_amount_cents < 0 {
            return Err(AppError::bad_request(
                "invalid_request",
                format!("line_items[{i}].unit_amount_cents must not be negative."),
            ));
        }
        let amount = i64::from(item.quantity)
            .checked_mul(item.unit_amount_cents)
            .ok_or_else(too_large)?;
        total = total.checked_add(amount).ok_or_else(too_large)?;
        priced.push(PricedLineItem {
            description: description.to_string(),
            quantity: item.quantity,
            unit_amount_cents: item.unit_amount_cents,
            amount_cents: amount,
        });
    }
    Ok((priced, total))
}

fn too_large() -> AppError {
    AppError::new(
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "amount_too_large",
        "The invoice total does not fit in a 64-bit number of cents.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(quantity: i32, unit: i64) -> LineItemInput {
        LineItemInput { description: "x".into(), quantity, unit_amount_cents: unit }
    }

    #[test]
    fn computes_total_in_cents() {
        let (priced, total) = price_line_items(&[item(3, 1250), item(1, 5000)]).unwrap();
        assert_eq!(total, 8750);
        assert_eq!(priced[0].amount_cents, 3750);
    }

    #[test]
    fn rejects_overflow_instead_of_wrapping() {
        let err = price_line_items(&[item(2, i64::MAX)]).unwrap_err();
        assert_eq!(err.code, "amount_too_large");
        let err = price_line_items(&[item(1, i64::MAX), item(1, 1)]).unwrap_err();
        assert_eq!(err.code, "amount_too_large");
    }

    #[test]
    fn rejects_bad_items() {
        assert!(price_line_items(&[]).is_err());
        assert!(price_line_items(&[item(0, 100)]).is_err());
        assert!(price_line_items(&[item(1, -1)]).is_err());
    }
}
