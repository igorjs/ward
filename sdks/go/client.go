// SPDX-License-Identifier: Apache-2.0

// Package ward is the Go SDK for the ward sandbox daemon.
//
// Wraps the gRPC API in proto/ward.proto with idiomatic Go: context.Context
// for cancellation, struct-of-options for keyword-style args, errors as
// values. The generated protobuf stubs live in the wardpb package and are
// committed, so `go get` needs no codegen step.
//
// Status: first cut. Covers sandbox lifecycle, exec, run and output
// streaming; the remaining RPCs are reachable through the wardpb stubs.
package ward

//go:generate ./generate.sh

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/emptypb"

	"github.com/igorjs/ward/sdks/go/wardpb"
)

// tokenHeader carries a sandbox's capability token on token-gated calls.
const tokenHeader = "x-ward-sandbox-token"

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
	Name   string
	Image  string
	Status string
	// Token is the sandbox's capability token. The daemon returns it once,
	// on the CreateSandbox response; keep it to authorize later calls.
	Token string
}

// StatusUnknown is the Sandbox.Status for a wire value this SDK predates.
const StatusUnknown = "unknown"

// sandboxStatusNames maps proto SandboxStatus values to Sandbox.Status.
var sandboxStatusNames = map[int32]string{
	0: "unspecified",
	1: "creating",
	2: "running",
	3: "stopped",
	4: "failed",
	5: "exited",
	6: "stopping",
}

// SandboxFromWire builds a Sandbox from decoded wire fields. A status the
// SDK does not know yields StatusUnknown instead of an error, so a newer
// daemon never breaks an older client.
func SandboxFromWire(id, image string, status int32) Sandbox {
	name, ok := sandboxStatusNames[status]
	if !ok {
		name = StatusUnknown
	}
	return Sandbox{ID: id, Image: image, Status: name}
}

func sandboxFromProto(info *wardpb.SandboxInfo) *Sandbox {
	sb := SandboxFromWire(info.GetId(), info.GetImage(), int32(info.GetStatus()))
	sb.Name = info.GetName()
	sb.Token = info.GetToken()
	return &sb
}

// ExecResult is returned by Client.Run.
type ExecResult struct {
	PID      string
	Stdout   string
	Stderr   string
	ExitCode *int32 // pointer so "not yet exited" is distinguishable from "exited with 0"
}

// Event kinds delivered by Client.StreamOutput.
const (
	KindStdout = "stdout"
	KindStderr = "stderr"
	KindExit   = "exit"
	// KindError is a terminal event: the stream broke and Err says why.
	KindError = "error"
)

// StreamEvent is one event from Client.StreamOutput.
type StreamEvent struct {
	Kind       string // KindStdout | KindStderr | KindExit | KindError
	Line       string
	ExitCode   *int32
	DurationMs uint64
	Err        error // set only when Kind is KindError
}

// CreateSandboxOptions configures a new sandbox.
type CreateSandboxOptions struct {
	Image string
	Name  string
	// Command is the main process argv; empty means none.
	Command []string
	Workdir string
	// Egress defaults to EgressDeny when empty.
	Egress EgressMode
	// EgressAllowlist lists the domains allowed when Egress is EgressAllowlist.
	EgressAllowlist []string
	CPUs            uint32
	MemoryMB        uint32
	TimeoutSeconds  uint64
	Env             map[string]string
	FromSnapshot    string
	// SourceToken authorizes reading FromSnapshot's source sandbox; required
	// whenever FromSnapshot is set.
	SourceToken string
}

// ─── Errors ───────────────────────────────────────────────────────────

// Sentinel errors for the daemon status codes callers usually branch on.
// Test with errors.Is; the wrapped message is the daemon's own text.
var (
	ErrNotFound         = errors.New("ward: not found")
	ErrAlreadyExists    = errors.New("ward: already exists")
	ErrInvalidArgument  = errors.New("ward: invalid argument")
	ErrPermissionDenied = errors.New("ward: permission denied")
)

