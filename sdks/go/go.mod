module github.com/igorjs/ward/sdks/go

// Pin to the specific patch level (not just `go 1.24`) so OSV scanner
// resolves the directive to the patched stdlib release rather than
// the minimum 1.24.x, fixing GO-2025-3750, surfacing on every PR scan.
// Bump in lockstep with each new advisory: each advisory's "fixed
// version" line is the floor.
go 1.24.4

require (
	google.golang.org/grpc v1.80.0
	google.golang.org/protobuf v1.36.11
)

require (
	golang.org/x/net v0.49.0 // indirect
	golang.org/x/sys v0.40.0 // indirect
	golang.org/x/text v0.33.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260120221211-b8f7ae30c516 // indirect
)
