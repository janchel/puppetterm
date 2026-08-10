package action

import (
	"bytes"
	"context"
	"testing"

	"github.com/puppetterm/agent/internal/protocol"
)

func TestServiceValidation(t *testing.T) {
	cases := []struct {
		name   string
		params any
	}{
		{"bad unit (injection attempt)", map[string]any{"unit": "evil; rm -rf /", "op": "status"}},
		{"bad op", map[string]any{"unit": "ssh", "op": "explode"}},
		{"missing unit", map[string]any{"op": "status"}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			var buf bytes.Buffer
			enc := protocol.NewEncoder(&buf)
			if code := Service(context.Background(), reqWithParams(t, "service", c.params), enc); code == 0 {
				t.Fatal("want non-zero exit")
			}
			for _, ev := range parseEvents(t, &buf) {
				if ev.Type == protocol.EventError {
					return
				}
			}
			t.Fatal("missing error event")
		})
	}
}

// TestServiceStatus exercises the real systemctl (read-only op) when systemd
// is available; skipped otherwise.
func TestServiceStatus(t *testing.T) {
	if !systemdAvailable() {
		t.Skip("systemd not available")
	}
	events := captureEvents(t, Service, reqWithParams(t, "service", map[string]any{
		"unit": "ssh", "op": "status",
	}))
	for _, ev := range events {
		if ev.Type == protocol.EventResult {
			var r serviceResult
			if err := jsonUnmarshal(ev.Structured, &r); err != nil {
				t.Fatal(err)
			}
			if r.Unit != "ssh" || r.Op != "status" || r.Active == "" {
				t.Fatalf("unexpected result: %+v", r)
			}
			return
		}
	}
	t.Fatalf("missing result event: %+v", events)
}
