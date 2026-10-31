//! Schedule v1. The logical workspace is the owned resident memory a query
//! holds, measured in logical bytes. The pool reserves that memory in
//! [`CHARGE_QUANTUM_BYTES`] steps, so every reservation and every receipt
//! peak is a multiple of the quantum. With no ledger installed, every
//! charge is a no-op so admission can stay off.

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::{Error, Result};

/// Published schedule name.
pub const SCHEDULE_V1: &str = "v1";

/// The reservation quantum. Owned workspace is charged in whole quanta, so
/// one quantum is the smallest non-zero reservation.
pub const CHARGE_QUANTUM_BYTES: u64 = 4096;
/// Every admitted statement reserves at least this many logical bytes.
pub const PEAK_ADMISSION_FLOOR: u64 = CHARGE_QUANTUM_BYTES;
/// New admissions leave this fraction of the pool for queries already
/// running. `capacity / ADMISSION_TAIL_DIVISOR` is the tail.
pub const ADMISSION_TAIL_DIVISOR: u64 = 8;

/// Why a receipt was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenOutcome {
    Success,
    CapacityExceeded,
}

impl TokenOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::CapacityExceeded => "capacity-exceeded",
        }
    }
}

/// Logical bytes recorded for a plan. `PROFILE` fills this from the receipt.
/// Admission does not estimate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenEstimate {
    pub peak_logical_bytes: u64,
    pub token_schedule: &'static str,
}

/// Logical workspace bytes a statement actually held at its high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionReceipt {
    pub peak_logical_bytes: u64,
    pub token_schedule: &'static str,
    pub outcome: TokenOutcome,
}

impl ExecutionReceipt {
    pub fn empty(outcome: TokenOutcome) -> Self {
        Self {
            peak_logical_bytes: 0,
            token_schedule: SCHEDULE_V1,
            outcome,
        }
    }
}

/// Reserved logical bytes for `bytes` of owned workspace. Zero bytes is zero.
/// Any positive size rounds up to the next whole quantum, at least one.
pub fn rounded_logical_bytes(bytes: u64) -> u64 {
    if bytes == 0 {
        0
    } else {
        bytes
            .div_ceil(CHARGE_QUANTUM_BYTES)
            .saturating_mul(CHARGE_QUANTUM_BYTES)
    }
}

/// Tokens a new admission must leave free. Running growth may spend them.
pub fn admission_tail(capacity: u64) -> u64 {
    capacity / ADMISSION_TAIL_DIVISOR
}

/// Grows and releases the process-wide peak reservation.
pub trait PeakPool: Send + Sync {
    fn try_grow(&self, extra: u64) -> bool;
    fn release(&self, amount: u64);
}

/// Installed on the query worker for one admitted statement.
pub struct TokenLedger {
    /// Owned workspace still retained.
    current_bytes: u64,
    /// Highest `current_bytes` reached. A shrink does not lower it.
    peak_bytes: u64,
    /// `peak_bytes` rounded to the quantum. The receipt reports this.
    peak_reserved: u64,
    /// Logical bytes reserved in the pool. The admission cost is a floor
    /// until the live workspace passes it. After that this follows the live
    /// workspace.
    held: u64,
    /// Logical bytes reserved at admission. A floor only until the live
    /// workspace passes it.
    admitted: u64,
    capacity: u64,
    pool: Arc<dyn PeakPool>,
    sticky: Option<Error>,
    /// The processing-memory calibration hook pauses at most once per
    /// statement. It is inert unless CYPHERLITE_PEAK_HOLD_MS is set.
    peak_hold_done: bool,
}

impl fmt::Debug for TokenLedger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenLedger")
            .field("current_bytes", &self.current_bytes)
            .field("peak_bytes", &self.peak_bytes)
            .field("peak_reserved", &self.peak_reserved)
            .field("held", &self.held)
            .field("admitted", &self.admitted)
            .field("capacity", &self.capacity)
            .field("sticky", &self.sticky)
            .field("peak_hold_done", &self.peak_hold_done)
            .finish_non_exhaustive()
    }
}

impl TokenLedger {
    pub fn new(capacity: u64, reserved: u64, pool: Arc<dyn PeakPool>) -> Self {
        Self {
            current_bytes: 0,
            peak_bytes: 0,
            peak_reserved: 0,
            held: reserved,
            admitted: reserved,
            capacity,
            pool,
            sticky: None,
            peak_hold_done: false,
        }
    }

