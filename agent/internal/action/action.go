// Package action implements the typed actions the remote agent can execute.
package action

import (
	"context"

	"github.com/puppetterm/agent/internal/protocol"
)

// Handler executes a single action: it writes events to out and returns the
// process exit code the agent should exit with (0 on success, non-zero on
// failure).
type Handler func(ctx context.Context, req protocol.Request, out *protocol.Encoder) int

// Registry maps action names to handlers.
type Registry struct {
	handlers map[string]Handler
}

// NewRegistry returns a Registry preloaded with the built-in actions.
func NewRegistry() *Registry {
	r := &Registry{handlers: make(map[string]Handler)}
	r.Register("run", Run)
	r.Register("snapshot", Snapshot)
	r.Register("service", Service)
	r.Register("log", Log)
	r.Register("config", Config)
	r.Register("read", Read)
	return r
}

// Register adds or replaces an action handler.
func (r *Registry) Register(name string, h Handler) {
	r.handlers[name] = h
}

// Handle dispatches to the named action and returns the process exit code.
// Unknown actions emit an error event and return 1.
func (r *Registry) Handle(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	h, ok := r.handlers[req.Action]
	if !ok {
		_ = out.Errorf(req.RequestID, "unknown action %q", req.Action)
		return 1
	}
	return h(ctx, req, out)
}
