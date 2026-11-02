// SPDX-License-Identifier: Apache-2.0

package ward

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/igorjs/ward/sdks/go/wardpb"
)

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

func TestConnectRequiresATarget(t *testing.T) {
	// Act
	_, err := Connect(context.Background())

	// Assert
	if err == nil {
		t.Fatal("Connect with no options succeeded, want error")
	}
}

func TestConnectRejectsSocketAndTCPTogether(t *testing.T) {
	// Act
	_, err := Connect(context.Background(), WithSocket("/x.sock"), WithTCP("localhost:1"))

	// Assert
	if err == nil {
		t.Fatal("Connect with both transports succeeded, want error")
	}
}

func TestCreateOptionsMapToRequest(t *testing.T) {
	// Arrange
	opts := &CreateSandboxOptions{
		Image: "alpine", Name: "n", Command: []string{"sleep", "1"}, Workdir: "/w",
		Egress: EgressAllowlist, EgressAllowlist: []string{"example.com"},
		CPUs: 2, MemoryMB: 512, TimeoutSeconds: 30,
		Env: map[string]string{"A": "b"}, FromSnapshot: "snap",
	}

	// Act
	req, err := opts.toProto()

	// Assert
	if err != nil {
		t.Fatalf("toProto: %v", err)
	}
	if req.Egress.Mode != wardpb.EgressMode_EGRESS_MODE_ALLOWLIST || req.Egress.Domains[0] != "example.com" {
		t.Fatalf("egress = %+v", req.Egress)
	}
	if req.Resources.Cpus != 2 || req.Resources.MemoryMb != 512 || req.Resources.TimeoutSeconds != 30 {
		t.Fatalf("resources = %+v", req.Resources)
	}
	if req.Image != "alpine" || req.Name != "n" || req.Workdir != "/w" || req.FromSnapshot != "snap" ||
		len(req.Command) != 2 || req.Env["A"] != "b" {
		t.Fatalf("request = %+v", req)
	}
}

func TestCreateOptionsEgressDefaultsToDeny(t *testing.T) {
	// Act
	req, err := (&CreateSandboxOptions{Image: "alpine"}).toProto()

	// Assert
	if err != nil || req.Egress.Mode != wardpb.EgressMode_EGRESS_MODE_DENY {
		t.Fatalf("egress = %v, err = %v", req.GetEgress(), err)
	}
}

func TestCreateOptionsUnknownEgressIsRejected(t *testing.T) {
	// Act
	_, err := (&CreateSandboxOptions{Egress: "weird"}).toProto()

	// Assert
	if err == nil {
		t.Fatal("unknown egress mode accepted")
	}
}

func TestMapErrorMapsStatusCodes(t *testing.T) {
	cases := []struct {
		name string
		code codes.Code
		want error
	}{
		{"not found", codes.NotFound, ErrNotFound},
		{"already exists", codes.AlreadyExists, ErrAlreadyExists},
		{"invalid argument", codes.InvalidArgument, ErrInvalidArgument},
		{"permission denied", codes.PermissionDenied, ErrPermissionDenied},
		{"canceled", codes.Canceled, context.Canceled},
		{"deadline", codes.DeadlineExceeded, context.DeadlineExceeded},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			// Arrange
			in := status.Error(tc.code, "boom")

			// Act
			err := mapError(in)

			// Assert
			if !errors.Is(err, tc.want) {
				t.Fatalf("mapError = %v, want %v", err, tc.want)
			}
		})
	}
}

func TestMapErrorOtherCodeKeepsCodeAndMessage(t *testing.T) {
	// Act
	err := mapError(status.Error(codes.Internal, "kaput"))

	// Assert
	if err == nil || !strings.Contains(err.Error(), "Internal") || !strings.Contains(err.Error(), "kaput") {
		t.Fatalf("err = %v", err)
	}
}

func TestCreateSandboxSendsRequestAndReturnsToken(t *testing.T) {
	// Arrange
	f := &fakeWard{}
	c := startFake(t, f)

	// Act
	sb, err := c.CreateSandbox(context.Background(), &CreateSandboxOptions{Image: "alpine", Egress: EgressOpen, MemoryMB: 64})

	// Assert
	if err != nil {
		t.Fatalf("CreateSandbox: %v", err)
	}
	if sb.ID != "sb_1" || sb.Token != "tok-1" || sb.Status != "running" || sb.Image != "alpine" {
		t.Fatalf("sandbox = %+v", sb)
	}
	if f.createReq.Egress.Mode != wardpb.EgressMode_EGRESS_MODE_OPEN || f.createReq.Resources.MemoryMb != 64 {
		t.Fatalf("server saw %+v", f.createReq)
	}
	if got := f.token("CreateSandbox"); got != "" {
		t.Fatalf("plain create sent token %q", got)
	}
}

func TestCreateSandboxSendsSourceTokenForSnapshot(t *testing.T) {
	// Arrange
	f := &fakeWard{}
	c := startFake(t, f)

	// Act
	_, err := c.CreateSandbox(context.Background(), &CreateSandboxOptions{Image: "a", FromSnapshot: "s", SourceToken: "src"})

	// Assert
	if err != nil || f.token("CreateSandbox") != "src" {
		t.Fatalf("err = %v, token = %q", err, f.token("CreateSandbox"))
	}
}

