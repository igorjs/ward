// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Cross-sandbox communication subsystem.
//!
//! The broker is an in-process pub/sub fabric: sandboxes publish messages
//! to dotted-segment topics, and other sandboxes subscribe to receive
//! them. Policy is deny-default — only sandboxes that opt into the same
//! `Group` can exchange messages, and a sandbox in `Deny` mode can
//! neither publish nor subscribe.
//!
//! The broker lives alongside `SandboxManager` rather than inside it:
//! the manager owns sandbox lifecycle and calls `register_sandbox` /
//! `deregister_sandbox` at the right moments; the gRPC layer routes
//! `Publish`, `Subscribe`, and `GetCommunicationLog` straight to the
//! broker. That keeps routing concerns separate from CRUD bookkeeping.

pub mod broker;

pub use broker::{Broker, DeliveredMessage, LogEntry};
