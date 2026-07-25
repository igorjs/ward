module github.com/igorjs/ward/sdks/go

// Pin to the specific patch level (not just `go 1.24`) so OSV scanner
// resolves the directive to the patched stdlib release rather than
// the minimum 1.24.x — fixes GO-2025-3750 surfacing on every PR scan.
// Bump in lockstep with each new advisory: each advisory's "fixed
// version" line is the floor.
go 1.25.0
