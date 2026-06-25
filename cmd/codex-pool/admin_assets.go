package main

import (
	_ "embed"
	"net/http"
	"strings"
)

// Keep the visible admin version tied to git metadata injected at build time.
// The HTML deliberately contains a placeholder instead of a hand-edited release
// string, because stale manual footer bumps made it hard to tell which fix was
// actually deployed. Do not simplify this back to a static value.
const adminVersionPlaceholder = "{{CODEX_POOL_VERSION}}"

var (
	buildVersion = "dev"
	buildCommit  = "unknown"
	buildBuiltAt = "unknown"
)

//go:embed web/admin.html
var adminPageHTML string

//go:embed web/app.css
var adminCSS string

//go:embed web/app.js
var adminJS string

//go:embed web/logo.svg
var adminLogoSVG string

//go:embed web/manifest.webmanifest
var adminManifest string

func adminPage() string {
	return strings.ReplaceAll(adminPageHTML, adminVersionPlaceholder, adminDisplayVersion())
}

func adminDisplayVersion() string {
	version := strings.TrimSpace(buildVersion)
	if version == "" {
		version = "dev"
	}
	commit := strings.TrimSpace(buildCommit)
	if commit != "" && commit != "unknown" && !strings.Contains(version, commit) {
		if len(commit) > 8 {
			commit = commit[:8]
		}
		version += "+" + commit
	}
	return version
}

func handleAdminCSS(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/css; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write([]byte(adminCSS))
}

func handleAdminJS(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/javascript; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write([]byte(adminJS))
}

func handleAdminLogo(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "image/svg+xml")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write([]byte(adminLogoSVG))
}

func handleAdminManifest(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "application/manifest+json; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write([]byte(adminManifest))
}
