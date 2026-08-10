// Command puppetterm-agent is a stateless remote worker invoked through SSH.
// It reads a single action request as JSON on stdin and writes NDJSON events
// to stdout. It has no listener, no daemon, and no external dependencies.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"os"
	"time"

	"github.com/puppetterm/agent/internal/action"
	"github.com/puppetterm/agent/internal/audit"
	"github.com/puppetterm/agent/internal/protocol"
)

func main() {
	os.Exit(run(os.Stdin, os.Stdout))
}

func run(in io.Reader, outw io.Writer) int {
	out := protocol.NewEncoder(outw)

	raw, err := io.ReadAll(in)
	if err != nil {
		_ = out.Errorf("", "read stdin: %v", err)
		return 1
	}
	if len(bytes.TrimSpace(raw)) == 0 {
		_ = out.Errorf("", "empty request")
		return 1
	}

	var req protocol.Request
	if err := json.Unmarshal(raw, &req); err != nil {
		_ = out.Errorf("", "malformed request: %v", err)
		return 1
	}
	if req.Action == "" {
		_ = out.Errorf("", "request missing 'action'")
		return 1
	}

	ctx := context.Background()
	if req.TimeoutMS > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, time.Duration(req.TimeoutMS)*time.Millisecond)
		defer cancel()
	}

	reg := action.NewRegistry()
	code := reg.Handle(ctx, req, out)

	// Append-only audit log on the host (best-effort).
	audit.Record(req.Action, req.RequestID, string(req.Params), code)

	return code
}
