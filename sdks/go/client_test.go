// SPDX-License-Identifier: Apache-2.0

package ward

import "testing"

const unknownStatusCode int32 = 9999

func TestSandboxFromWireKnownStatus(t *testing.T) {
	// Arrange
	const exitedCode int32 = 5

	// Act
	sb := SandboxFromWire("sb_01", "alpine:latest", exitedCode)

	// Assert
	if sb.Status != "exited" {
		t.Fatalf("status = %q, want %q", sb.Status, "exited")
	}
}

func TestSandboxFromWireUnknownStatusDoesNotFail(t *testing.T) {
	// Arrange
	id, image := "sb_01", "alpine:latest"

	// Act
	sb := SandboxFromWire(id, image, unknownStatusCode)

	// Assert
	if sb.Status != StatusUnknown {
		t.Fatalf("status = %q, want %q", sb.Status, StatusUnknown)
	}
	if sb.ID != id || sb.Image != image {
		t.Fatalf("other fields lost: %+v", sb)
	}
}
