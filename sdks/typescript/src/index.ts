// SPDX-License-Identifier: Apache-2.0

/**
 * @igorjs/ward-sdk - public surface.
 *
 * Re-exports the user-facing types so callers can write:
 *
 *     import { WardClient, Sandbox, ExecResult } from '@igorjs/ward-sdk';
 *
 * The protobuf-generated types live under ./proto and are re-exported
 * lazily once the codegen step has run.
 */

export {
  WardClient,
  defaultSocketPath,
  type ConnectOptions,
  type CreateSandboxOptions,
  type Sandbox,
  type ExecResult,
  type StreamEvent,
} from "./client.js";