    pub fn receipt(&self, outcome: TokenOutcome) -> ExecutionReceipt {
        ExecutionReceipt {
            peak_logical_bytes: self.peak_reserved,
            token_schedule: SCHEDULE_V1,
            outcome,
        }
    }

    fn refuse(
        &mut self,
        name: &'static str,
        limit: u64,
        retryable: bool,
        outcome: TokenOutcome,
    ) -> Error {
        let error =
            Error::resource_limit_with_receipt(name, limit, retryable, self.receipt(outcome));
        if self.sticky.is_none() {
            self.sticky = Some(error.clone());
        }
        error
    }

    /// Logical bytes the pool should hold for `bytes` of current workspace.
    ///
    /// The admission cost stays reserved until the live workspace passes
    /// it. After a successful measurement above that cost, the pool
    /// follows the live workspace, including below the original
    /// reservation. The receipt peak does not follow it down.
    fn reservation_for(&self, bytes: u64) -> u64 {
        let live = rounded_logical_bytes(bytes);
        if self.peak_reserved > self.admitted {
            live
        } else {
            live.max(self.admitted)
        }
    }

    /// Move the current workspace from `from` bytes to `to` bytes.
    ///
    /// Growth checks the extra tokens against the shared pool. A refusal
    /// leaves the ledger untouched. A shrink releases tokens down to the
    /// admission reservation until the live workspace has passed it, and
    /// does not lower the receipt, including after a sticky error.
    fn reaccount(&mut self, from: u64, to: u64) -> Result<()> {
        let current = self.current_bytes.saturating_sub(from).saturating_add(to);
        let peak = self.peak_bytes.max(current);
        let peak_reserved = rounded_logical_bytes(peak);
        let needed = self.reservation_for(current);
        let raises_peak = peak_reserved > self.peak_reserved;
        let raises_hold = needed > self.held;
        if raises_peak || raises_hold {
            if let Some(error) = self.sticky.clone() {
                return Err(error);
            }
            if raises_hold {
                let extra = needed - self.held;
                if !self.pool.try_grow(extra) {
                    return Err(self.refuse(
                        Error::CONCURRENT_LOGICAL_WORKSPACE_LIMIT,
                        self.capacity,
                        true,
                        TokenOutcome::CapacityExceeded,
                    ));
                }
            }
        }
        if needed < self.held {
            self.pool.release(self.held - needed);
        }
        self.current_bytes = current;
        self.peak_bytes = peak;
        self.peak_reserved = self.peak_reserved.max(peak_reserved);
        self.held = needed;
        Ok(())
    }

    fn poll(&mut self) -> Result<()> {
        match self.sticky.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn claim_peak_hold(&mut self, target: u64) -> Option<(u64, u64, u64)> {
        if self.peak_hold_done || self.held < target {
            return None;
        }
        self.peak_hold_done = true;
        Some((self.peak_reserved, self.held, self.peak_bytes))
    }
}

impl Drop for TokenLedger {
    fn drop(&mut self) {
        if self.held > 0 {
            self.pool.release(self.held);
            self.held = 0;
        }
    }
}

thread_local! {
    static ACTIVE: Cell<*mut TokenLedger> = const { Cell::new(std::ptr::null_mut()) };
    /// True while [`with_ledger`] holds the exclusive ledger borrow. A
    /// `PeakPool` callback that reenters the token API sees this and panics
    /// instead of minting a second `&mut`.
    static BORROWED: Cell<bool> = const { Cell::new(false) };
}

/// Clears [`BORROWED`] when `with_ledger` returns or unwinds, so a panic
/// inside `try_grow` or `release` does not stick the thread.
struct BorrowGuard;

impl Drop for BorrowGuard {
    fn drop(&mut self) {
        BORROWED.with(|borrowed| borrowed.set(false));
    }
}

/// Installs `ledger` on this thread for the lifetime of the guard.
pub struct LedgerGuard<'a> {
    prev: *mut TokenLedger,
    _ledger: PhantomData<&'a mut TokenLedger>,
}

impl<'a> LedgerGuard<'a> {
    pub fn install(ledger: &'a mut TokenLedger) -> Self {
        let prev = ACTIVE.with(|slot| slot.replace(ledger));
        Self {
            prev,
            _ledger: PhantomData,
        }
    }
}

impl Drop for LedgerGuard<'_> {
    fn drop(&mut self) {
        ACTIVE.with(|slot| slot.set(self.prev));
    }
}

