# ward-sdk-go

Go client for the [ward](https://github.com/igorjs/ward) sandbox daemon.
Wraps the gRPC API at [`proto/ward.proto`](../../proto/ward.proto) with
an idiomatic `Client` struct.

> **Status: pre-release.** Covers sandbox lifecycle (create, get, list,
> start, stop, remove), exec, run and output streaming. The generated gRPC
> stubs in `wardpb` expose every other RPC. Tracked in
> [issue #41](https://github.com/igorjs/ward/issues/41).

## Install

```sh
go get github.com/igorjs/ward/sdks/go@v0.1.0
```

The module lives in a subdirectory of the repo, so release tags carry the
directory prefix: `sdks/go/v0.1.0`. Those tags include the generated gRPC
stubs in `wardpb`, so there is no codegen step to use the SDK. The stubs are
not committed on `main`, so do not use `@main` or a pseudo-version.

## Local development

The stubs in `wardpb` are generated and gitignored. Install the
prerequisites, generate them, then build and test:

```sh
# protoc: brew install protobuf  (or: apt-get install protobuf-compiler)
go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.11
go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.6.2

cd sdks/go
./generate.sh
go vet ./...
go test -race ./...
```

Rerun `./generate.sh` after any change to `proto/ward.proto`.

## Capability tokens

`CreateSandbox` returns the sandbox's token once, in `Sandbox.Token`. Pass it
to every call that acts on the sandbox (`Run`, `Exec`, `StreamOutput`,
`StartSandbox`, `StopSandbox`, `RemoveSandbox`). The client sends it in the
`x-ward-sandbox-token` metadata header. `ListSandboxes` needs no token, and
`GetSandbox` takes an optional one.

## Quick start

```go
package main

import (
	"context"
	"log"

	ward "github.com/igorjs/ward/sdks/go"
)

func main() {
	ctx := context.Background()

	client, err := ward.Connect(ctx, ward.WithDefaultSocket())
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	sb, err := client.CreateSandbox(ctx, &ward.CreateSandboxOptions{
		Image:  "alpine:latest",
		Egress: ward.EgressDeny,
	})
	if err != nil {
		log.Fatal(err)
	}
	defer client.RemoveSandbox(ctx, sb.ID, sb.Token)

	result, err := client.Run(ctx, sb.ID, []string{"echo", "hello"}, sb.Token)
	if err != nil {
		log.Fatal(err)
	}
	log.Println(result.Stdout)
	if result.ExitCode != nil {
		log.Println("exit code:", *result.ExitCode)
	}
}
```

## Why Go

Backend integrations live in Go more often than not: Kubernetes operators,
internal CI runners, custom platform services. A first-class Go SDK lets
those tools embed ward without re-implementing the gRPC client by hand
each time.

## Licence

Apache-2.0. Independent of ward's AGPL-3.0 daemon licence per
[ADR-005](../../docs/adr/005-sdk-strategy.md).
