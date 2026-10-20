package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sync"
)

// rotatingLogFile appends to path and, past maxBytes, renames it to path.1
// (shifting older ones up to path.<keep>, the oldest dropped), so the log kept
// under /data stays bounded however long the pool runs.
type rotatingLogFile struct {
	mu       sync.Mutex
	path     string
	maxBytes int64
	keep     int
	file     *os.File
	size     int64
}

func newRotatingLogFile(path string, maxBytes int64, keep int) (*rotatingLogFile, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, err
	}
	logFile := &rotatingLogFile{path: path, maxBytes: maxBytes, keep: keep}
	if err := logFile.open(); err != nil {
		return nil, err
	}
	return logFile, nil
}

func (l *rotatingLogFile) open() error {
	file, err := os.OpenFile(l.path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	info, err := file.Stat()
	if err != nil {
		_ = file.Close()
		return err
	}
	l.file, l.size = file, info.Size()
	return nil
}

func (l *rotatingLogFile) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.size+int64(len(p)) > l.maxBytes && l.size > 0 {
		_ = l.file.Close()
		for index := l.keep; index >= 1; index-- {
			from := l.path
			if index > 1 {
				from = fmt.Sprintf("%s.%d", l.path, index-1)
			}
			_ = os.Rename(from, fmt.Sprintf("%s.%d", l.path, index))
		}
		if err := l.open(); err != nil {
			return 0, err
		}
	}
	written, err := l.file.Write(p)
	l.size += int64(written)
	return written, err
}