fn with_ledger<T>(f: impl FnOnce(&mut TokenLedger) -> T) -> Option<T> {
    ACTIVE.with(|slot| {
        let ptr = slot.get();
        if ptr.is_null() {
            return None;
        }
        if BORROWED.with(|borrowed| borrowed.replace(true)) {
            panic!("token ledger reentered while its exclusive borrow is live");
        }
        let _guard = BorrowGuard;
        // SAFETY: `LedgerGuard<'a>` keeps an exclusive mutable borrow of the
        // installed ledger for the guard's full lifetime, and its drop
        // restores the previous pointer before that borrow can end, so
        // `ptr` is non-dangling. `BORROWED` is set for this call. A
        // reentrant `with_ledger` — including a `PeakPool::try_grow` or
        // `release` that calls back into the token API — panics instead of
        // minting a second `&mut` to the same ledger.
        Some(f(unsafe { &mut *ptr }))
    })
}

fn reaccount(from: u64, to: u64) -> Result<()> {
    match with_ledger(|ledger| ledger.reaccount(from, to)) {
        Some(result) => result,
        None => Ok(()),
    }
}

/// True while this thread has a token ledger installed. Workspace walks are
/// skipped when this is false, so admission-off execution does not pay for
/// a reservation it will not keep.
pub fn workspace_accounting() -> bool {
    ACTIVE.with(|slot| !slot.get().is_null())
}

/// Reservation for one owned workspace structure. Drop releases it.
/// A no-op when token admission is off.
#[derive(Debug)]
pub struct WorkspaceCharge {
    bytes: u64,
}

