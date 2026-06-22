package main

import "fmt"

// version and gitSHA are injected at build time via -ldflags -X main.version /
// main.gitSHA (see flake.nix). The defaults are inert dev placeholders; a build
// product shipping these untouched trips version_test.go. See eng-versioning(7).
var (
	version = "0.0.0-dev"
	gitSHA  = "unknown"
)

// versionString is the "<version> (<sha>)" provenance reported in the RFC 0005
// initialize handshake.
func versionString() string {
	return fmt.Sprintf("%s (%s)", version, gitSHA)
}

// versionLine is what `posh-palette --version` prints.
func versionLine() string {
	return "posh-palette " + versionString()
}
