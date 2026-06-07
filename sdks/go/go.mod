module github.com/igorjs/ward/sdks/go

// Bumped from 1.22 to 1.23 in response to GO-2025-3750 (OSV scanner
// pinning the minimum-version directive to the highest 1.22.x patch).
// 1.23.10 is the fixed release per the advisory. The SDK is still a
// scaffold (no Go code yet), so bumping has no runtime impact.
go 1.23

require (
	google.golang.org/grpc v1.66.0
	google.golang.org/protobuf v1.34.2
)