impl WorkspaceCharge {
    pub fn empty() -> Self {
        Self { bytes: 0 }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Reserve `bytes`. Zero does not touch the ledger, so an empty
    /// structure does not replace the pre-execution estimate.
    pub fn retain(bytes: u64) -> Result<Self> {
        if bytes == 0 {
            return Ok(Self::empty());
        }
        reaccount(0, bytes)?;
        Ok(Self { bytes })
    }

    /// Move this charge to `bytes`. A refusal leaves the previous size.
    pub fn resize(&mut self, bytes: u64) -> Result<()> {
        if bytes == self.bytes {
            return Ok(());
        }
        reaccount(self.bytes, bytes)?;
        self.bytes = bytes;
        Ok(())
    }

    /// Reserve `bytes()` only while a ledger is installed. The closure is
    /// not called when admission is off.
    pub fn retain_with(bytes: impl FnOnce() -> u64) -> Result<Self> {
        if !workspace_accounting() {
            return Ok(Self::empty());
        }
        Self::retain(bytes())
    }

    /// Move this charge to `bytes()` only while a ledger is installed.
    pub fn resize_with(&mut self, bytes: impl FnOnce() -> u64) -> Result<()> {
        if !workspace_accounting() {
            return Ok(());
        }
        self.resize(bytes())
    }

    /// Leave this charge accounted in the active ledger. The ledger then
    /// owns the reservation and releases it when the ledger itself drops.
    pub fn transfer_to_ledger(mut self) {
        self.bytes = 0;
    }
}

impl Drop for WorkspaceCharge {
    fn drop(&mut self) {
        if self.bytes > 0 {
            let _ = reaccount(self.bytes, 0);
            self.bytes = 0;
        }
    }
}

/// Current workspace bytes, high-water bytes, and pool tokens held.
pub fn workspace_meter() -> Option<(u64, u64, u64)> {
    with_ledger(|ledger| (ledger.current_bytes, ledger.peak_bytes, ledger.held))
}

/// Surface a budget already crossed on this thread.
pub fn poll() -> Result<()> {
    match with_ledger(|ledger| ledger.poll()) {
        Some(result) => result,
        None => Ok(()),
    }
}

/// The receipt of the active ledger, when one is installed.
pub fn current_receipt(outcome: TokenOutcome) -> Option<ExecutionReceipt> {
    with_ledger(|ledger| ledger.receipt(outcome))
}

/// Test hook for the processing-memory load test. Unset in production.
/// After a named workspace family reaches CYPHERLITE_PEAK_HOLD_TOKENS,
/// the process logs its live reservation and sleeps so a sampler can read
/// RSS while that reservation and its owning structure are alive.
/// `peak_bytes` is the high-water mark. `held` follows the workspace
/// currently reserved.
pub fn peak_hold(family: &'static str) {
    let ms = peak_hold_ms();
    if ms == 0 {
        return;
    }
    if peak_hold_family().is_some_and(|configured| configured != family) {
        return;
    }
    let target = peak_hold_tokens();
    let Some(snapshot) = with_ledger(|ledger| ledger.claim_peak_hold(target)) else {
        return;
    };
    let Some((peak_logical_bytes, held, peak_bytes)) = snapshot else {
        return;
    };
    eprintln!(
        "peak hold: family={family} peak_logical_bytes={peak_logical_bytes} held={held} peak_bytes={peak_bytes}"
    );
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Upper bound on the load-test peak hold. A leaked environment variable
/// cannot stall a sort for longer than a minute.
const PEAK_HOLD_CAP_MS: u64 = 60_000;

fn peak_hold_ms() -> u64 {
    static HOLD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *HOLD.get_or_init(|| {
        let Ok(raw) = std::env::var("CYPHERLITE_PEAK_HOLD_MS") else {
            return 0;
        };
        let Ok(ms) = raw.trim().parse::<u64>() else {
            return 0;
        };
        ms.min(PEAK_HOLD_CAP_MS)
    })
}

fn peak_hold_tokens() -> u64 {
    static TOKENS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TOKENS.get_or_init(|| {
        std::env::var("CYPHERLITE_PEAK_HOLD_TOKENS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .max(1)
    })
}

fn peak_hold_family() -> Option<&'static str> {
    static FAMILY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FAMILY
        .get_or_init(|| {
            std::env::var("CYPHERLITE_PEAK_HOLD_FAMILY")
                .ok()
                .map(|raw| raw.trim().to_string())
                .filter(|raw| !raw.is_empty())
        })
        .as_deref()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// One reservation quantum, so pool sizes read as quanta.
    const Q: u64 = CHARGE_QUANTUM_BYTES;

    struct LocalPool {
        capacity: u64,
        reserved: std::sync::atomic::AtomicU64,
    }

    impl LocalPool {
        fn reserved(&self) -> u64 {
            self.reserved.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl PeakPool for LocalPool {
        fn try_grow(&self, extra: u64) -> bool {
            let mut current = self.reserved();
            loop {
                let Some(next) = current.checked_add(extra) else {
                    return false;
                };
                if next > self.capacity {
                    return false;
                }
                match self.reserved.compare_exchange_weak(
                    current,
                    next,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                ) {
                    Ok(_) => return true,
                    Err(now) => current = now,
                }
            }
        }

        fn release(&self, amount: u64) {
            if amount > 0 {
                self.reserved
                    .fetch_sub(amount, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    fn open_pool(capacity: u64, reserved: u64) -> (Arc<LocalPool>, TokenLedger) {
        let pool = Arc::new(LocalPool {
            capacity,
            reserved: std::sync::atomic::AtomicU64::new(0),
        });
        assert!(pool.try_grow(reserved));
        let ledger = TokenLedger::new(capacity, reserved, pool.clone());
        (pool, ledger)
    }

    #[test]
    fn charges_are_a_noop_without_a_ledger() {
        let charge = WorkspaceCharge::retain(1_000_000).unwrap();
        assert_eq!(charge.bytes(), 1_000_000);
        drop(charge);
        assert!(poll().is_ok());
        assert!(current_receipt(TokenOutcome::Success).is_none());
        assert!(workspace_meter().is_none());
        peak_hold("sort");
    }

    #[test]
    fn workspace_high_water_survives_a_shrink() {
        let (pool, mut ledger) = open_pool(100 * Q, Q);
        let _guard = LedgerGuard::install(&mut ledger);
        let mut charge = WorkspaceCharge::retain(Q + 1).unwrap();
        assert_eq!(workspace_meter(), Some((Q + 1, Q + 1, 2 * Q)));
        assert_eq!(pool.reserved(), 2 * Q);
        assert_eq!(
            current_receipt(TokenOutcome::Success)
                .unwrap()
                .peak_logical_bytes,
            2 * Q
        );
        charge.resize(1).unwrap();
        assert_eq!(workspace_meter(), Some((1, Q + 1, Q)));
        assert_eq!(pool.reserved(), Q);
        assert_eq!(
            current_receipt(TokenOutcome::Success)
                .unwrap()
                .peak_logical_bytes,
            2 * Q
        );
        drop(charge);
        assert_eq!(workspace_meter(), Some((0, Q + 1, 0)));
        assert_eq!(pool.reserved(), 0);
        assert_eq!(
            current_receipt(TokenOutcome::Success)
                .unwrap()
                .peak_logical_bytes,
            2 * Q
        );
        drop(_guard);
        drop(ledger);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn admission_reservation_holds_until_the_live_workspace_passes_it() {
        let (pool, mut ledger) = open_pool(100 * Q, 5 * Q);
        let _guard = LedgerGuard::install(&mut ledger);
        {
            let charge = WorkspaceCharge::retain(1).unwrap();
            assert_eq!(pool.reserved(), 5 * Q);
            assert_eq!(workspace_meter().unwrap().2, 5 * Q);
            drop(charge);
            assert_eq!(pool.reserved(), 5 * Q);
        }

        let mut charge = WorkspaceCharge::retain(5 * Q + 1).unwrap();
        assert_eq!(pool.reserved(), 6 * Q);
        assert_eq!(
            current_receipt(TokenOutcome::Success)
                .unwrap()
                .peak_logical_bytes,
            6 * Q
        );
        charge.resize(1).unwrap();
        assert_eq!(pool.reserved(), Q);
        assert_eq!(workspace_meter().unwrap().2, Q);
        assert_eq!(
            current_receipt(TokenOutcome::Success)
                .unwrap()
                .peak_logical_bytes,
            6 * Q
        );
        drop(charge);
        assert_eq!(pool.reserved(), 0);
        drop(_guard);
        drop(ledger);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn calibration_hold_claims_once_after_target() {
        let (_pool, mut ledger) = open_pool(100 * Q, Q);
        assert!(ledger.claim_peak_hold(2 * Q).is_none());
        {
            let _guard = LedgerGuard::install(&mut ledger);
            let _charge = WorkspaceCharge::retain(Q + 1).unwrap();
            assert_eq!(
                with_ledger(|active| active.claim_peak_hold(2 * Q)).flatten(),
                Some((2 * Q, 2 * Q, Q + 1))
            );
            assert!(with_ledger(|active| active.claim_peak_hold(2 * Q))
                .flatten()
                .is_none());
        }
        assert!(ledger.claim_peak_hold(2 * Q).is_none());
    }

    #[test]
    fn capacity_failure_does_not_change_the_workspace() {
        let (pool, mut ledger) = open_pool(Q, Q);
        let _guard = LedgerGuard::install(&mut ledger);
        let err = WorkspaceCharge::retain(Q + 1).unwrap_err();
        assert!(err
            .to_string()
            .contains("concurrent-logical-workspace-limit"));
        assert_eq!(workspace_meter(), Some((0, 0, Q)));
        assert_eq!(pool.reserved(), Q);
        assert_eq!(
            current_receipt(TokenOutcome::Success)
                .unwrap()
                .peak_logical_bytes,
            0
        );
    }

    #[test]
    #[should_panic(expected = "token ledger reentered")]
    fn a_pool_callback_cannot_reborrow_the_ledger() {
        struct Reentering;

        impl PeakPool for Reentering {
            fn try_grow(&self, _extra: u64) -> bool {
                let _ = workspace_meter();
                true
            }

            fn release(&self, _amount: u64) {}
        }

        let mut ledger = TokenLedger::new(100, 0, Arc::new(Reentering));
        let _guard = LedgerGuard::install(&mut ledger);
        let _ = WorkspaceCharge::retain(1);
    }

    #[test]
    fn a_panic_inside_the_pool_releases_the_ledger_borrow() {
        struct Boom;

        impl PeakPool for Boom {
            fn try_grow(&self, _extra: u64) -> bool {
                panic!("grow failed");
            }

            fn release(&self, _amount: u64) {}
        }

        let mut ledger = TokenLedger::new(100, 0, Arc::new(Boom));
        let _guard = LedgerGuard::install(&mut ledger);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = WorkspaceCharge::retain(1);
        }));
        assert!(caught.is_err());
        assert_eq!(workspace_meter(), Some((0, 0, 0)));
    }

    #[test]
    fn peak_bytes_round_up_to_one_quantum() {
        assert_eq!(rounded_logical_bytes(0), 0);
        assert_eq!(rounded_logical_bytes(1), Q);
        assert_eq!(rounded_logical_bytes(Q), Q);
        assert_eq!(rounded_logical_bytes(Q + 1), 2 * Q);
        assert_eq!(admission_tail(0), 0);
        assert_eq!(admission_tail(8), 1);
        assert_eq!(admission_tail(89_100), 11_137);
    }
}
