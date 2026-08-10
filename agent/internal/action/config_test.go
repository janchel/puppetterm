package action

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/puppetterm/agent/internal/protocol"
)

func TestConfigReadWrite(t *testing.T) {
	dir := t.TempDir()
	cfg := filepath.Join(dir, "cfg.json")
	if err := os.WriteFile(cfg, []byte(`{"config_prefixes":["`+dir+`"]}`), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PUPPETTERM_CONFIG", cfg)

	target := filepath.Join(dir, "app.conf")
	if err := os.WriteFile(target, []byte("hello\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	// read
	events := captureEvents(t, Config, reqWithParams(t, "config", map[string]any{
		"path": target, "op": "read",
	}))
	var out strings.Builder
	for _, ev := range events {
		if ev.Type == protocol.EventOutput {
			out.WriteString(ev.Data)
		}
	}
	if out.String() != "hello\n" {
		t.Fatalf("read = %q", out.String())
	}

	// write (direct — the temp file is user-writable)
	captureEvents(t, Config, reqWithParams(t, "config", map[string]any{
		"path": target, "op": "write", "content": "updated\n",
	}))
	data, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "updated\n" {
		t.Fatalf("write content = %q", string(data))
	}
}

func TestConfigDenied(t *testing.T) {
	dir := t.TempDir()
	cfg := filepath.Join(dir, "cfg.json")
	if err := os.WriteFile(cfg, []byte(`{"config_prefixes":[]}`), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PUPPETTERM_CONFIG", cfg)

	// Config prefixes empty → everything denied.
	var buf bytes.Buffer
	enc := protocol.NewEncoder(&buf)
	code := Config(context.Background(), reqWithParams(t, "config", map[string]any{
		"path": "/etc/hosts", "op": "read",
	}), enc)
	if code == 0 {
		t.Fatal("want non-zero exit")
	}
	for _, ev := range parseEvents(t, &buf) {
		if ev.Type == protocol.EventError && strings.Contains(ev.Message, "allow-list") {
			return
		}
	}
	t.Fatal("missing allow-list error")
}
