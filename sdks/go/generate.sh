#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Generate the gRPC stubs in ./wardpb from proto/ward.proto.
# The stubs are not committed on main: generate them locally and in CI.
# Release tags (sdks/go/vX.Y.Z) carry them, so `go get` needs no codegen.
# Needs protoc plus protoc-gen-go and protoc-gen-go-grpc on PATH:
#   go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.12
#   go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.6.2
set -euo pipefail

cd "$(dirname "$0")"

PKG="github.com/igorjs/ward/sdks/go/wardpb"

mkdir -p wardpb

# The proto is append-only and carries no go_package, so the Go package
# is mapped here instead.
protoc \
  --proto_path=../../proto \
  --go_out=wardpb --go_opt=paths=source_relative --go_opt="Mward.proto=${PKG}" \
  --go-grpc_out=wardpb --go-grpc_opt=paths=source_relative --go-grpc_opt="Mward.proto=${PKG}" \
  ward.proto
