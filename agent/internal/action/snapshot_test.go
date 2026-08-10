package action

import (
	"encoding/json"
	"testing"

	"github.com/puppetterm/agent/internal/protocol"
)

func TestSnapshotStructured(t *testing.T) {
	events := captureEvents(t, Snapshot, reqWithParams(t, "snapshot", nil))
	for _, ev := range events {
		if ev.Type != protocol.EventResult || len(ev.Structured) == 0 {
			continue
		}
		var s SystemSnapshot
		if err := json.Unmarshal(ev.Structured, &s); err != nil {
			t.Fatal(err)
		}
		if s.Hostname == "" {
			t.Error("hostname is empty")
		}
		if s.UptimeSeconds <= 0 {
			t.Error("uptime_seconds is not positive")
		}
		if s.Mem.TotalKB == 0 {
			t.Error("mem.total_kb is 0")
		}
		if len(s.Disk) == 0 {
			t.Error("no disk entries")
		}
		return
	}
	t.Fatalf("missing structured result: %+v", events)
}
