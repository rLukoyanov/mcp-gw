// Package config loads and validates the tokenmaxing configuration.
//
// The configuration file is JSON and compatible with Claude Desktop:
//
//	{
//	  "mcpServers": {
//	    "local": {
//	      "command": "npx",
//	      "args": ["-y", "some-mcp-server"],
//	      "env": {"KEY": "value"},
//	      "description": "What this server provides"
//	    },
//	    "remote": {
//	      "url": "https://example.com/mcp",
//	      "headers": {"Authorization": "Bearer ..."},
//	      "description": "Remote MCP server",
//	      "autoConnect": false
//	    }
//	  }
//	}
//
// A server either runs as a local process ("command") or is reached over
// HTTP/SSE ("url"); exactly one of the two must be set. "description" is
// optional metadata for discovery. "autoConnect" defaults to true: set it to
// false to keep a server disconnected at startup so its tools are only loaded
// when the model explicitly targets it. Unknown fields are ignored so existing
// Claude Desktop configuration files can be reused as-is.
package config

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// Config is the root of the configuration file.
type Config struct {
	// Servers maps a user-chosen server name to its connection settings.
	Servers map[string]Server `json:"mcpServers"`
}

// Server describes how to connect to a single MCP server.
type Server struct {
	// Command starts a local MCP server over stdio.
	Command string `json:"command,omitempty"`
	// Args are passed to Command.
	Args []string `json:"args,omitempty"`
	// Env is added to the environment of Command.
	Env map[string]string `json:"env,omitempty"`

	// URL is the endpoint of a remote MCP server. Use it instead of Command.
	URL string `json:"url,omitempty"`
	// Headers are sent with every request to a remote MCP server.
	Headers map[string]string `json:"headers,omitempty"`

	// Description is optional metadata shown to users.
	Description string `json:"description,omitempty"`
	// AutoConnect controls whether the server is connected at startup.
	// It defaults to true; false keeps the server disconnected until it is
	// explicitly targeted, so startup does not pay for its tool list.
	AutoConnect *bool `json:"autoConnect,omitempty"`
}

// AutoConnectEnabled reports whether the server should be connected at
// startup. An omitted field means true.
func (s Server) AutoConnectEnabled() bool {
	return s.AutoConnect == nil || *s.AutoConnect
}

// Transport is the connection type of an MCP server.
type Transport string

const (
	// TransportStdio runs a local process and speaks MCP over stdin/stdout.
	TransportStdio Transport = "stdio"
	// TransportHTTP talks to a remote MCP server over HTTP.
	TransportHTTP Transport = "http"
)

// EnvConfigPath names the environment variable that overrides the default
// configuration file path.
const EnvConfigPath = "TOKENMAXING_CONFIG"

// Transport reports how the server should be reached.
func (s Server) Transport() (Transport, error) {
	hasCommand := strings.TrimSpace(s.Command) != ""
	hasURL := strings.TrimSpace(s.URL) != ""

	switch {
	case hasCommand && hasURL:
		return "", errors.New(`set either "command" or "url", not both`)
	case hasCommand:
		return TransportStdio, nil
	case hasURL:
		return TransportHTTP, nil
	default:
		return "", errors.New(`one of "command" or "url" is required`)
	}
}

// Validate checks that the server can be connected to.
func (s Server) Validate() error {
	if _, err := s.Transport(); err != nil {
		return err
	}
	if s.URL == "" {
		return nil
	}

	u, err := url.Parse(s.URL)
	if err != nil {
		return fmt.Errorf("invalid url: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return fmt.Errorf("invalid url %q: scheme must be http or https", s.URL)
	}
	if u.Host == "" {
		return fmt.Errorf("invalid url %q: host is required", s.URL)
	}
	return nil
}

// Validate checks every configured server and reports all problems at once.
func (c *Config) Validate() error {
	if c.Servers == nil {
		c.Servers = map[string]Server{}
	}

	var errs []error
	for name, server := range c.Servers {
		if strings.TrimSpace(name) == "" {
			errs = append(errs, errors.New("server name must not be empty"))
			continue
		}
		if err := server.Validate(); err != nil {
			errs = append(errs, fmt.Errorf("server %q: %w", name, err))
		}
	}
	return errors.Join(errs...)
}

// Names returns the configured server names in sorted order.
func (c *Config) Names() []string {
	names := make([]string, 0, len(c.Servers))
	for name := range c.Servers {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// Empty reports whether no servers are configured.
func (c *Config) Empty() bool {
	return len(c.Servers) == 0
}

// Load reads, parses and validates the configuration file at path.
func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, fmt.Errorf("config file not found: %s (pass --config or set %s)", path, EnvConfigPath)
		}
		return nil, err
	}
	cfg, err := Parse(data)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return cfg, nil
}

// Parse parses and validates configuration data.
func Parse(data []byte) (*Config, error) {
	var cfg Config
	dec := json.NewDecoder(bytes.NewReader(data))
	if err := dec.Decode(&cfg); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	if _, err := dec.Token(); err != io.EOF {
		return nil, errors.New("parse config: unexpected data after top-level object")
	}
	if err := cfg.Validate(); err != nil {
		return nil, err
	}
	return &cfg, nil
}

// ResolvePath returns the configuration file path to use. An explicit path
// wins, then the TOKENMAXING_CONFIG environment variable, then the standard
// user configuration directory.
func ResolvePath(explicit string) (string, error) {
	if explicit != "" {
		return explicit, nil
	}
	if env := os.Getenv(EnvConfigPath); env != "" {
		return env, nil
	}
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", fmt.Errorf("resolve default config path: %w", err)
	}
	return filepath.Join(dir, "tokenmaxing", "config.json"), nil
}
