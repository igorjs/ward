// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

mod launch;
mod manager;
pub mod store;
#[cfg(all(test, not(feature = "krunvm")))]
pub(crate) use manager::tests::{create_with_command, host_manager};
pub use manager::{MAIN_PROCESS_PID, ProcessIo, STOP_GRACE_SECONDS, SandboxManager};
pub(crate) use manager::{decode_wait_status, event_chunk, event_wait_status};
