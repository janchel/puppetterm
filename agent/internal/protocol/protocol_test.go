package protocol

import (
	"bufio"
	"bytes"
	"encoding/json"
	"testing"
)

func TestEncoderOutputEvent(t *testing.T) {
	var buf bytes.Buffer
	e := NewEncoder(&buf)
	if err := e.Output(StreamStdout, "hi\n", "r-1"); err != nil {
		t.Fatal(err)
	}

	var ev Event
	if err := json.Unmarshal(bytes.TrimSpace(buf.Bytes()), &ev); err != nil {
		t.Fatal(err)
	}
	if ev.Type != EventOutput || ev.Stream != StreamStdout || ev.Data != "hi\n" || ev.RequestID != "r-1" {
		t.Fatalf("unexpected event: %+v", ev)
	}
}

func TestEncoderResult(t *testing.T) {
	var buf bytes.Buffer
	e := NewEncoder(&buf)
	if err := e.Result(3, map[string]any{"k": "v"}, "r-2"); err != nil {
		t.Fatal(err)
	}

	var ev Event
	if err := json.Unmarshal(bytes.TrimSpace(buf.Bytes()), &ev); err != nil {
		t.Fatal(err)
	}
	if ev.Exit == nil || *ev.Exit != 3 {
		t.Fatalf("exit = %v, want 3", ev.Exit)
	}
	if string(ev.Structured) != `{"k":"v"}` {
		t.Fatalf("structured = %s", ev.Structured)
	}
}

func TestEncoderErrorf(t *testing.T) {
	var buf bytes.Buffer
	e := NewEncoder(&buf)
	if err := e.Errorf("r-3", "boom %d", 42); err != nil {
		t.Fatal(err)
	}

	var ev Event
	if err := json.Unmarshal(bytes.TrimSpace(buf.Bytes()), &ev); err != nil {
		t.Fatal(err)
	}
	if ev.Type != EventError || ev.Message != "boom 42" || ev.RequestID != "r-3" {
		t.Fatalf("unexpected event: %+v", ev)
	}
}

func TestOneEventPerLine(t *testing.T) {
	var buf bytes.Buffer
	e := NewEncoder(&buf)
	if err := e.Output(StreamStdout, "a\nb\n", "r"); err != nil {
		t.Fatal(err)
	}
	if err := e.Result(0, nil, "r"); err != nil {
		t.Fatal(err)
	}

	sc := bufio.NewScanner(&buf)
	lines := 0
	for sc.Scan() {
		lines++
	}
	if lines != 2 {
		t.Fatalf("want 2 NDJSON lines, got %d", lines)
	}
}
