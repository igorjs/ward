# ADR-001: Project Scope, Purpose, and Layering

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

The container ecosystem is dominated by Docker, a service orchestration platform repurposed for ephemeral job execution. Docker's overhead is acceptable for long-running services but wasteful for short-lived isolated workloads (CI jobs, AI agent sessions, code execution sandboxes). Worse, Docker's namespace-based isolation shares the host kernel and is vulnerable to container escape exploits.

Emerging SaaS alternatives (E2B, Daytona, Vercel Sandbox) solve isolation but require cloud dependencies. Open-source agent orchestrators offer simple DX but rely on plain Docker with no egress controls, no resource enforcement, and no kernel isolation.

There is a gap for a tool that combines hardware-backed microVM isolation with a simple, local-first developer experience, and that is useful beyond AI agent orchestration.

## Decision

**Ward** is a general-purpose sandbox daemon that creates, manages, and destroys isolated execution environments with first-class egress control, resource limits, and mount management. Each sandbox runs in its own microVM with its own Linux kernel via libkrun. Ward knows nothing about AI, prompts, git worktrees, or any specific workflow. It runs things in isolation.

### Layer 1: Ward Daemon (this project)

A compiled Rust binary that runs as a daemon, exposes a Unix socket gRPC API, and manages sandbox lifecycle:

- MicroVM creation from OCI images (via libkrun)
- Command execution with streaming stdout/stderr
- Code string execution with language runtime detection
- Egress filtering (domain-level allowlisting, see ADR-008)
- Resource limits (CPU, memory, PID count, timeout)
- Filesystem mounts (read-only and read-write)
- Snapshots (save/restore sandbox state)
- Shared volumes (daemon-managed, cross-sandbox storage)
- Cross-sandbox publish/subscribe with deny-default group routing (see ADR-011)
- Cleanup on crash, timeout, or explicit teardown

### Layer 2: SDKs (separate packages)

Thin, typed clients in multiple languages that communicate with the daemon over Unix socket gRPC (locally) or TCP gRPC + auth (remote). SDKs are transport layers only. Intelligence lives in the daemon.

### Out of scope for Ward

- AI agent orchestration, CI job scheduling, container image building
- Multi-node distribution and clustering
- Weak isolation fallbacks (no Docker/runc mode)

## Consequences

- Ward is useful to any project needing isolated execution, not just AI tooling.
- A single isolation backend (libkrun) simplifies testing, maintenance, and the mental model.
- The SDK surface is small and stable because the API is simple.
- Multi-node and fleet-scale concerns stay out of the daemon.