func TestGetSandboxToleratesUnknownStatusAndMapsNotFound(t *testing.T) {
	// Arrange
	c := startFake(t, &fakeWard{})

	// Act
	sb, err := c.GetSandbox(context.Background(), "sb_9", "")
	_, missingErr := c.GetSandbox(context.Background(), "missing", "")

	// Assert
	if err != nil || sb.Status != StatusUnknown {
		t.Fatalf("sb = %+v, err = %v", sb, err)
	}
	if !errors.Is(missingErr, ErrNotFound) {
		t.Fatalf("err = %v, want ErrNotFound", missingErr)
	}
}

func TestListSandboxes(t *testing.T) {
	// Arrange
	c := startFake(t, &fakeWard{})

	// Act
	list, err := c.ListSandboxes(context.Background())

	// Assert
	if err != nil || len(list) != 2 || list[1].ID != "b" {
		t.Fatalf("list = %+v, err = %v", list, err)
	}
}

func TestLifecycleCallsCarryToken(t *testing.T) {
	// Arrange
	f := &fakeWard{}
	c := startFake(t, f)
	ctx := context.Background()

	// Act
	stopped, stopErr := c.StopSandbox(ctx, "sb_1", "tok-1")
	started, startErr := c.StartSandbox(ctx, "sb_1", "tok-1")
	rmErr := c.RemoveSandbox(ctx, "sb_1", "tok-1")

	// Assert
	if stopErr != nil || startErr != nil || rmErr != nil {
		t.Fatalf("errors: %v %v %v", stopErr, startErr, rmErr)
	}
	if stopped.Status != "stopped" || started.Status != "running" {
		t.Fatalf("stopped = %+v, started = %+v", stopped, started)
	}
	for _, rpc := range []string{"Stop", "Start", "RemoveSandbox"} {
		if got := f.token(rpc); got != "tok-1" {
			t.Fatalf("%s token = %q", rpc, got)
		}
	}
}

func TestRemoveSandboxWithoutTokenIsPermissionDenied(t *testing.T) {
	// Arrange
	c := startFake(t, &fakeWard{})

	// Act
	err := c.RemoveSandbox(context.Background(), "sb_1", "")

	// Assert
	if !errors.Is(err, ErrPermissionDenied) {
		t.Fatalf("err = %v, want ErrPermissionDenied", err)
	}
}

func TestRunCollectsOutputAndExitCode(t *testing.T) {
	// Arrange
	f := &fakeWard{events: []*wardpb.StreamEvent{
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_STDOUT, Line: "hello "},
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_STDERR, Line: "warn"},
		{Type: wardpb.StreamEventType(77), Line: "from the future"},
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_STDOUT, Line: "world"},
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_EXIT, ExitCode: 3},
	}}
	c := startFake(t, f)

	// Act
	res, err := c.Run(context.Background(), "sb_1", []string{"echo"}, "tok-1")

	// Assert
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if res.PID != "p1" || res.Stdout != "hello world" || res.Stderr != "warn" {
		t.Fatalf("result = %+v", res)
	}
	if res.ExitCode == nil || *res.ExitCode != 3 {
		t.Fatalf("exit code = %v, want 3", res.ExitCode)
	}
	if f.token("Exec") != "tok-1" || f.token("StreamOutput") != "tok-1" {
		t.Fatalf("tokens = %v", f.tokens)
	}
}

func TestRunWithExitZeroDiffersFromNoExit(t *testing.T) {
	// Arrange
	f := &fakeWard{events: []*wardpb.StreamEvent{
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_EXIT, ExitCode: 0},
	}}
	c := startFake(t, f)

	// Act
	res, err := c.Run(context.Background(), "sb_1", []string{"true"}, "t")

	// Assert
	if err != nil || res.ExitCode == nil || *res.ExitCode != 0 {
		t.Fatalf("res = %+v, err = %v", res, err)
	}
}

func TestStreamOutputDeliversEventsThenCloses(t *testing.T) {
	// Arrange
	f := &fakeWard{events: []*wardpb.StreamEvent{
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_STDOUT, Line: "x"},
		{Type: wardpb.StreamEventType_STREAM_EVENT_TYPE_EXIT, ExitCode: 0, DurationMs: 12},
	}}
	c := startFake(t, f)

	// Act
	ch, err := c.StreamOutput(context.Background(), "sb_1", "p1", "tok-1")
	var got []StreamEvent
	for ev := range ch {
		got = append(got, ev)
	}

	// Assert
	if err != nil || len(got) != 2 {
		t.Fatalf("err = %v, events = %+v", err, got)
	}
	if got[0].Kind != KindStdout || got[0].Line != "x" {
		t.Fatalf("first = %+v", got[0])
	}
	if got[1].Kind != KindExit || got[1].ExitCode == nil || *got[1].ExitCode != 0 || got[1].DurationMs != 12 {
		t.Fatalf("last = %+v", got[1])
	}
}

func TestStreamOutputClosesChannelOnContextCancel(t *testing.T) {
	// Arrange
	c := startFake(t, &fakeWard{}) // no events: server holds the stream open
	ctx, cancel := context.WithCancel(context.Background())
	ch, err := c.StreamOutput(ctx, "sb_1", "p1", "tok-1")
	if err != nil {
		t.Fatalf("StreamOutput: %v", err)
	}

	// Act
	cancel()

	// Assert
	select {
	case ev, ok := <-ch:
		if ok {
			t.Fatalf("unexpected event after cancel: %+v", ev)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("channel not closed after cancel")
	}
}
