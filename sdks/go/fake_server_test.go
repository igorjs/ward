// SPDX-License-Identifier: Apache-2.0

package ward

import (
	"context"
	"net"
	"os"
	"path/filepath"
	"sync"
	"testing"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/emptypb"

	"github.com/igorjs/ward/sdks/go/wardpb"
)

// fakeWard is a minimal in-process daemon that records what it receives.
type fakeWard struct {
	wardpb.UnimplementedWardServer

	mu        sync.Mutex
	tokens    map[string]string // rpc name -> token header seen
	createReq *wardpb.CreateSandboxRequest
	events    []*wardpb.StreamEvent
}

func (f *fakeWard) record(ctx context.Context, rpc string) {
	md, _ := metadata.FromIncomingContext(ctx)
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.tokens == nil {
		f.tokens = map[string]string{}
	}
	f.tokens[rpc] = ""
	if v := md.Get(tokenHeader); len(v) > 0 {
		f.tokens[rpc] = v[0]
	}
}

func (f *fakeWard) token(rpc string) string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.tokens[rpc]
}

func (f *fakeWard) CreateSandbox(ctx context.Context, req *wardpb.CreateSandboxRequest) (*wardpb.SandboxInfo, error) {
	f.record(ctx, "CreateSandbox")
	f.mu.Lock()
	f.createReq = req
	f.mu.Unlock()
	return &wardpb.SandboxInfo{
		Id: "sb_1", Image: req.GetImage(), Name: req.GetName(),
		Status: wardpb.SandboxStatus_SANDBOX_STATUS_RUNNING, Token: "tok-1",
	}, nil
}

func (f *fakeWard) GetSandbox(_ context.Context, req *wardpb.GetSandboxRequest) (*wardpb.SandboxInfo, error) {
	if req.GetId() == "missing" {
		return nil, status.Error(codes.NotFound, "no such sandbox")
	}
	// A status value newer than this SDK knows.
	return &wardpb.SandboxInfo{Id: req.GetId(), Status: wardpb.SandboxStatus(9999)}, nil
}

func (f *fakeWard) ListSandboxes(context.Context, *emptypb.Empty) (*wardpb.ListSandboxesResponse, error) {
	return &wardpb.ListSandboxesResponse{Sandboxes: []*wardpb.SandboxInfo{{Id: "a"}, {Id: "b"}}}, nil
}

func (f *fakeWard) Stop(ctx context.Context, req *wardpb.StopRequest) (*wardpb.SandboxInfo, error) {
	f.record(ctx, "Stop")
	return &wardpb.SandboxInfo{Id: req.GetId(), Status: wardpb.SandboxStatus_SANDBOX_STATUS_STOPPED}, nil
}

func (f *fakeWard) Start(ctx context.Context, req *wardpb.StartRequest) (*wardpb.SandboxInfo, error) {
	f.record(ctx, "Start")
	return &wardpb.SandboxInfo{Id: req.GetId(), Status: wardpb.SandboxStatus_SANDBOX_STATUS_RUNNING}, nil
}

func (f *fakeWard) RemoveSandbox(ctx context.Context, _ *wardpb.RemoveSandboxRequest) (*emptypb.Empty, error) {
	f.record(ctx, "RemoveSandbox")
	if f.token("RemoveSandbox") != "tok-1" {
		return nil, status.Error(codes.PermissionDenied, "bad token")
	}
	return &emptypb.Empty{}, nil
}

func (f *fakeWard) Exec(ctx context.Context, _ *wardpb.ExecRequest) (*wardpb.ProcessInfo, error) {
	f.record(ctx, "Exec")
	return &wardpb.ProcessInfo{Pid: "p1"}, nil
}

func (f *fakeWard) StreamOutput(_ *wardpb.StreamOutputRequest, stream grpc.ServerStreamingServer[wardpb.StreamEvent]) error {
	f.record(stream.Context(), "StreamOutput")
	for _, ev := range f.events {
		if err := stream.Send(ev); err != nil {
			return err
		}
	}
	if f.events == nil {
		<-stream.Context().Done() // hold the stream open until the client leaves
	}
	return nil
}

// startFake serves f on a unix socket and returns a connected Client.
func startFake(t *testing.T, f *fakeWard) *Client {
	t.Helper()
	// t.TempDir embeds the test name, which overflows the unix socket path
	// limit on macOS, so use a short directory instead.
	dir, err := os.MkdirTemp("", "w")
	if err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	sock := filepath.Join(dir, "s")
	lis, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	srv := grpc.NewServer()
	wardpb.RegisterWardServer(srv, f)
	go func() { _ = srv.Serve(lis) }()
	t.Cleanup(srv.Stop)

	c, err := Connect(context.Background(), WithSocket(sock))
	if err != nil {
		t.Fatalf("Connect: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}
