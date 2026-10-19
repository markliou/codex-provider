package main

import (
	"bufio"
	"crypto/rand"
	"crypto/sha1"
	"encoding/base64"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

// This is the minimal RFC 6455 framing the Responses WebSocket relay needs:
// the pool accepts a Codex client's socket and dials codex-upstream on
// loopback, and every message passes through unchanged. Neither leg negotiates
// an extension; the upstream leg's permessage-deflate is codex-upstream's, as
// in a native Codex. Text and binary messages are reassembled from
// continuation frames, pings are answered on the leg they arrive on, and a
// close is surfaced as io.EOF.

const (
	wsOpContinuation = 0x0
	wsOpText         = 0x1
	wsOpBinary       = 0x2
	wsOpClose        = 0x8
	wsOpPing         = 0x9
	wsOpPong         = 0xA

	wsMaxMessageBytes = 64 << 20
	wsAcceptGUID      = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
)

var errWebSocketClosed = io.EOF

type wsConn struct {
	conn    net.Conn
	reader  *bufio.Reader
	masked  bool // a client masks every frame it sends
	writeMu sync.Mutex
	closed  sync.Once
}

func wsAcceptKey(key string) string {
	sum := sha1.Sum([]byte(key + wsAcceptGUID))
	return base64.StdEncoding.EncodeToString(sum[:])
}

func isWebSocketUpgrade(r *http.Request) bool {
	return strings.EqualFold(r.Header.Get("Upgrade"), "websocket") && headerHasToken(r.Header, "Connection", "upgrade")
}

func headerHasToken(header http.Header, name, token string) bool {
	for _, value := range header.Values(name) {
		for _, part := range strings.Split(value, ",") {
			if strings.EqualFold(strings.TrimSpace(part), token) {
				return true
			}
		}
	}
	return false
}

// acceptWebSocket completes the server handshake on a hijacked connection.
// extraHeaders are added to the 101 response.
func acceptWebSocket(w http.ResponseWriter, r *http.Request, extraHeaders http.Header) (*wsConn, error) {
	key := strings.TrimSpace(r.Header.Get("Sec-WebSocket-Key"))
	if !isWebSocketUpgrade(r) || key == "" || r.Header.Get("Sec-WebSocket-Version") != "13" {
		return nil, errors.New("not a websocket handshake")
	}
	hijacker, ok := w.(http.Hijacker)
	if !ok {
		return nil, errors.New("connection cannot be hijacked")
	}
	conn, buffered, err := hijacker.Hijack()
	if err != nil {
		return nil, err
	}
	// The server's read and write deadlines must not cut a socket that stays
	// open across turns.
	_ = conn.SetDeadline(time.Time{})
	var response strings.Builder
	response.WriteString("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n")
	response.WriteString("Sec-WebSocket-Accept: " + wsAcceptKey(key) + "\r\n")
	for name, values := range extraHeaders {
		for _, value := range values {
			if safe := safeHeaderValue(value); safe != "" {
				response.WriteString(name + ": " + safe + "\r\n")
			}
		}
	}
	response.WriteString("\r\n")
	if _, err := conn.Write([]byte(response.String())); err != nil {
		_ = conn.Close()
		return nil, err
	}
	return &wsConn{conn: conn, reader: buffered.Reader}, nil
}

// dialWebSocket opens a client WebSocket. A handshake that is answered with
// anything but 101 returns that response (body readable) and no connection,
// so the caller can classify the refusal like any upstream answer.
func dialWebSocket(target string, headers http.Header) (*wsConn, *http.Response, error) {
	parsed, err := url.Parse(target)
	if err != nil {
		return nil, nil, err
	}
	if parsed.Scheme != "ws" {
		return nil, nil, fmt.Errorf("unsupported websocket scheme %q", parsed.Scheme)
	}
	conn, err := net.DialTimeout("tcp", parsed.Host, 10*time.Second)
	if err != nil {
		return nil, nil, err
	}
	nonce := make([]byte, 16)
	_, _ = rand.Read(nonce)
	key := base64.StdEncoding.EncodeToString(nonce)
	request := &http.Request{Method: http.MethodGet, URL: parsed, Host: parsed.Host, Header: headers.Clone(), Proto: "HTTP/1.1", ProtoMajor: 1, ProtoMinor: 1}
	request.Header.Set("Upgrade", "websocket")
	request.Header.Set("Connection", "Upgrade")
	request.Header.Set("Sec-WebSocket-Key", key)
	request.Header.Set("Sec-WebSocket-Version", "13")
	if err := request.Write(conn); err != nil {
		_ = conn.Close()
		return nil, nil, err
	}
	reader := bufio.NewReader(conn)
	response, err := http.ReadResponse(reader, request)
	if err != nil {
		_ = conn.Close()
		return nil, nil, err
	}
	if response.StatusCode != http.StatusSwitchingProtocols {
		body, _ := io.ReadAll(io.LimitReader(response.Body, maxRequestBody))
		_ = conn.Close()
		response.Body = io.NopCloser(strings.NewReader(string(body)))
		return nil, response, nil
	}
	if response.Header.Get("Sec-WebSocket-Accept") != wsAcceptKey(key) {
		_ = conn.Close()
		return nil, nil, errors.New("websocket handshake accept key mismatch")
	}
	return &wsConn{conn: conn, reader: reader, masked: true}, response, nil
}

// ReadMessage returns the next text or binary message. Pings are answered and
// pongs dropped; a close frame is acknowledged and reported as io.EOF.
func (c *wsConn) ReadMessage() (int, []byte, error) {
	var message []byte
	opcode := -1
	for {
		fin, frameOp, payload, err := c.readFrame()
		if err != nil {
			return 0, nil, err
		}
		switch frameOp {
		case wsOpPing:
			if err := c.writeFrame(wsOpPong, payload); err != nil {
				return 0, nil, err
			}
			continue
		case wsOpPong:
			continue
		case wsOpClose:
			// Echo the status code back, as RFC 6455 asks, before closing.
			if len(payload) > 2 {
				payload = payload[:2]
			}
			_ = c.writeFrame(wsOpClose, payload)
			c.Close()
			return 0, nil, errWebSocketClosed
		case wsOpText, wsOpBinary:
			if opcode != -1 {
				return 0, nil, errors.New("websocket data frame inside a fragmented message")
			}
			opcode = frameOp
		case wsOpContinuation:
			if opcode == -1 {
				return 0, nil, errors.New("websocket continuation without a message")
			}
		default:
			return 0, nil, fmt.Errorf("websocket opcode %d", frameOp)
		}
		if len(message)+len(payload) > wsMaxMessageBytes {
			return 0, nil, errors.New("websocket message too large")
		}
		message = append(message, payload...)
		if fin {
			return opcode, message, nil
		}
	}
}

func (c *wsConn) readFrame() (bool, int, []byte, error) {
	var header [2]byte
	if _, err := io.ReadFull(c.reader, header[:]); err != nil {
		return false, 0, nil, err
	}
	fin := header[0]&0x80 != 0
	if header[0]&0x70 != 0 {
		return false, 0, nil, errors.New("websocket frame uses an unnegotiated extension")
	}
	opcode := int(header[0] & 0x0F)
	masked := header[1]&0x80 != 0
	length := uint64(header[1] & 0x7F)
	switch length {
	case 126:
		var extended [2]byte
		if _, err := io.ReadFull(c.reader, extended[:]); err != nil {
			return false, 0, nil, err
		}
		length = uint64(binary.BigEndian.Uint16(extended[:]))
	case 127:
		var extended [8]byte
		if _, err := io.ReadFull(c.reader, extended[:]); err != nil {
			return false, 0, nil, err
		}
		length = binary.BigEndian.Uint64(extended[:])
	}
	if length > wsMaxMessageBytes {
		return false, 0, nil, errors.New("websocket frame too large")
	}
	var mask [4]byte
	if masked {
		if _, err := io.ReadFull(c.reader, mask[:]); err != nil {
			return false, 0, nil, err
		}
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(c.reader, payload); err != nil {
		return false, 0, nil, err
	}
	if masked {
		for i := range payload {
			payload[i] ^= mask[i%4]
		}
	}
	return fin, opcode, payload, nil
}

func (c *wsConn) WriteMessage(opcode int, payload []byte) error {
	return c.writeFrame(opcode, payload)
}

func (c *wsConn) writeFrame(opcode int, payload []byte) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	header := make([]byte, 0, 14)
	header = append(header, 0x80|byte(opcode))
	maskBit := byte(0)
	if c.masked {
		maskBit = 0x80
	}
	switch length := len(payload); {
	case length < 126:
		header = append(header, maskBit|byte(length))
	case length <= 0xFFFF:
		header = append(header, maskBit|126, byte(length>>8), byte(length))
	default:
		header = append(header, maskBit|127)
		header = binary.BigEndian.AppendUint64(header, uint64(length))
	}
	frame := payload
	if c.masked {
		var mask [4]byte
		_, _ = rand.Read(mask[:])
		header = append(header, mask[:]...)
		frame = make([]byte, len(payload))
		for i := range payload {
			frame[i] = payload[i] ^ mask[i%4]
		}
	}
	if _, err := c.conn.Write(header); err != nil {
		return err
	}
	_, err := c.conn.Write(frame)
	return err
}

// CloseWith sends a close frame with a status code, then closes the socket.
func (c *wsConn) CloseWith(code int, reason string) {
	payload := binary.BigEndian.AppendUint16(nil, uint16(code))
	payload = append(payload, reason...)
	_ = c.writeFrame(wsOpClose, payload)
	c.Close()
}

func (c *wsConn) Close() {
	c.closed.Do(func() {
		_ = c.conn.Close()
	})
}
