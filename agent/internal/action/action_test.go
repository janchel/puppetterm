package action

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"testing"

	"github.com/puppetterm/agent/internal/protocol"
)

// parseEvents decodes every NDJSON event written to buf.
func parseEvents(t *testing.T, buf *bytes.Buffer) []protocol.Event {
	t.Helper()
	sc := bufio.NewScanner(buf)
	var events []protocol.Event
	for sc.Scan() {
		if len(bytes.TrimSpace(sc.Bytes())) == 0 {
			continue
		}
		var ev protocol.Event
		if err := json.Unmarshal(sc.Bytes(), &ev); err != nil {
			t.Fatal(err)
		}
		events = append(events, ev)
	}
	return events
}

// captureEvents runs a handler with a background context and returns its events.
func captureEvents(t *testing.T, h Handler, req protocol.Request) []protocol.Event {
	t.Helper()
	var buf bytes.Buffer
	enc := protocol.NewEncoder(&buf)
	if code := h(context.Background(), req, enc); code != 0 {
		t.Fatalf("handler exited %d", code)
	}
	return parseEvents(t, &buf)
}

func TestUnknownAction(t *testing.T) {
	reg := NewRegistry()
	var buf bytes.Buffer
	enc := protocol.NewEncoder(&buf)

	code := reg.Handle(context.Background(), protocol.Request{Action: "nope"}, enc)
	if code != 1 {
		t.Fatalf("exit = %d, want 1", code)
	}
	for _, ev := range parseEvents(t, &buf) {
		if ev.Type == protocol.EventError {
			return
		}
	}
	t.Fatal("missing error event")
}
