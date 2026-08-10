// Package protocol defines the NDJSON wire protocol between the client and
// puppetterm-agent. The client invokes the agent over SSH as a stateless CLI:
//
//	ssh <host> /usr/local/bin/puppetterm-agent
//
// The request is a single JSON object read from stdin; the agent writes
// events to stdout as newline-delimited JSON (one event per line):
//
//	{"type":"output","stream":"stdout","data":"hi\n","request_id":"r-42"}
//	{"type":"result","exit":0,"structured":{...},"request_id":"r-42"}
//	{"type":"error","message":"...","request_id":"r-42"}
package protocol

import (
	"encoding/json"
	"fmt"
	"io"
	"sync"
)

// Request is the single action request read from stdin.
type Request struct {
	Action    string          `json:"action"`
	RequestID string          `json:"request_id,omitempty"`
	TimeoutMS int             `json:"timeout_ms,omitempty"`
	Params    json.RawMessage `json:"params,omitempty"`
}

// Event types written to stdout.
const (
	EventOutput = "output"
	EventResult = "result"
	EventError  = "error"
)

// Stream names for output events.
const (
	StreamStdout = "stdout"
	StreamStderr = "stderr"
)

// Event is a single NDJSON line written to stdout.
type Event struct {
	Type       string          `json:"type"`
	Stream     string          `json:"stream,omitempty"`
	Data       string          `json:"data,omitempty"`
	Exit       *int            `json:"exit,omitempty"`
	Structured json.RawMessage `json:"structured,omitempty"`
	Message    string          `json:"message,omitempty"`
	RequestID  string          `json:"request_id,omitempty"`
}

// Encoder writes NDJSON events to an io.Writer. It is safe for concurrent use.
type Encoder struct {
	mu  sync.Mutex
	enc *json.Encoder
}

// NewEncoder returns an Encoder writing to w.
func NewEncoder(w io.Writer) *Encoder {
	return &Encoder{enc: json.NewEncoder(w)}
}

// Output emits an output event carrying raw bytes from a command stream.
func (e *Encoder) Output(stream, data, requestID string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.enc.Encode(Event{
		Type:      EventOutput,
		Stream:    stream,
		Data:      data,
		RequestID: requestID,
	})
}

// Result emits a result event with an exit code and optional structured payload.
func (e *Encoder) Result(exit int, structured any, requestID string) error {
	var raw json.RawMessage
	if structured != nil {
		b, err := json.Marshal(structured)
		if err != nil {
			return err
		}
		raw = b
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.enc.Encode(Event{
		Type:       EventResult,
		Exit:       &exit,
		Structured: raw,
		RequestID:  requestID,
	})
}

// Errorf emits an error event with a formatted message.
func (e *Encoder) Errorf(requestID, format string, args ...any) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.enc.Encode(Event{
		Type:      EventError,
		Message:   fmt.Sprintf(format, args...),
		RequestID: requestID,
	})
}
