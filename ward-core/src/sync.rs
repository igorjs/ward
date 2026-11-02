// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Lock helpers shared across the daemon.

use std::sync::{Mutex, MutexGuard};

/// Locking that survives a panic in another holder.
pub trait LockUnpoisoned<T> {
    /// A poisoned lock only means a holder panicked; the guarded data is
    /// still usable, and one panic must not take every later request down.
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> LockUnpoisoned<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_unpoisoned_returns_the_data_after_a_holder_panicked() {
        // Arrange
        let lock = std::sync::Arc::new(Mutex::new(7));
        let held = std::sync::Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("poison the lock");
        })
        .join();

        // Act
        let value = *lock.lock_unpoisoned();

        // Assert
        assert_eq!(value, 7);
    }
}