// mapError converts a gRPC error into the SDK's error shape. Errors that
// are not gRPC statuses, and context cancellation, pass through unchanged.
func mapError(err error) error {
	if err == nil {
		return nil
	}
	st, ok := status.FromError(err)
	if !ok {
		return err
	}
	var sentinel error
	switch st.Code() {
	case codes.NotFound:
		sentinel = ErrNotFound
	case codes.AlreadyExists:
		sentinel = ErrAlreadyExists
	case codes.InvalidArgument:
		sentinel = ErrInvalidArgument
	case codes.PermissionDenied:
		sentinel = ErrPermissionDenied
	case codes.Canceled:
		return context.Canceled
	case codes.DeadlineExceeded:
		return context.DeadlineExceeded
	default:
		return fmt.Errorf("ward: daemon error %s: %s", st.Code(), st.Message())
	}
	return fmt.Errorf("%w: %s", sentinel, st.Message())
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

// WithTCP connects over TCP without TLS. Requires daemon-side mTLS / token
// auth (ADR-013) which is not yet implemented in wardd, so only use it on a
// trusted network.
func WithTCP(target string) Option {
	return func(c *clientConfig) error {
		c.tcpTarget = target
		return nil
	}
}

// Client is the gRPC client for the ward daemon. It is safe for concurrent use.
type Client struct {
	conn *grpc.ClientConn
	rpc  wardpb.WardClient
}

// Connect prepares a connection to the daemon using the supplied options.
// The underlying channel dials lazily, so an unreachable daemon surfaces as
// an error on the first call rather than here.
func Connect(_ context.Context, opts ...Option) (*Client, error) {
	cfg := &clientConfig{}
	for _, o := range opts {
		if err := o(cfg); err != nil {
			return nil, err
		}
	}
	var target string
	switch {
	case cfg.socketPath != "" && cfg.tcpTarget != "":
		return nil, fmt.Errorf("ward.Connect: WithSocket and WithTCP are mutually exclusive")
	case cfg.socketPath != "":
		target = "unix:" + cfg.socketPath
	case cfg.tcpTarget != "":
		target = cfg.tcpTarget
	default:
		return nil, fmt.Errorf("ward.Connect: must supply WithDefaultSocket, WithSocket, or WithTCP")
	}
	conn, err := grpc.NewClient(target, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return nil, fmt.Errorf("ward.Connect: %w", err)
	}
	return &Client{conn: conn, rpc: wardpb.NewWardClient(conn)}, nil
}

// Close releases the underlying gRPC connection.
func (c *Client) Close() error {
	return c.conn.Close()
}

// withToken adds the sandbox capability token to the outgoing call. An
// empty token adds nothing, which suits calls the daemon leaves ungated.
func withToken(ctx context.Context, token string) context.Context {
	if token == "" {
		return ctx
	}
	return metadata.AppendToOutgoingContext(ctx, tokenHeader, token)
}

// ── Sandbox lifecycle ────────────────────────────────────────────────

var egressModes = map[EgressMode]wardpb.EgressMode{
	"":              wardpb.EgressMode_EGRESS_MODE_DENY,
	EgressDeny:      wardpb.EgressMode_EGRESS_MODE_DENY,
	EgressOpen:      wardpb.EgressMode_EGRESS_MODE_OPEN,
	EgressAllowlist: wardpb.EgressMode_EGRESS_MODE_ALLOWLIST,
}

func (o *CreateSandboxOptions) toProto() (*wardpb.CreateSandboxRequest, error) {
	mode, ok := egressModes[o.Egress]
	if !ok {
		return nil, fmt.Errorf("ward: unknown egress mode %q", o.Egress)
	}
	return &wardpb.CreateSandboxRequest{
		Image:   o.Image,
		Name:    o.Name,
		Command: o.Command,
		Workdir: o.Workdir,
		Env:     o.Env,
		Egress:  &wardpb.EgressPolicy{Mode: mode, Domains: o.EgressAllowlist},
		Resources: &wardpb.ResourceLimits{
			Cpus:           o.CPUs,
			MemoryMb:       o.MemoryMB,
			TimeoutSeconds: o.TimeoutSeconds,
		},
		FromSnapshot: o.FromSnapshot,
	}, nil
}

// CreateSandbox creates a fresh sandbox from an OCI image. The returned
// Sandbox carries the capability token needed by the other calls.
func (c *Client) CreateSandbox(ctx context.Context, opts *CreateSandboxOptions) (*Sandbox, error) {
	if opts == nil {
		return nil, fmt.Errorf("ward: CreateSandbox needs options")
	}
	req, err := opts.toProto()
	if err != nil {
		return nil, err
	}
	info, err := c.rpc.CreateSandbox(withToken(ctx, opts.SourceToken), req)
	if err != nil {
		return nil, mapError(err)
	}
	return sandboxFromProto(info), nil
}

// GetSandbox inspects a sandbox by name, id or unique id prefix. The token
// is optional: with it the daemon also fills in fields that can hold secrets.
func (c *Client) GetSandbox(ctx context.Context, sandboxID, token string) (*Sandbox, error) {
	info, err := c.rpc.GetSandbox(withToken(ctx, token), &wardpb.GetSandboxRequest{Id: sandboxID})
	if err != nil {
		return nil, mapError(err)
	}
	return sandboxFromProto(info), nil
}

// ListSandboxes returns every sandbox the daemon knows. It needs no token.
func (c *Client) ListSandboxes(ctx context.Context) ([]Sandbox, error) {
	resp, err := c.rpc.ListSandboxes(ctx, &emptypb.Empty{})
	if err != nil {
		return nil, mapError(err)
	}
	out := make([]Sandbox, 0, len(resp.GetSandboxes()))
	for _, info := range resp.GetSandboxes() {
		out = append(out, *sandboxFromProto(info))
	}
	return out, nil
}

// StartSandbox starts a stopped sandbox.
func (c *Client) StartSandbox(ctx context.Context, sandboxID, token string) (*Sandbox, error) {
	info, err := c.rpc.Start(withToken(ctx, token), &wardpb.StartRequest{Id: sandboxID})
	if err != nil {
		return nil, mapError(err)
	}
	return sandboxFromProto(info), nil
}

// StopSandbox stops a running sandbox, using the daemon's default grace period.
func (c *Client) StopSandbox(ctx context.Context, sandboxID, token string) (*Sandbox, error) {
	info, err := c.rpc.Stop(withToken(ctx, token), &wardpb.StopRequest{Id: sandboxID})
	if err != nil {
		return nil, mapError(err)
	}
	return sandboxFromProto(info), nil
}

// RemoveSandbox tears a sandbox down.
func (c *Client) RemoveSandbox(ctx context.Context, sandboxID, token string) error {
	_, err := c.rpc.RemoveSandbox(withToken(ctx, token), &wardpb.RemoveSandboxRequest{Id: sandboxID})
	return mapError(err)
}

// ── Process operations ───────────────────────────────────────────────

// Exec starts a command in the sandbox without waiting for it and returns
// the process id to pass to StreamOutput.
func (c *Client) Exec(ctx context.Context, sandboxID string, argv []string, token string) (string, error) {
	info, err := c.rpc.Exec(withToken(ctx, token), &wardpb.ExecRequest{SandboxId: sandboxID, Command: argv})
	if err != nil {
		return "", mapError(err)
	}
	return info.GetPid(), nil
}

// Run executes a command in the sandbox and returns when the process exits.
// For long-running commands, prefer StreamOutput.
func (c *Client) Run(ctx context.Context, sandboxID string, argv []string, token string) (*ExecResult, error) {
	pid, err := c.Exec(ctx, sandboxID, argv, token)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	stream, err := c.openStream(ctx, sandboxID, pid, token)
	if err != nil {
		return nil, err
	}
	result := &ExecResult{PID: pid}
	for {
		ev, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return result, nil
		}
		if err != nil {
			return nil, mapError(err)
		}
		switch se, ok := streamEventFromProto(ev); {
		case !ok:
		case se.Kind == KindStdout:
			result.Stdout += se.Line
		case se.Kind == KindStderr:
			result.Stderr += se.Line
		case se.Kind == KindExit:
			result.ExitCode = se.ExitCode
		}
	}
}

