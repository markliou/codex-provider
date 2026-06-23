package main

import (
	_ "embed"
	"net/http"
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
