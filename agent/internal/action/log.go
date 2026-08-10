package action

import (
	"context"
	"encoding/json"
	"os/exec"
	"strconv"

	"github.com/puppetterm/agent/internal/allow"
	"github.com/puppetterm/agent/internal/protocol"
)

type logParams struct {
	Path   string `json:"path"`
	Lines  int    `json:"lines"`  // number of tail lines (default 50)
	Follow bool   `json:"follow"` // tail -f
}

// Log tails a log file (optionally following). Paths are restricted to the
// allow-list (default: /var/log/).
func Log(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	var p logParams
	if err := json.Unmarshal(req.Params, &p); err != nil {
		_ = out.Errorf(req.RequestID, "invalid log params: %v", err)
		return 1
	}
	if p.Path == "" {
		_ = out.Errorf(req.RequestID, "log action requires a 'path'")
		return 1
	}
	cfg := allow.Load(allow.DefaultConfigPath)
	if !cfg.Allows(cfg.LogPrefixes, p.Path) {
		_ = out.Errorf(req.RequestID, "path %q is not in the allow-list", p.Path)
		return 1
	}
	if p.Lines <= 0 {
		p.Lines = 50
	}
	if p.Lines > 5000 {
		p.Lines = 5000
	}

	args := []string{"-n", strconv.Itoa(p.Lines)}
	if p.Follow {
		args = append(args, "-f")
	}
	cmd := exec.CommandContext(ctx, "tail", append(args, p.Path)...)
	code := execStream(ctx, req.RequestID, out, cmd, nil)

	_ = out.Result(0, map[string]any{
		"path": p.Path, "lines": p.Lines, "follow": p.Follow, "exit": code,
	}, req.RequestID)
	return 0
}