// StreamOutput streams stdout / stderr / exit events from a running process.
// The returned channel is closed after the final exit event, when the stream
// fails (a KindError event comes first), or when ctx is cancelled.
func (c *Client) StreamOutput(ctx context.Context, sandboxID, pid, token string) (<-chan StreamEvent, error) {
	ctx, cancel := context.WithCancel(ctx)
	stream, err := c.openStream(ctx, sandboxID, pid, token)
	if err != nil {
		cancel()
		return nil, err
	}
	out := make(chan StreamEvent)
	go func() {
		defer close(out)
		defer cancel()
		send := func(ev StreamEvent) bool {
			select {
			case out <- ev:
				return true
			case <-ctx.Done():
				return false
			}
		}
		for {
			ev, err := stream.Recv()
			if errors.Is(err, io.EOF) {
				return
			}
			if err != nil {
				if ctx.Err() == nil {
					send(StreamEvent{Kind: KindError, Err: mapError(err)})
				}
				return
			}
			if se, ok := streamEventFromProto(ev); ok && !send(se) {
				return
			}
		}
	}()
	return out, nil
}

func (c *Client) openStream(ctx context.Context, sandboxID, pid, token string) (grpc.ServerStreamingClient[wardpb.StreamEvent], error) {
	stream, err := c.rpc.StreamOutput(withToken(ctx, token), &wardpb.StreamOutputRequest{SandboxId: sandboxID, Pid: pid})
	if err != nil {
		return nil, mapError(err)
	}
	return stream, nil
}

// streamEventFromProto maps a wire event; ok is false for an event type this
// SDK does not know, which callers skip.
func streamEventFromProto(ev *wardpb.StreamEvent) (StreamEvent, bool) {
	switch ev.GetType() {
	case wardpb.StreamEventType_STREAM_EVENT_TYPE_STDOUT:
		return StreamEvent{Kind: KindStdout, Line: ev.GetLine()}, true
	case wardpb.StreamEventType_STREAM_EVENT_TYPE_STDERR:
		return StreamEvent{Kind: KindStderr, Line: ev.GetLine()}, true
	case wardpb.StreamEventType_STREAM_EVENT_TYPE_EXIT:
		code := ev.GetExitCode()
		return StreamEvent{Kind: KindExit, ExitCode: &code, DurationMs: ev.GetDurationMs()}, true
	default:
		return StreamEvent{}, false
	}
}
