/**
 * Thin Promise-based wrapper over the ward gRPC API.
 *
 * The protobuf stubs (./proto) are generated from proto/ward.proto via
 * `npm run proto:generate`. Until that runs the method bodies throw
 * NotImplementedError — treat this file as the SHAPE of the SDK rather
 * than a turn-key import path.
 */

import * as os from "node:os";
import * as path from "node:path";

// ─── Types ────────────────────────────────────────────────────────────

export interface ConnectOptions {
  /** Path to wardd's Unix domain socket. Mutually exclusive with `tcpTarget`. */
  socketPath?: string;
  /** TCP target `host:port`. Requires daemon-side mTLS + auth (ADR-013). */
  tcpTarget?: string;
}

export interface CreateSandboxOptions {
  image: string;
  egress?: "deny" | "open" | "allowlist";
  egressAllowlist?: string[];
  cpus?: number;
  memoryMb?: number;
  timeoutSeconds?: number;
  env?: Record<string, string>;
}

export interface Sandbox {
  id: string;
  image: string;
  status: string;
}

export interface ExecResult {
  pid: string;
  stdout: string;
  stderr: string;
  exitCode: number | null;
}

export interface StreamEvent {
  kind: "stdout" | "stderr" | "exit";
  line?: string;
  exitCode?: number | null;
  durationMs?: number;
}

// ─── Defaults ─────────────────────────────────────────────────────────

/**
 * Resolve the path wardd is listening on for the current user. Mirrors
 * `ward-core/src/config.rs::default_socket_path`:
 *   - macOS: `$HOME/.ward/ward.sock`
 *   - Linux: `$XDG_RUNTIME_DIR/ward/ward.sock` if set, else `$HOME/.ward/ward.sock`
 */
export function defaultSocketPath(): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) {
    return path.join(xdg, "ward", "ward.sock");
  }
  const home = process.env.HOME ?? os.homedir();
  if (!home) {
    throw new Error("HOME is not set; cannot resolve default ward socket path");
  }
  return path.join(home, ".ward", "ward.sock");
}

// ─── Client ───────────────────────────────────────────────────────────

export class WardClient {
  readonly socketPath?: string;
  readonly tcpTarget?: string;
  // Lazily-initialised gRPC channel + stub. Wired up in connect() once
  // the proto codegen has produced the stub.
  private _channel?: unknown;
  private _stub?: unknown;

  private constructor(opts: ConnectOptions) {
    this.socketPath = opts.socketPath;
    this.tcpTarget = opts.tcpTarget;
  }

  /** Connect over a Unix domain socket (default for local wardd). */
  static connect(socketPath?: string): WardClient {
    return new WardClient({ socketPath: socketPath ?? defaultSocketPath() });
  }

  /** Connect over TCP. Requires daemon-side mTLS / token auth (ADR-013). */
  static connectTcp(target: string): WardClient {
    return new WardClient({ tcpTarget: target });
  }

  // ── Sandbox lifecycle ────────────────────────────────────────────

  async createSandbox(_opts: CreateSandboxOptions): Promise<Sandbox> {
    throw new Error("first-cut scaffold; wire to gRPC stub when proto codegen lands");
  }

  async removeSandbox(_sandboxId: string): Promise<void> {
    throw new Error("first-cut scaffold; wire to gRPC stub when proto codegen lands");
  }

  /**
   * Convenience: create a sandbox, run a callback with it, remove it on
   * exit (even on rejection). Equivalent to a Python context manager.
   */
  async withSandbox<T>(
    opts: CreateSandboxOptions,
    body: (sb: Sandbox) => Promise<T>,
  ): Promise<T> {
    const sb = await this.createSandbox(opts);
    try {
      return await body(sb);
    } finally {
      await this.removeSandbox(sb.id);
    }
  }

  // ── Process operations ───────────────────────────────────────────

  async run(_sandboxId: string, _argv: string[]): Promise<ExecResult> {
    throw new Error("first-cut scaffold; wire to gRPC stub when proto codegen lands");
  }

  async *streamOutput(_sandboxId: string, _pid: string): AsyncIterable<StreamEvent> {
    throw new Error("first-cut scaffold; wire to gRPC stub when proto codegen lands");
    // Once wired, the body will yield StreamEvents read from the
    // server-streaming RPC until the exit event arrives, then return.
  }

  /** Close the underlying gRPC channel. */
  close(): void {
    // Will call channel.close() once the channel is wired up.
  }
}
