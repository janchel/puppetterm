// Package allow implements allow-list checks for agent actions (logs, config).
package allow

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
)

// DefaultConfigPath is where an optional allow-list config can live.
// Override with the PUPPETTERM_CONFIG env var (used by tests and presets).
const DefaultConfigPath = "/etc/puppetterm/config.json"

// Config declares which path prefixes each action may access.
type Config struct {
	LogPrefixes    []string `json:"log_prefixes"`
	ConfigPrefixes []string `json:"config_prefixes"`
}

// Load reads the allow-list config, falling back to safe defaults.
func Load(path string) Config {
	if p := os.Getenv("PUPPETTERM_CONFIG"); p != "" {
		path = p
	}
	c := Config{LogPrefixes: []string{"/var/log/"}}
	if data, err := os.ReadFile(path); err == nil {
		var loaded Config
		if json.Unmarshal(data, &loaded) == nil {
			if loaded.LogPrefixes != nil {
				c.LogPrefixes = loaded.LogPrefixes
			}
			if loaded.ConfigPrefixes != nil {
				c.ConfigPrefixes = loaded.ConfigPrefixes
			}
		}
	}
	return c
}

// Allows reports whether path is inside (or equal to) one of the prefixes.
func (c Config) Allows(prefixes []string, path string) bool {
	clean := filepath.Clean(path)
	for _, p := range prefixes {
		p = filepath.Clean(p)
		if clean == p {
			return true
		}
		if strings.HasPrefix(clean, strings.TrimSuffix(p, "/")+"/") {
			return true
		}
	}
	return false
}
