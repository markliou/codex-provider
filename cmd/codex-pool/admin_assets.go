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
