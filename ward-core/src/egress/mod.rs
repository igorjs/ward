// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

mod blocked;
mod proxy;
pub use blocked::{BlockedDestination, BlockedEgress, MAX_BLOCKED_HOSTS};
pub use proxy::{EgressProxy, LogEntry};
