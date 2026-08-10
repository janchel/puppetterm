package action

import (
	"context"
	"encoding/json"
	"io"
	"os"
	"os/exec"
	"strings"

	"github.com/puppetterm/agent/internal/allow"
	"github.com/puppetterm/agent/internal/protocol"
)

type configParams struct {
	Path    string `json:"path"`
	Op      string `json:"op"` // read | write
	Content string `json:"content,omitempty"`
}

// Config reads/writes config files, restricted to allow-listed prefixes.
// Writes try a direct write first, then fall back to `sudo -n tee` (which
// requires the scoped sudoers grant added by the installer).
func Config(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	var p configParams
	if err := json.Unmarshal(req.Params, &p); err != nil {
		_ = out.Errorf(req.RequestID, "invalid config params: %v", err)
		return 1
	}
	if p.Path == "" {
		_ = out.Errorf(req.RequestID, "config action requires a 'path'")
		return 1
	}
	if p.Op != "read" && p.Op != "write" {
		_ = out.Errorf(req.RequestID, "config op must be 'read' or 'write', got %q", p.Op)
		return 1
	}
	cfg := allow.Load(allow.DefaultConfigPath)
	if !cfg.Allows(cfg.ConfigPrefixes, p.Path) {
		_ = out.Errorf(req.RequestID, "path %q is not in the config allow-list", p.Path)
		return 1
	}

	switch p.Op {
	case "read":
		data, err := os.ReadFile(p.Path)
		if err != nil {
			_ = out.Errorf(req.RequestID, "read %s: %v", p.Path, err)
			return 1
		}
		_ = out.Output(protocol.StreamStdout, string(data), req.RequestID)
		_ = out.Result(0, map[string]any{"path": p.Path, "op": "read", "bytes": len(data)}, req.RequestID)
		return 0
	case "write":
		// Direct write first (works for user-writable allow-listed files).
		if err := os.WriteFile(p.Path, []byte(p.Content), 0o644); err == nil {
			_ = out.Result(0, map[string]any{"path": p.Path, "op": "write", "method": "direct"}, req.RequestID)
			return 0
		}
		// Fall back to the scoped write helper (installed by the installer's
		// preset and granted NOPASSWD via sudoers). The helper enforces its
		// own path allow-list, so sudoers needs no wildcards.
		helper := os.Getenv("PUPPETTERM_WRITE_HELPER")
		if helper == "" {
			helper = "/usr/local/lib/puppetterm/write-file"
		}
		cmd := exec.CommandContext(ctx, "sudo", "-n", helper, p.Path)
		cmd.Stdout = io.Discard
		cmd.Stderr = io.Discard
		code := execStream(ctx, req.RequestID, out, cmd, strings.NewReader(p.Content))
		_ = out.Result(0, map[string]any{
			"path": p.Path, "op": "write", "method": "sudo-helper", "exit": code,
		}, req.RequestID)
		return 0
	}
	return 0
}
