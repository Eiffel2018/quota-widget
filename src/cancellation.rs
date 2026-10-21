//! Cooperative cancellation for one scoped unit of background work.
//!
//! Checkpoints return `Interrupted`; they never unwind through database work.
//! Atomic file replacements and SQLite transactions retain their own commit
//! boundaries. A caller must not publish or persist a partial canceled rebuild.
use std::{
    cell::RefCell,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

thread_local! {
    static CURRENT: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Background work canceled",
            ))
        } else {
            Ok(())
        }
    }

    /// Install this token on the current thread, restoring the enclosing scope
    /// on both normal return and panic. Other threads are unaffected.
    pub fn scope<T>(&self, work: impl FnOnce() -> T) -> T {
        let previous = CURRENT.with(|current| current.replace(Some(self.clone())));
        let _reset = ResetScope(previous);
        work()
    }
}

struct ResetScope(Option<CancellationToken>);
impl Drop for ResetScope {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            current.replace(self.0.take());
        });
    }
}

pub fn is_cancelled() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    })
}

/// A no-op outside a cancellation scope, so synchronous callers keep their
/// existing behavior. Expensive loops and pre-commit boundaries call this.
pub fn check() -> io::Result<()> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(Ok(()), CancellationToken::check)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_idempotent() {
        let token = CancellationToken::new();
        let other = token.clone();
        assert!(token.check().is_ok());
        other.cancel();
        other.cancel();
        assert!(token.is_cancelled());
        assert_eq!(
            token.check().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn nested_scopes_restore_the_previous_token_and_isolate_threads() {
        let canceled = CancellationToken::new();
        canceled.cancel();
        let healthy = CancellationToken::new();
        assert!(check().is_ok());
        canceled.scope(|| {
            assert!(is_cancelled());
            healthy.scope(|| assert!(check().is_ok()));
            assert!(is_cancelled());
            std::thread::spawn(|| assert!(check().is_ok()))
                .join()
                .unwrap();
        });
        assert!(check().is_ok());
    }

    #[test]
    fn scope_is_restored_after_unwinding() {
        let token = CancellationToken::new();
        token.cancel();
        let _ = std::panic::catch_unwind(|| token.scope(|| panic!("synthetic panic")));
        assert!(check().is_ok());
    }
}
