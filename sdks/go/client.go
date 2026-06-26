// SPDX-License-Identifier: Apache-2.0

// Package ward is the Go SDK for the ward sandbox daemon.
//
// Wraps the gRPC API in proto/ward.proto with idiomatic Go: context.Context
// for cancellation, struct-of-options for keyword-style args, errors as
// values. The protobuf stubs are generated from the .proto file via
// `buf generate` (or `protoc-gen-go-grpc` directly). See README for
// the codegen step.
//
// Status: first-cut scaffold. The exported types are stable; the gRPC
// stub wiring is deferred until the codegen step is committed.
package ward

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
)

// ─── Types ────────────────────────────────────────────────────────────

// EgressMode controls a sandbox's network policy.
type EgressMode string

const (
	EgressDeny      EgressMode = "deny"
	EgressOpen      EgressMode = "open"
	EgressAllowlist EgressMode = "allowlist"
)

// Sandbox represents one ward sandbox.
type Sandbox struct {
	ID     string
	Image  string
	Status string
}

// ExecResult is returned by Client.Run.
type ExecResult struct {
	PID      string
	Stdout   string
	Stderr   string
	ExitCode *int32 // pointer so "not yet exited" is distinguishable from "exited with 0"
}

// StreamEvent is one event from Client.StreamOutput.
type StreamEvent struct {
	Kind       string // "stdout" | "stderr" | "exit"
	Line       string
	ExitCode   *int32
	DurationMs uint64
}

// CreateSandboxOptions configures a new sandbox.
type CreateSandboxOptions struct {
	Image           string
	Egress          EgressMode
	EgressAllowlist []string
	CPUs            uint32
	MemoryMB        uint32
	TimeoutSeconds  uint64
	Env             map[string]string
	FromSnapshot    string
}

// ─── Defaults ─────────────────────────────────────────────────────────

// DefaultSocketPath returns the path wardd listens on for the current
// user. Mirrors ward-core/src/config.rs::default_socket_path:
//   - macOS: $HOME/.ward/ward.sock
//   - Linux: $XDG_RUNTIME_DIR/ward/ward.sock if set, else $HOME/.ward/ward.sock
func DefaultSocketPath() (string, error) {
	if xdg := os.Getenv("XDG_RUNTIME_DIR"); xdg != "" {
		return filepath.Join(xdg, "ward", "ward.sock"), nil
	}
	home := os.Getenv("HOME")
	if home == "" {
		return "", fmt.Errorf("HOME is not set; cannot resolve default ward socket path")
	}
	return filepath.Join(home, ".ward", "ward.sock"), nil
}

// ─── Client ───────────────────────────────────────────────────────────

// Option configures a Client at construction time.
type Option func(*clientConfig) error

type clientConfig struct {
	socketPath string
	tcpTarget  string
}

// WithDefaultSocket connects to the user's default wardd socket.
func WithDefaultSocket() Option {
	return func(c *clientConfig) error {
		p, err := DefaultSocketPath()
		if err != nil {
			return err
		}
		c.socketPath = p
		return nil
	}
}

// WithSocket connects to an explicit Unix domain socket path.
func WithSocket(path string) Option {
	return func(c *clientConfig) error {
		c.socketPath = path
		return nil
	}
}

// WithTCP connects over TCP. Requires daemon-side mTLS / token auth
// (ADR-013) which is not yet implemented in wardd.
func WithTCP(target string) Option {
	return func(c *clientConfig) error {
		c.tcpTarget = target
		return nil
	}
}

// Client is the gRPC client for the ward daemon.
//
// The grpc.ClientConn and generated stub fields will be added when proto
// codegen lands. Leaving them off until then keeps godoc honest about the
// struct shape and avoids `unused`-style placeholder fields surfacing in
// stack traces or `%+v` formatting.
type Client struct {
	socketPath string
	tcpTarget  string
}

// Connect dials the daemon using the supplied options.
func Connect(_ctx context.Context, opts ...Option) (*Client, error) {
	cfg := &clientConfig{}
	for _, o := range opts {
		if err := o(cfg); err != nil {
			return nil, err
		}
	}
	if cfg.socketPath == "" && cfg.tcpTarget == "" {
		return nil, fmt.Errorf("ward.Connect: must supply WithDefaultSocket, WithSocket, or WithTCP")
	}
	return &Client{socketPath: cfg.socketPath, tcpTarget: cfg.tcpTarget}, nil
}

// Close releases the underlying gRPC connection.
func (c *Client) Close() error {
	// Will call conn.Close() once the channel is wired up.
	return nil
}

// ── Sandbox lifecycle ────────────────────────────────────────────────

// CreateSandbox creates a fresh sandbox from an OCI image.
func (c *Client) CreateSandbox(_ctx context.Context, _opts *CreateSandboxOptions) (*Sandbox, error) {
	return nil, fmt.Errorf("first-cut scaffold; wire to gRPC stub when proto codegen lands")
}

// RemoveSandbox tears a sandbox down. Idempotent: removing an
// already-gone sandbox is a no-op success.
func (c *Client) RemoveSandbox(_ctx context.Context, _sandboxID string) error {
	return fmt.Errorf("first-cut scaffold; wire to gRPC stub when proto codegen lands")
}

// ── Process operations ───────────────────────────────────────────────

// Run executes a command in the sandbox and returns when the process exits.
// For long-running commands, prefer StreamOutput.
func (c *Client) Run(_ctx context.Context, _sandboxID string, _argv []string) (*ExecResult, error) {
	return nil, fmt.Errorf("first-cut scaffold; wire to gRPC stub when proto codegen lands")
}

// StreamOutput streams stdout / stderr / exit events from a running process.
// The returned channel is closed after the final exit event is delivered.
func (c *Client) StreamOutput(_ctx context.Context, _sandboxID, _pid string) (<-chan StreamEvent, error) {
	return nil, fmt.Errorf("first-cut scaffold; wire to gRPC stub when proto codegen lands")
}
