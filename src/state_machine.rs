//! The invoice state machine. `allowed_from` is the single source of truth for
//! which public actions are legal from which states.
//!
//! ```text
//! open ──pay──▶ locked ──PSP success──▶ paid (terminal)
//!   ▲             │  ▲
//!   └─declined────┘  └──pay── uncollectible ◀──mark_uncollectible── open
//!                    declined ──▶ uncollectible
//! open / uncollectible ──void──▶ void (terminal)
//! ```
//!
//! The moves out of `locked` are not public actions. Only the pay handler and
//! the reconciler make them, always with `WHERE state = 'locked'`.

use axum::http::StatusCode;
use serde_json::json;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceState {
    Open,
    /// A payment attempt is in flight. Nothing else may touch the invoice.
    Locked,
    Uncollectible,
    Paid,
    Void,
}

impl InvoiceState {
    pub const ALL: [InvoiceState; 5] = [
        InvoiceState::Open,
        InvoiceState::Locked,
        InvoiceState::Uncollectible,
        InvoiceState::Paid,
        InvoiceState::Void,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            InvoiceState::Open => "open",
            InvoiceState::Locked => "locked",
            InvoiceState::Uncollectible => "uncollectible",
            InvoiceState::Paid => "paid",
            InvoiceState::Void => "void",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == s)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, InvoiceState::Paid | InvoiceState::Void)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pay,
    Void,
    MarkUncollectible,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Pay => "pay",
            Action::Void => "void",
            Action::MarkUncollectible => "mark_uncollectible",
        }
    }
}

/// States from which each public action is allowed.
pub fn allowed_from(action: Action) -> &'static [InvoiceState] {
    use InvoiceState::*;
    match action {
        Action::Pay => &[Open, Uncollectible],
        Action::Void => &[Open, Uncollectible],
        Action::MarkUncollectible => &[Open],
    }
}

/// The same list as strings, for `WHERE state = ANY($n)` in SQL.
pub fn allowed_from_db(action: Action) -> Vec<String> {
    allowed_from(action).iter().map(|s| s.as_str().to_string()).collect()
}

pub fn is_allowed(action: Action, from: InvoiceState) -> bool {
    allowed_from(action).contains(&from)
}

/// The 409 returned when `action` is not allowed from `current`.
pub fn rejection(current: InvoiceState, action: Action) -> AppError {
    let details = json!({ "current_state": current.as_str(), "action": action.as_str() });
    let err = match (current, action) {
        (InvoiceState::Locked, _) => AppError::new(
            StatusCode::CONFLICT,
            "payment_in_progress",
            "A payment for this invoice is in progress. Retry once it has finished.",
        ),
        (InvoiceState::Paid, Action::Pay) => AppError::new(
            StatusCode::CONFLICT,
            "invoice_already_paid",
            "This invoice has already been paid.",
        ),
        _ => AppError::new(
            StatusCode::CONFLICT,
            "invalid_state_transition",
            format!(
                "Cannot {} an invoice in state '{}'.",
                action.as_str(),
                current.as_str()
            ),
        ),
    };
    err.with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_and_locked_states_allow_no_public_action() {
        for state in [InvoiceState::Paid, InvoiceState::Void, InvoiceState::Locked] {
            for action in [Action::Pay, Action::Void, Action::MarkUncollectible] {
                assert!(!is_allowed(action, state), "{action:?} from {state:?}");
            }
        }
    }

    #[test]
    fn uncollectible_can_still_be_paid_or_voided_but_not_reopened() {
        assert!(is_allowed(Action::Pay, InvoiceState::Uncollectible));
        assert!(is_allowed(Action::Void, InvoiceState::Uncollectible));
        assert!(!is_allowed(Action::MarkUncollectible, InvoiceState::Uncollectible));
    }

    #[test]
    fn only_paid_and_void_are_terminal() {
        let terminal: Vec<_> = InvoiceState::ALL.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(terminal, vec![InvoiceState::Paid, InvoiceState::Void]);
    }
}
