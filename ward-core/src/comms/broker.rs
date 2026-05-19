// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! In-process pub/sub broker. Routes messages between same-group
//! sandboxes, enforces deny-default policy, keeps a bounded audit log.

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use bytes::Bytes;
use tokio::sync::{RwLock, mpsc};

use crate::protocol::{ApiError, CommunicationMode, CommunicationPolicy};

/// Maximum log entries kept per sandbox. Bounded so a long-lived daemon
/// doesn't grow unboundedly; old entries fall off the front as new ones
/// arrive. Picked to give callers a useful audit window without making
/// memory growth a function of message volume.
const MAX_LOG_ENTRIES_PER_SANDBOX: usize = 256;

/// Per-subscription channel buffer. Lossy fan-out: when full, the publisher
/// drops the message for that subscriber instead of blocking. This is the
/// standard pub/sub model — backpressuring publishers via mpsc would let
/// one slow subscriber stall the whole bus.
const SUBSCRIBER_CHANNEL_BUFFER: usize = 32;

/// A message delivered to a subscriber. Internal shape; the gRPC layer
/// converts this to `pb::Message` before sending on the wire.
#[derive(Debug, Clone)]
pub struct DeliveredMessage {
    pub from_sandbox: String,
    pub topic: String,
    pub payload: Bytes,
    pub timestamp: SystemTime,
}

/// One row of the per-sandbox audit log. Captures *every* publish event
/// involving the sandbox — both successful deliveries and policy denials —
/// so operators can audit who tried to talk to whom.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub from_sandbox: String,
    pub topic: String,
    pub allowed: bool,
    pub subscriber_count: u32,
    pub timestamp: SystemTime,
}

/// In-process pub/sub broker.
pub struct Broker {
    inner: RwLock<BrokerState>,
}

#[derive(Default)]
struct BrokerState {
    /// Policies are snapshotted at register time. A sandbox can be a
    /// publisher, a subscriber, or both — but only ever with one policy.
    policies: HashMap<String, CommunicationPolicy>,
    /// topic -> active subscriber list. Closed senders are reaped lazily
    /// on the next publish; the broker doesn't poll.
    subscriptions: HashMap<String, Vec<Subscription>>,
    /// sandbox_id -> ring buffer of recent audit entries.
    log: HashMap<String, VecDeque<LogEntry>>,
}

