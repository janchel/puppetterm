package action

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/puppetterm/agent/internal/protocol"
)

func systemdAvailable() bool {
	err := exec.Command("systemctl", "is-system-running").Run()
	return err == nil || isExitErr(err)
}

func isExitErr(err error) bool {
	_, ok := err.(*exec.ExitError)
	return ok
}

func jsonUnmarshal(data []byte, v any) error {
	return json.Unmarshal(data, v)
}

func TestLogTail(t *testing.T) {
	dir := t.TempDir()
	cfg := filepath.Join(dir, "cfg.json")
	if err := os.WriteFile(cfg, []byte(`{"log_prefixes":["`+dir+`"]}`), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PUPPETTERM_CONFIG", cfg)

	logFile := filepath.Join(dir, "test.log")
	if err := os.WriteFile(logFile, []byte("line1\nline2\nline3\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	events := captureEvents(t, Log, reqWithParams(t, "log", map[string]any{
		"path": logFile, "lines": 2,
	}))
	var got strings.Builder
	for _, ev := range events {
		if ev.Type == protocol.EventOutput {
			got.WriteString(ev.Data)
		}
	}
	if !strings.Contains(got.String(), "line2") || !strings.Contains(got.String(), "line3") {
		t.Fatalf("tail output = %q", got.String())
	}
	if strings.Contains(got.String(), "line1") {
		t.Fatalf("tail should have dropped line1: %q", got.String())
	}
}

func TestLogDenied(t *testing.T) {
	dir := t.TempDir()
	cfg := filepath.Join(dir, "cfg.json")
	if err := os.WriteFile(cfg, []byte(`{"log_prefixes":[]}`), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PUPPETTERM_CONFIG", cfg)

	var buf bytes.Buffer
	enc := protocol.NewEncoder(&buf)
	if code := Log(context.Background(), reqWithParams(t, "log", map[string]any{
		"path": "/etc/shadow",
	}), enc); code == 0 {
		t.Fatal("want non-zero exit")
	}
	for _, ev := range parseEvents(t, &buf) {
		if ev.Type == protocol.EventError && strings.Contains(ev.Message, "allow-list") {
			return
		}
	}
	t.Fatal("missing allow-list error")
}
