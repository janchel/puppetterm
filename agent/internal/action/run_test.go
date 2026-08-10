package action

import (
	"bytes"
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/puppetterm/agent/internal/protocol"
)

func reqWithParams(t *testing.T, name string, params any) protocol.Request {
	t.Helper()
	b, err := json.Marshal(params)
	if err != nil {
		t.Fatal(err)
	}
	return protocol.Request{Action: name, RequestID: "t", Params: b}
}

func findResult(t *testing.T, events []protocol.Event, wantExit int) bool {
	t.Helper()
	for _, ev := range events {
		if ev.Type == protocol.EventResult && ev.Exit != nil && *ev.Exit == wantExit {
			return true
		}
	}
	return false
}

func TestRunEcho(t *testing.T) {
	events := captureEvents(t, Run, reqWithParams(t, "run", map[string]any{"cmd": "echo hi"}))

	var out strings.Builder
	for _, ev := range events {
		if ev.Type == protocol.EventOutput {
			out.WriteString(ev.Data)
		}
	}
	if out.String() != "hi\n" {
		t.Fatalf("output = %q, want %q", out.String(), "hi\n")
	}
	if !findResult(t, events, 0) {
		t.Fatalf("missing exit:0 result: %+v", events)
	}
}

func TestRunExitCode(t *testing.T) {
	events := captureEvents(t, Run, reqWithParams(t, "run", map[string]any{"cmd": "exit 3"}))
	if !findResult(t, events, 3) {
		t.Fatalf("missing exit:3 result: %+v", events)
	}
}

func TestRunStderr(t *testing.T) {
	events := captureEvents(t, Run, reqWithParams(t, "run", map[string]any{"cmd": "echo err 1>&2"}))
	for _, ev := range events {
		if ev.Type == protocol.EventOutput && ev.Stream == protocol.StreamStderr && strings.Contains(ev.Data, "err") {
			return
		}
	}
	t.Fatalf("missing stderr output: %+v", events)
}

func TestRunMissingCmd(t *testing.T) {
	var buf bytes.Buffer
	enc := protocol.NewEncoder(&buf)
	code := Run(context.Background(), reqWithParams(t, "run", map[string]any{}), enc)
	if code == 0 {
		t.Fatal("want non-zero exit for missing cmd")
	}
	for _, ev := range parseEvents(t, &buf) {
		if ev.Type == protocol.EventError {
			return
		}
	}
	t.Fatal("missing error event")
}

func TestRunTimeout(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()

	var buf bytes.Buffer
	enc := protocol.NewEncoder(&buf)
	start := time.Now()
	code := Run(ctx, reqWithParams(t, "run", map[string]any{"cmd": "sleep 30"}), enc)
	if time.Since(start) > 5*time.Second {
		t.Fatal("timeout took too long")
	}
	if code != exitTimeout {
		t.Fatalf("exit = %d, want %d", code, exitTimeout)
	}
	for _, ev := range parseEvents(t, &buf) {
		if ev.Type == protocol.EventError {
			return
		}
	}
	t.Fatal("missing timeout error event")
}
