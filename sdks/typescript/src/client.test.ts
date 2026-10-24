// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import { test } from "node:test";

import { STATUS_UNKNOWN, sandboxFromWire } from "./client.js";

const EXITED_CODE = 5;
const UNKNOWN_CODE = 9999;

test("sandboxFromWire maps a known status", () => {
  // Act
  const sb = sandboxFromWire("sb_01", "alpine:latest", EXITED_CODE);

  // Assert
  assert.equal(sb.status, "exited");
});

test("sandboxFromWire tolerates an unknown status", () => {
  // Act
  const sb = sandboxFromWire("sb_01", "alpine:latest", UNKNOWN_CODE);

  // Assert
  assert.deepEqual(sb, { id: "sb_01", image: "alpine:latest", status: STATUS_UNKNOWN });
});
