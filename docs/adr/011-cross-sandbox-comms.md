# ADR-011: Cross-Sandbox Communication (Pub/Sub Broker)

**Status:** Accepted (implemented in `ward-core/src/comms/`)
**Date:** 2026-05-14
**Authors:** Igor

## Context

Multi-agent and pipeline workflows need to coordinate. An AI agent that builds, an agent that tests, and an agent that deploys are not independent: they share state, signal completion, and pass artefacts. Shared volumes (ADR-010) cover bulk data; messaging covers signals and coordination events.

Building this on top of an external message bus (Redis, NATS, Kafka) would force users to deploy a second piece of infrastructure and would punch holes in the egress isolation model — sandboxes would need outbound network access to reach the bus. Building it inside Ward keeps the trust boundary inside one daemon.

The challenge is preserving tenant isolation. Two sandboxes that should not see each other's traffic must not be able to communicate even via publish/subscribe. Default-allow with manual filtering is unsafe; default-deny with explicit opt-in is the right model.

## Decision

Ward includes an in-process pub/sub broker that routes messages between sandboxes within the **same communication group**, enforces **deny-default** policy, and keeps a bounded per-sandbox audit log.

### Policy model

Every sandbox is created with a `CommunicationPolicy` in `CreateSandboxRequest`:

```protobuf
message CommunicationPolicy {
  CommunicationMode mode = 1;       // DENY (default) | GROUP
  string group = 2;                 // required when mode = GROUP
}
```

The two-mode design (no `OPEN`) is intentional: every published message names a publisher and a topic, and every subscriber receives only messages from publishers in the same group. There is no "broadcast to anyone" mode.

**Policy matrix:**

| Publisher mode | Subscriber mode | Same group? | Delivered? |
|----------------|-----------------|-------------|------------|
| Deny | any | n/a | No (publish rejected as InvalidArgument) |
| any | Deny | n/a | No (subscribe rejected as InvalidArgument) |
| Group "alpha" | Group "alpha" | yes | Yes |
| Group "alpha" | Group "beta" | no | No (silently dropped — not delivered, no error) |

A sandbox in Group mode without a group string (empty / missing) is effectively Deny. The broker's `can_communicate` helper enforces this defensively even if state slips through validation.

### RPC surface

```protobuf
service Ward {
  rpc Publish              (PublishRequest)              returns (google.protobuf.Empty);
  rpc Subscribe            (SubscribeRequest)            returns (stream Message);
  rpc GetCommunicationLog  (GetCommunicationLogRequest)  returns (CommunicationLogResponse);
}
```

**Publish** is unary. **Subscribe** is server-streaming, mirroring the pattern from `StreamOutput` (ADR-004). **GetCommunicationLog** returns recent audit entries.

### Routing semantics

- **Lossy fan-out.** Each subscriber has a bounded mpsc buffer (32 messages). When a subscriber's buffer is full, the publisher drops the message for *that subscriber* (others still receive). This is the standard pub/sub model — backpressuring publishers via mpsc would let one slow subscriber stall the whole bus.
- **No persistence.** Messages exist only in transit. A late subscriber does not receive past messages.
- **Closed-receiver reaping.** When a subscriber's `Receiver` is dropped, the broker reaps the subscription on the next publish to that topic. No polling task.

### Audit log

The broker keeps a bounded ring buffer (256 entries) per sandbox of every publish event that involved the sandbox (as publisher *or* as subscriber). Entries capture:

- `from_sandbox`, `topic`, `timestamp`
- `allowed`: false if the publisher's policy denied the attempt
- `subscriber_count`: how many subscribers received the message

The audit log is queried via `GetCommunicationLog` and is the basis for compliance / forensic analysis.

### Lifecycle integration

`SandboxManager` registers each sandbox with the broker on `create` (`broker.register_sandbox(id, policy)`) and deregisters it on `remove` (`broker.deregister_sandbox(id)`). Deregistration drops the policy, the audit log, and any active subscriptions in one pass.

### Topic format

Topics are dotted segments: `agent.results.build`, `pipeline.stage.completed`. The validator (`validate::topic_name`) enforces:

- Non-empty, ≤ 128 characters
- ASCII alphanumeric + dash + underscore + dot
- No leading dot, trailing dot, or consecutive dots

The dot-segment shape opens the door to hierarchical subscription patterns in future (`agent.*` matching all `agent.<anything>`) without a breaking change.

### Payload format

`PublishRequest.payload` is `bytes`. Maximum 1 MiB per message (`MAX_PUBLISH_PAYLOAD_BYTES` in `validate.rs`). Empty payloads are allowed and used for ping-style signals.

## Consequences

- Cross-sandbox coordination is a first-class feature, not a workaround built on shared volumes or external bus.
- Default-deny means new sandboxes cannot exfiltrate via the bus unless explicitly placed in a group.
- The broker is in-process; there is no external dependency. This is safe because routing is between trust domains the daemon already owns.
- Audit log per sandbox enables operator visibility without exposing payload content.
- Group strings are opaque to the broker; coordinating two sandboxes is as simple as creating both with the same `group: "team-x"`.
- The CLI exposes the feature via `ward create --comms-mode group --comms-group team-x` and `ward publish` / `ward subscribe`. The E2E test suite demonstrates the headline flow: two sandboxes in the same group, one publishes, the other receives.
