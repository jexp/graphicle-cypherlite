//! Cooperative query cancellation (ADR-0111).
//!
//! A [`CancelToken`] is a shared flag. A caller (for example a transport
//! enforcing a query timeout or handling a disconnect) holds one handle and
//! cancels it; the thread executing the query holds a cloned handle and stops
//! at its next cancellation check with a typed error whose message contains
//! `"query cancelled"`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cooperative cancellation flag shared between a query's caller and the
/// worker executing it (ADR-0111).
///
/// Cloning shares the same underlying flag, so a transport can cancel a query
/// it did not start. A `CancelToken` that is never cancelled lets a query run
/// to completion, which is how [`crate::Error`]'s callers that do not care
/// about cancellation use it.
///
/// ```
/// use cypherlite_core::CancelToken;
///
/// let token = CancelToken::new();
/// assert!(!token.is_cancelled());
/// let transport = token.clone();
/// transport.cancel();
/// assert!(token.is_cancelled());
/// ```
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// A fresh token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel the query sharing this token. Idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether [`Self::cancel`] has been called on this token or any clone.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_the_flag() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
        clone.cancel();
        assert!(token.is_cancelled());
    }
}