struct Subscription {
    sandbox_id: String,
    tx: mpsc::Sender<DeliveredMessage>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(BrokerState::default()),
        }
    }

    /// Snapshot a sandbox's communication policy. Called by SandboxManager
    /// on successful `create`. Idempotent: re-registering overwrites the
    /// previous policy entry, which matches the lifecycle semantics (a
    /// sandbox's policy is immutable for its lifetime, but a fresh sandbox
    /// reusing an old id starts clean).
    pub async fn register_sandbox(&self, sandbox_id: String, policy: CommunicationPolicy) {
        let mut state = self.inner.write().await;
        state.policies.insert(sandbox_id, policy);
    }

    /// Drop all per-sandbox state. Called by SandboxManager on `remove`.
    /// Removes policy, audit log, and any active subscriptions in one
    /// pass so the broker can't leak references to a sandbox that no
    /// longer exists.
    pub async fn deregister_sandbox(&self, sandbox_id: &str) {
        let mut state = self.inner.write().await;
        state.policies.remove(sandbox_id);
        state.log.remove(sandbox_id);
        for subs in state.subscriptions.values_mut() {
            subs.retain(|s| s.sandbox_id != sandbox_id);
        }
    }

    /// Publish a message to a topic. Returns the number of subscribers
    /// the message was delivered to. Records an audit-log entry on the
    /// publisher's row and on each delivered subscriber's row.
    ///
    /// Lossy: subscribers with a full buffer drop the message rather than
    /// backpressuring the publisher. Subscribers with a closed channel
    /// are reaped on this call.
    pub async fn publish(
        &self,
        from_sandbox: &str,
        topic: &str,
        payload: Bytes,
    ) -> Result<u32, ApiError> {
        let timestamp = SystemTime::now();
        let mut state = self.inner.write().await;

        let publisher_policy = state
            .policies
            .get(from_sandbox)
            .cloned()
            .ok_or_else(|| ApiError::SandboxNotFound(from_sandbox.to_string()))?;

        // Deny mode: refuse before we touch subscribers. The log entry
        // records the attempt so operators can audit denied traffic.
        if publisher_policy.mode == CommunicationMode::Deny {
            push_log_entry(
                &mut state.log,
                from_sandbox,
                LogEntry {
                    from_sandbox: from_sandbox.to_string(),
                    topic: topic.to_string(),
                    allowed: false,
                    subscriber_count: 0,
                    timestamp,
                },
            );
            return Err(ApiError::InvalidRequest(format!(
                "sandbox {from_sandbox} is in Deny mode and cannot publish"
            )));
        }

        // Reap closed senders for this topic before iterating, so a
        // long-dead subscriber doesn't keep showing up in the candidates.
        if let Some(subs) = state.subscriptions.get_mut(topic) {
            subs.retain(|s| !s.tx.is_closed());
        }

        // Snapshot candidates to release the subscriptions borrow before
        // we touch the policies + log maps. Cheap: just (id, Sender) clones.
        let candidates: Vec<(String, mpsc::Sender<DeliveredMessage>)> = state
            .subscriptions
            .get(topic)
            .map(|subs| {
                subs.iter()
                    .map(|s| (s.sandbox_id.clone(), s.tx.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let mut delivered = 0u32;
        let mut per_subscriber_entries: Vec<(String, LogEntry)> = vec![];
        for (sub_id, tx) in candidates {
            let sub_policy = match state.policies.get(&sub_id).cloned() {
                Some(p) => p,
                // Subscriber's policy is missing — sandbox was deregistered
                // mid-publish. Skip silently; the reap on next publish
                // (or deregister itself) cleans the subscription.
                None => continue,
            };
            if !can_communicate(&publisher_policy, &sub_policy) {
                continue;
            }

            let msg = DeliveredMessage {
                from_sandbox: from_sandbox.to_string(),
                topic: topic.to_string(),
                payload: payload.clone(),
                timestamp,
            };
            // try_send: full = drop, closed = reaped next pass.
            if tx.try_send(msg).is_ok() {
                delivered += 1;
                per_subscriber_entries.push((
                    sub_id,
                    LogEntry {
                        from_sandbox: from_sandbox.to_string(),
                        topic: topic.to_string(),
                        allowed: true,
                        subscriber_count: 1,
                        timestamp,
                    },
                ));
            }
        }

        // Apply per-subscriber log entries.
        for (sub_id, entry) in per_subscriber_entries {
            push_log_entry(&mut state.log, &sub_id, entry);
        }

        // Publisher's own log row: one entry per publish call, regardless
        // of how many subscribers it fanned out to.
        push_log_entry(
            &mut state.log,
            from_sandbox,
            LogEntry {
                from_sandbox: from_sandbox.to_string(),
                topic: topic.to_string(),
                allowed: true,
                subscriber_count: delivered,
                timestamp,
            },
        );

        Ok(delivered)
    }

    /// Register a subscription. Returns the receiver side; the caller
    /// holds it for the lifetime of the stream. Closed receivers are
    /// reaped on next publish — no separate cleanup task.
    pub async fn subscribe(
        &self,
        sandbox_id: &str,
        topic: &str,
    ) -> Result<mpsc::Receiver<DeliveredMessage>, ApiError> {
        let mut state = self.inner.write().await;

        let policy = state
            .policies
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| ApiError::SandboxNotFound(sandbox_id.to_string()))?;

        if policy.mode == CommunicationMode::Deny {
            return Err(ApiError::InvalidRequest(format!(
                "sandbox {sandbox_id} is in Deny mode and cannot subscribe"
            )));
        }

        let (tx, rx) = mpsc::channel(SUBSCRIBER_CHANNEL_BUFFER);
        state
            .subscriptions
            .entry(topic.to_string())
            .or_default()
            .push(Subscription {
                sandbox_id: sandbox_id.to_string(),
                tx,
            });

        Ok(rx)
    }

    /// Return a snapshot of recent audit-log entries for a sandbox.
    /// Empty vec for unknown / never-active sandboxes.
    pub async fn log(&self, sandbox_id: &str) -> Vec<LogEntry> {
        let state = self.inner.read().await;
        state
            .log
            .get(sandbox_id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Decide whether sandbox A's policy permits communication with sandbox B's
/// policy. Symmetric: either side in `Deny` mode blocks all traffic; both
/// sides in `Group` mode require identical, non-empty group strings.
pub(crate) fn can_communicate(a: &CommunicationPolicy, b: &CommunicationPolicy) -> bool {
    match (&a.mode, &b.mode) {
        (CommunicationMode::Deny, _) | (_, CommunicationMode::Deny) => false,
        (CommunicationMode::Group, CommunicationMode::Group) => {
            // Empty/None group strings never match — a sandbox in Group
            // mode without a group is effectively deny.
            match (&a.group, &b.group) {
                (Some(g1), Some(g2)) => !g1.is_empty() && g1 == g2,
                _ => false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helper: ring-buffer push for audit log entries.
// ---------------------------------------------------------------------------

pub(crate) fn push_log_entry(
    log: &mut HashMap<String, VecDeque<LogEntry>>,
    sandbox_id: &str,
    entry: LogEntry,
) {
    let q = log.entry(sandbox_id.to_string()).or_default();
    q.push_back(entry);
    while q.len() > MAX_LOG_ENTRIES_PER_SANDBOX {
        q.pop_front();
    }
}

// ---------------------------------------------------------------------------
// Tests: policy matrix + register/deregister lifecycle
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn deny() -> CommunicationPolicy {
        CommunicationPolicy {
            mode: CommunicationMode::Deny,
            group: None,
        }
    }

    fn group(name: &str) -> CommunicationPolicy {
        CommunicationPolicy {
            mode: CommunicationMode::Group,
            group: Some(name.to_string()),
        }
    }

    fn group_without_name() -> CommunicationPolicy {
        CommunicationPolicy {
            mode: CommunicationMode::Group,
            group: None,
        }
    }

    // ----- can_communicate -----------------------------------------------

    #[test]
    fn given_both_deny_when_can_communicate_then_false() {
        assert!(!can_communicate(&deny(), &deny()));
    }

    #[test]
    fn given_one_deny_one_group_when_can_communicate_then_false() {
        // Deny on either side blocks. Symmetric.
        assert!(!can_communicate(&deny(), &group("alpha")));
        assert!(!can_communicate(&group("alpha"), &deny()));
    }

    #[test]
    fn given_same_group_when_can_communicate_then_true() {
        assert!(can_communicate(&group("alpha"), &group("alpha")));
    }

    #[test]
    fn given_different_groups_when_can_communicate_then_false() {
        assert!(!can_communicate(&group("alpha"), &group("beta")));
    }

    #[test]
    fn given_group_without_name_when_can_communicate_then_false() {
        // A sandbox in Group mode without a group string is effectively
        // Deny — the validator should have caught this at create time,
        // but the broker stays safe even if state slips through.
        assert!(!can_communicate(&group_without_name(), &group("alpha")));
        assert!(!can_communicate(&group("alpha"), &group_without_name()));
        assert!(!can_communicate(
            &group_without_name(),
            &group_without_name()
        ));
    }

    #[test]
    fn given_empty_group_string_when_can_communicate_then_false() {
        // Defensive: an empty string is treated as no-group.
        let empty = CommunicationPolicy {
            mode: CommunicationMode::Group,
            group: Some(String::new()),
        };
        assert!(!can_communicate(&empty, &empty));
    }

    // ----- register / deregister -----------------------------------------

    #[tokio::test]
    async fn given_fresh_broker_when_register_sandbox_then_policy_stored() {
        // Arrange
        let broker = Broker::new();

        // Act
        broker.register_sandbox("sb1".into(), group("alpha")).await;

        // Assert: probe via the read-only inner state. (Public API only
        // exposes outcomes; this test pokes through the lock for clarity.)
        let state = broker.inner.read().await;
        let policy = state.policies.get("sb1").expect("registered");
        assert_eq!(policy.mode, CommunicationMode::Group);
        assert_eq!(policy.group.as_deref(), Some("alpha"));
    }

    #[tokio::test]
    async fn given_registered_sandbox_when_deregister_then_state_cleared() {
        // Arrange: register, then add some log entries directly so we
        // can verify they get reaped.
        let broker = Broker::new();
        broker.register_sandbox("sb1".into(), group("alpha")).await;
        {
            let mut state = broker.inner.write().await;
            push_log_entry(
                &mut state.log,
                "sb1",
                LogEntry {
                    from_sandbox: "sb1".into(),
                    topic: "t1".into(),
                    allowed: true,
                    subscriber_count: 0,
                    timestamp: SystemTime::now(),
                },
            );
        }

        // Act
        broker.deregister_sandbox("sb1").await;

        // Assert
        let state = broker.inner.read().await;
        assert!(!state.policies.contains_key("sb1"));
        assert!(!state.log.contains_key("sb1"));
    }

    #[tokio::test]
    async fn given_re_registered_sandbox_when_policy_changes_then_latest_wins() {
        // Arrange: re-registration overwrites — matches the lifecycle where
        // a fresh sandbox reusing an id starts with its own policy.
        let broker = Broker::new();
        broker.register_sandbox("sb1".into(), group("alpha")).await;

        // Act
        broker.register_sandbox("sb1".into(), group("beta")).await;

        // Assert
        let state = broker.inner.read().await;
        assert_eq!(
            state.policies.get("sb1").unwrap().group.as_deref(),
            Some("beta")
        );
    }

    // ----- publish + subscribe end-to-end --------------------------------

    #[tokio::test]
    async fn given_two_same_group_sandboxes_when_publish_then_subscriber_receives() {
        // Arrange: alice + bob in group "team". Bob subscribes to "events".
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), group("team")).await;
        broker.register_sandbox("bob".into(), group("team")).await;
        let mut rx = broker.subscribe("bob", "events").await.expect("subscribe");

        // Act
        let delivered = broker
            .publish("alice", "events", Bytes::from_static(b"hello"))
            .await
            .expect("publish");

        // Assert
        assert_eq!(delivered, 1);
        let msg = rx.recv().await.expect("message");
        assert_eq!(msg.from_sandbox, "alice");
        assert_eq!(msg.topic, "events");
        assert_eq!(msg.payload, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn given_different_group_sandboxes_when_publish_then_subscriber_does_not_receive() {
        // Arrange: alice in "team-a", bob in "team-b". The Group policy
        // means they CANNOT see each other's messages.
        let broker = Broker::new();
        broker
            .register_sandbox("alice".into(), group("team-a"))
            .await;
        broker.register_sandbox("bob".into(), group("team-b")).await;
        let mut rx = broker.subscribe("bob", "events").await.expect("subscribe");

        // Act
        let delivered = broker
            .publish("alice", "events", Bytes::from_static(b"hello"))
            .await
            .expect("publish");

        // Assert: zero delivered. try_recv returns Empty because nothing
        // was sent. Distinguishes from "stream closed".
        assert_eq!(delivered, 0);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn given_deny_publisher_when_publish_then_invalid_request_and_audit_log_records_attempt()
    {
        // Arrange: alice in Deny, can't publish at all. Audit log records
        // the attempt so operators can see who tried.
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), deny()).await;

        // Act
        let err = broker
            .publish("alice", "events", Bytes::from_static(b"x"))
            .await
            .expect_err("Deny publisher");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
        let log = broker.log("alice").await;
        assert_eq!(log.len(), 1);
        assert!(!log[0].allowed);
        assert_eq!(log[0].subscriber_count, 0);
    }

    #[tokio::test]
    async fn given_deny_subscriber_when_subscribe_then_invalid_request() {
        // Arrange
        let broker = Broker::new();
        broker.register_sandbox("bob".into(), deny()).await;

        // Act
        let err = broker.subscribe("bob", "events").await.expect_err("Deny");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_unregistered_publisher_when_publish_then_sandbox_not_found() {
        // Arrange: ghost has never been register_sandbox'd.
        let broker = Broker::new();

        // Act
        let err = broker
            .publish("ghost", "events", Bytes::new())
            .await
            .expect_err("unregistered");

        // Assert: distinguishable from InvalidRequest so the gRPC layer
        // can translate to NotFound, not InvalidArgument.
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_publisher_when_publish_to_no_subscribers_then_returns_zero() {
        // Arrange: alice publishes but nobody is listening.
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), group("team")).await;

        // Act
        let delivered = broker
            .publish("alice", "events", Bytes::from_static(b"x"))
            .await
            .expect("publish");

        // Assert: zero delivered is success, not an error. Empty fan-out
        // is the common case for ephemeral broadcasts.
        assert_eq!(delivered, 0);
    }

    #[tokio::test]
    async fn given_three_subscribers_when_publish_then_all_receive() {
        // Arrange: fan-out to multiple subscribers on the same topic.
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), group("team")).await;
        for sub in ["bob", "carol", "dave"] {
            broker.register_sandbox(sub.into(), group("team")).await;
        }
        let mut bob_rx = broker.subscribe("bob", "events").await.unwrap();
        let mut carol_rx = broker.subscribe("carol", "events").await.unwrap();
        let mut dave_rx = broker.subscribe("dave", "events").await.unwrap();

        // Act
        let delivered = broker
            .publish("alice", "events", Bytes::from_static(b"hi"))
            .await
            .expect("publish");

        // Assert
        assert_eq!(delivered, 3);
        assert!(bob_rx.recv().await.is_some());
        assert!(carol_rx.recv().await.is_some());
        assert!(dave_rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn given_subscriber_drops_receiver_when_publish_then_closed_sub_reaped() {
        // Arrange: subscribe and immediately drop the receiver. The next
        // publish should reap the dead subscription.
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), group("team")).await;
        broker.register_sandbox("bob".into(), group("team")).await;
        {
            let _rx = broker.subscribe("bob", "events").await.unwrap();
            // _rx drops at end of scope → channel closes.
        }

        // Act
        let delivered = broker
            .publish("alice", "events", Bytes::from_static(b"x"))
            .await
            .unwrap();

        // Assert: not delivered, AND the subscription is gone from state.
        assert_eq!(delivered, 0);
        let state = broker.inner.read().await;
        let sub_count = state
            .subscriptions
            .get("events")
            .map(|v| v.len())
            .unwrap_or(0);
        assert_eq!(sub_count, 0, "closed sub should be reaped");
    }

    #[tokio::test]
    async fn given_publish_when_completes_then_publisher_log_records_delivery_count() {
        // Arrange
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), group("team")).await;
        broker.register_sandbox("bob".into(), group("team")).await;
        let _bob_rx = broker.subscribe("bob", "events").await.unwrap();

        // Act
        broker
            .publish("alice", "events", Bytes::from_static(b"x"))
            .await
            .unwrap();

        // Assert: alice's log shows what she sent.
        let log = broker.log("alice").await;
        assert_eq!(log.len(), 1);
        assert!(log[0].allowed);
        assert_eq!(log[0].subscriber_count, 1);
        assert_eq!(log[0].topic, "events");
    }

    #[tokio::test]
    async fn given_subscribers_when_deregister_publisher_then_no_leftover_state() {
        // Arrange: regression guard — deregistering a sandbox cleans up
        // its policy, log, AND any topic subscriptions it owns.
        let broker = Broker::new();
        broker.register_sandbox("alice".into(), group("team")).await;
        let _rx = broker.subscribe("alice", "events").await.unwrap();

        // Act
        broker.deregister_sandbox("alice").await;

        // Assert
        let state = broker.inner.read().await;
        assert!(!state.policies.contains_key("alice"));
        assert!(!state.log.contains_key("alice"));
        let alice_subs = state
            .subscriptions
            .get("events")
            .map(|v| v.iter().filter(|s| s.sandbox_id == "alice").count())
            .unwrap_or(0);
        assert_eq!(alice_subs, 0, "alice's subscription should be gone");
    }

    #[tokio::test]
    async fn given_log_ring_buffer_at_cap_when_push_then_oldest_drops() {
        // Arrange: fill the ring past the cap.
        let mut log: HashMap<String, VecDeque<LogEntry>> = HashMap::new();
        for i in 0..(MAX_LOG_ENTRIES_PER_SANDBOX + 5) {
            push_log_entry(
                &mut log,
                "sb1",
                LogEntry {
                    from_sandbox: "sb1".into(),
                    topic: format!("t{i}"),
                    allowed: true,
                    subscriber_count: 0,
                    timestamp: SystemTime::now(),
                },
            );
        }

        // Act + Assert: queue length never exceeds the cap, and the
        // oldest 5 entries have been evicted (front of the ring now
        // starts at index 5).
        let q = log.get("sb1").unwrap();
        assert_eq!(q.len(), MAX_LOG_ENTRIES_PER_SANDBOX);
        assert_eq!(q.front().unwrap().topic, "t5");
    }
}
