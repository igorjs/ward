# ADR-006: Licensing

**Status:** Accepted
**Date:** 2026-05-02
**Authors:** Igor

## Context

Ward is infrastructure software with two distinct components: a daemon that provides the isolation capability, and SDKs that let applications consume it. These components have different adoption dynamics and different risk profiles regarding competitive exploitation.

The daemon is where Ward's competitive value lives. A cloud provider or SaaS company could take the daemon, add proprietary features, and offer it as a managed service without contributing back. This has happened repeatedly in the open-source infrastructure space (ElasticSearch/AWS OpenSearch, MongoDB/DocumentDB, Terraform/OpenTofu).

The SDKs are consumption interfaces. Friction in SDK adoption directly reduces Ward's utility. Developers embedding a Ward SDK in their application should not face licensing concerns about their own code.

## Decision

### Ward Daemon: AGPL-3.0-only

The AGPL v3 requires that anyone who modifies the daemon and runs it as a network service must make their modified source code available. This protects against the "SaaS loophole" where a company takes open-source software, runs it as a hosted service, and never releases their changes.

Specifically:

- Anyone can use Ward internally without restriction.
- Anyone can modify Ward for their own use.
- If you distribute a modified Ward binary, you must release your modifications under AGPL v3.
- If you run a modified Ward as a network service (e.g., a hosted sandbox platform), you must release your modifications under AGPL v3.
- Running an unmodified Ward daemon in production (including as part of a commercial product) does not trigger any source disclosure obligation.

### Ward SDKs: Apache-2.0

All SDKs are licensed under Apache 2.0. This is a permissive license that allows unrestricted commercial use with two important protections over MIT:

1. **Explicit patent grant.** Contributors grant users a royalty-free patent license covering any patents that would be infringed by the contribution.
2. **Patent retaliation clause.** If a user sues any contributor for patent infringement related to the SDK, their license to the SDK is automatically terminated.

SDK users can embed Ward SDKs in proprietary, closed-source, commercial software without any obligation to release their own source code.

### Boundary between AGPL and Apache 2.0

The SDKs communicate with the daemon over gRPC (Unix socket locally, TCP remotely). This is an arms-length network interface, not a library link. Applications using the SDK are not derivative works of the AGPL-licensed daemon. The AGPL obligation applies only to the daemon binary itself and any modifications to it.

This is the same boundary model used by:
- MongoDB (SSPL server, Apache 2.0 drivers)
- Grafana (AGPL server, Apache 2.0 client libraries)
- Mastodon (AGPL server, MIT client libraries)

### Protocol specification

The protobuf schema (`proto/ward.proto`) is released under Creative Commons CC0 1.0 (public domain dedication). Anyone can use it to build their own SDK or compatible daemon without any license obligations.

## Consequences

- Cloud providers cannot take Ward, add proprietary features, and sell it as a closed service without releasing their changes.
- Developers can use Ward SDKs in any project (open source or proprietary) without licensing concerns.
- Third parties can build alternative SDKs or compatible tools from the `.proto` schema without touching any AGPL or Apache 2.0 code.
- Enterprise legal teams evaluating Ward will see a clean separation: AGPL for the server component they run on their own infrastructure, Apache 2.0 for the client library they embed in their code.
- The AGPL may deter some companies from contributing to the daemon. This is an accepted tradeoff. Companies that are unwilling to contribute under AGPL are typically the ones most likely to extract value without giving back.
