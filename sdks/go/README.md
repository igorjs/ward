# ward-sdk-go

Go client for the [ward](https://github.com/igorjs/ward) sandbox daemon.
Wraps the gRPC API at [`proto/ward.proto`](../../proto/ward.proto) with
an idiomatic `Client` struct.

> **Status: pre-release, first-cut scaffold.** Tracked in
> [issue #41](https://github.com/igorjs/ward/issues/41).

## Install (when published)

```sh
go get github.com/igorjs/ward/sdks/go@latest
```

For local development:

```sh
cd sdks/go
buf generate  # or: protoc-gen-go-grpc directly
go build ./...
go test ./...
```

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
	defer client.RemoveSandbox(ctx, sb.ID)

	result, err := client.Run(ctx, sb.ID, []string{"echo", "hello"})
	if err != nil {
		log.Fatal(err)
	}
	log.Println(result.Stdout)
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
