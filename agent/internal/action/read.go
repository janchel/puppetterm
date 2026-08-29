package action

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"strconv"
	"strings"

	"github.com/puppetterm/agent/internal/protocol"
)

type readParams struct {
	Path   string `json:"path"`
	Offset int `json:"offset,omitempty"` // 1-based starting line (default 1)
	Limit  int `json:"limit,omitempty"`  // max lines to return (default 200, max 5000)
}

const readDefaultLimit = 200
const readMaxLimit = 5000

// Read returns a bounded, line-range slice of a file — for paging through
// large logs/configs without dumping the whole file into the model context.
// Output is line-numbered; raise `offset` to page further, or use grep via
// run_command to jump straight to the relevant section.
func Read(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	var p readParams
	if err := json.Unmarshal(req.Params, &p); err != nil {
		_ = out.Errorf(req.RequestID, "invalid read params: %v", err)
		return 1
	}
	if p.Path == "" {
		_ = out.Errorf(req.RequestID, "read action requires a 'path'")
		return 1
	}
	if p.Offset < 1 {
		p.Offset = 1
	}
	limit := p.Limit
	if limit <= 0 {
		limit = readDefaultLimit
	}
	if limit > readMaxLimit {
		limit = readMaxLimit
	}

	f, err := os.Open(p.Path)
	if err != nil {
		_ = out.Errorf(req.RequestID, "open %s: %v", p.Path, err)
		return 1
	}
	defer f.Close()

	scanner := bufio.NewScanner(f)
	// Allow long lines (up to 4 MB) so logs with huge records don't split.
	scanner.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)

	var sb strings.Builder
	lineNo := 0
	emitted := 0
	for scanner.Scan() {
		lineNo++
		if lineNo < p.Offset {
			continue
		}
		if emitted >= limit {
			break
		}
		sb.WriteString(strconv.Itoa(lineNo))
		sb.WriteString("\t")
		sb.WriteString(scanner.Text())
		sb.WriteString("\n")
		emitted++
	}
	if err := scanner.Err(); err != nil {
		_ = out.Errorf(req.RequestID, "read %s: %v", p.Path, err)
		return 1
	}

	_ = out.Output(protocol.StreamStdout, sb.String(), req.RequestID)
	_ = out.Result(0, map[string]any{
		"path":        p.Path,
		"offset":      p.Offset,
		"limit":       limit,
		"emitted":     emitted,
		"total_lines": lineNo,
		"note":        "line-numbered; raise 'offset' to page further, or grep via run_command to find the relevant section first",
	}, req.RequestID)
	return 0
}
