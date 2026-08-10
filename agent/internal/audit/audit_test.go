package audit

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRecordAppends(t *testing.T) {
	dir := t.TempDir()
	log := filepath.Join(dir, "audit.log")
	t.Setenv("PUPPETTERM_AUDIT_LOG", log)

	Record("service", "r-1", `{"unit":"nginx","op":"restart"}`, 0)
	Record("run", "r-2", strings.Repeat("x", 800), 3)

	data, err := os.ReadFile(log)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(string(data)), "\n")
	if len(lines) != 2 {
		t.Fatalf("want 2 lines, got %d: %q", len(lines), data)
	}
	if !strings.Contains(lines[0], "action=service") || !strings.Contains(lines[0], "request_id=r-1") || !strings.Contains(lines[0], "exit=0") {
		t.Fatalf("bad first line: %q", lines[0])
	}
	// Long params must be truncated to 500 + "..." (well under 700 total).
	if len(lines[1]) > 640 {
		t.Fatalf("params not truncated: line is %d bytes", len(lines[1]))
	}
	if !strings.Contains(lines[1], "...") {
		t.Fatalf("expected truncation marker in %q", lines[1])
	}
}
