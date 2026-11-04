# legacy Go server (frozen reference)

The Go implementation (0.8.0) moved here during the Phase 5 cutover; the Rust
server (`ryns-code-mcp-rs/`) is now the primary one.

Layout choice: `go.mod` / `go.sum` stay at the repo ROOT, so from the root
`go vet ./...` and `go test ./...` still cover this package (module
`ryns-code-mcp`, package path `ryns-code-mcp/legacy-go`).

Build note (Go quirk): `go build ./...` collides because the single `main`
package's default output name equals this directory — use
`go build -o .state/ryns-go-legacy ./legacy-go` instead (the `-go` hatch in
`./run` does exactly this).
