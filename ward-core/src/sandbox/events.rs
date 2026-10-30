// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Lifecycle events of sandboxes, fanned out to any number of readers.
//!
//! The log never waits for a reader: a bounded broadcast channel overwrites
//! its oldest events, and a reader that fell behind is told how many it
//! missed. A bounded history of recent events serves `since` replays.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::protocol::EndReason;
use crate::sandbox::store::SandboxMeta;

/// Events the channel holds for a slow reader, and the history kept for
/// replays.
pub const EVENT_BOUND: usize = 1024;

/// What happened to a sandbox, named as Docker names container events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAction {
    Create,
    Start,
    Die,
    Stop,
    Destroy,
}

impl EventAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Start => "start",
            Self::Die => "die",
            Self::Stop => "stop",
            Self::Destroy => "destroy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxEvent {
    pub action: EventAction,
    pub sandbox_id: String,
    pub name: String,
    pub image: String,
    pub at: SystemTime,
    /// Set on `die`.
    pub exit_code: Option<i32>,
    /// Set on `die` when the cause is known.
    pub reason: Option<EndReason>,
}

impl SandboxEvent {
    pub fn new(action: EventAction, meta: &SandboxMeta) -> Self {
        Self {
            action,
            sandbox_id: meta.id.clone(),
            name: meta.options.spec.name.clone(),
            image: meta.options.image.clone(),
            at: SystemTime::now(),
            exit_code: None,
            reason: None,
        }
    }

    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }
}

struct Shared {
    tx: broadcast::Sender<SandboxEvent>,
    history: Mutex<VecDeque<SandboxEvent>>,
    bound: usize,
}

/// Cheap to clone: every clone shares one channel and one history.
#[derive(Clone)]
pub struct EventLog {
    shared: Arc<Shared>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::with_bound(EVENT_BOUND)
    }
}

impl EventLog {
    pub fn with_bound(bound: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                tx: broadcast::channel(bound).0,
                history: Mutex::new(VecDeque::with_capacity(bound)),
                bound,
            }),
        }
    }

    pub fn emit(&self, mut event: SandboxEvent) {
        // The lock covers the send so a reader subscribing meanwhile sees
        // each event either in the history or live, never both.
        let mut history = self.lock_history();
        // Emitters stamp before they get the lock, so keep history ordered.
        if let Some(last) = history.back() {
            event.at = event.at.max(last.at);
        }
        if history.len() == self.shared.bound {
            history.pop_front();
        }
        history.push_back(event.clone());
        // No receivers is normal, not a failure.
        let _ = self.shared.tx.send(event);
    }

    /// A reader that first yields the history from `since`, if given, then
    /// the events that follow.
    pub fn subscribe(&self, since: Option<SystemTime>) -> EventReader {
        let history = self.lock_history();
        let backlog = since
            .map(|since| history.iter().filter(|e| e.at >= since).cloned().collect())
            .unwrap_or_default();
        EventReader {
            backlog,
            rx: self.shared.tx.subscribe(),
        }
    }

    fn lock_history(&self) -> std::sync::MutexGuard<'_, VecDeque<SandboxEvent>> {
        // A poisoned history only means an emitter panicked mid-push.
        self.shared
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// An event and how many the reader missed just before it.
#[derive(Debug, PartialEq, Eq)]
pub struct Delivered {
    pub event: SandboxEvent,
    pub dropped: u64,
}

pub struct EventReader {
    backlog: VecDeque<SandboxEvent>,
    rx: broadcast::Receiver<SandboxEvent>,
}

impl EventReader {
    /// The next event, or `None` once the log is gone.
    pub async fn next(&mut self) -> Option<Delivered> {
        if let Some(event) = self.backlog.pop_front() {
            return Some(Delivered { event, dropped: 0 });
        }
        let mut dropped = 0;
        loop {
            match self.rx.recv().await {
                Ok(event) => return Some(Delivered { event, dropped }),
                Err(RecvError::Lagged(missed)) => dropped += missed,
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL_BOUND: usize = 4;
    const OVERFLOW: usize = 3;

    fn event(sandbox: usize) -> SandboxEvent {
        SandboxEvent {
            action: EventAction::Start,
            sandbox_id: format!("sb{sandbox}"),
            name: String::new(),
            image: String::new(),
            at: at(sandbox),
            exit_code: None,
            reason: None,
        }
    }

    /// Event `n` happened `n` seconds after the epoch.
    fn at(n: usize) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(n as u64)
    }

    fn ids(delivered: &[Delivered]) -> Vec<&str> {
        delivered
            .iter()
            .map(|d| d.event.sandbox_id.as_str())
            .collect()
    }

    async fn read(reader: &mut EventReader, count: usize) -> Vec<Delivered> {
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(reader.next().await.expect("event"));
        }
        out
    }

    #[test]
    fn given_an_event_stamped_before_the_previous_one_when_emitted_then_history_stays_ordered() {
        // Arrange
        let log = EventLog::with_bound(SMALL_BOUND);
        log.emit(event(2));

        // Act
        log.emit(event(1));

        // Assert
        let times: Vec<SystemTime> = log.lock_history().iter().map(|e| e.at).collect();
        assert_eq!(times, [at(2), at(2)]);
    }

    #[tokio::test]
    async fn given_a_reader_that_lagged_when_it_resumes_then_it_gets_the_newest_events_in_order_and_the_drop_count()
     {
        // Arrange
        let log = EventLog::with_bound(SMALL_BOUND);
        let mut reader = log.subscribe(None);
        (0..SMALL_BOUND + OVERFLOW).for_each(|i| log.emit(event(i)));

        // Act
        let got = read(&mut reader, SMALL_BOUND).await;

        // Assert
        assert_eq!(ids(&got), ["sb3", "sb4", "sb5", "sb6"]);
        assert_eq!(got[0].dropped, OVERFLOW as u64);
        assert_eq!(got[1].dropped, 0);
    }

    #[tokio::test]
    async fn given_history_when_subscribed_since_then_it_replays_from_then_and_follows_live() {
        // Arrange
        let log = EventLog::with_bound(SMALL_BOUND);
        log.emit(event(0));
        log.emit(event(1));
        let mut reader = log.subscribe(Some(at(1)));
        log.emit(event(2));

        // Act
        let got = read(&mut reader, 2).await;

        // Assert
        assert_eq!(ids(&got), ["sb1", "sb2"]);
    }

    #[tokio::test]
    async fn given_more_events_than_the_history_holds_when_subscribed_since_then_only_the_newest_replay()
     {
        // Arrange
        let log = EventLog::with_bound(SMALL_BOUND);
        (0..SMALL_BOUND + OVERFLOW).for_each(|i| log.emit(event(i)));
        let mut reader = log.subscribe(Some(at(0)));

        // Act
        let got = read(&mut reader, SMALL_BOUND).await;

        // Assert
        assert_eq!(ids(&got), ["sb3", "sb4", "sb5", "sb6"]);
    }
}
