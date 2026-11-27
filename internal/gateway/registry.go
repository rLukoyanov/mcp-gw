// Package gateway exposes tokenmaxing's discovery tools to MCP clients.
package gateway

import (
	"sort"
	"strings"
	"sync"
	"unicode"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

const (
	defaultLimit = 5
	maxLimit     = 50
)

// ServerInfo is the compact description of an upstream server returned by
// search_servers.
type ServerInfo struct {
	Name        string           `json:"name"`
	Description string           `json:"description,omitempty"`
	Transport   config.Transport `json:"transport"`
	AutoConnect bool             `json:"autoConnect"`
	Connected   bool             `json:"connected"`
	ToolCount   int              `json:"toolCount"`
	Error       string           `json:"error,omitempty"`
}

// ToolMatch is the compact description of a tool returned by search_tools.
type ToolMatch struct {
	Server      string `json:"server"`
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
}

// ToolDetail is the full description of a tool returned by get_tool_details.
type ToolDetail struct {
	Server      string `json:"server"`
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
	InputSchema any    `json:"inputSchema,omitempty"`
}

type serverEntry struct {
	info  ServerInfo
	tools []*mcp.Tool
}

// Registry is an in-memory index of upstream servers and their tools.
// It is safe for concurrent use.
type Registry struct {
	mu      sync.RWMutex
	servers map[string]*serverEntry
}

// NewRegistry returns an empty registry.
func NewRegistry() *Registry {
	return &Registry{servers: make(map[string]*serverEntry)}
}

// AddServer records a configured server before it is connected.
func (r *Registry) AddServer(name string, transport config.Transport, description string, autoConnect bool) {
	r.mu.Lock()
	defer r.mu.Unlock()

	entry := r.ensure(name)
	entry.info.Name = name
	entry.info.Transport = transport
	entry.info.Description = description
	entry.info.AutoConnect = autoConnect
}

// MarkConnected records a successful connection and indexes the server's tools.
func (r *Registry) MarkConnected(name string, tools []*mcp.Tool) {
	r.mu.Lock()
	defer r.mu.Unlock()

	entry := r.ensure(name)
	entry.info.Connected = true
	entry.info.ToolCount = len(tools)
	entry.info.Error = ""
	entry.tools = tools
}

// MarkError records that a configured server could not be connected.
func (r *Registry) MarkError(name string, err error) {
	r.mu.Lock()
	defer r.mu.Unlock()

	entry := r.ensure(name)
	entry.info.Connected = false
	entry.info.ToolCount = 0
	entry.info.Error = err.Error()
	entry.tools = nil
}

// Server returns the summary of a named server.
func (r *Registry) Server(name string) (ServerInfo, bool) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	entry, ok := r.servers[name]
	if !ok {
		return ServerInfo{}, false
	}
	return entry.info, true
}

// Servers returns all known servers sorted by name.
func (r *Registry) Servers() []ServerInfo {
	r.mu.RLock()
	defer r.mu.RUnlock()

	out := make([]ServerInfo, 0, len(r.servers))
	for _, entry := range r.servers {
		out = append(out, entry.info)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// SearchServers ranks servers by how well their name and description match
// query. An empty query returns all servers sorted by name.
func (r *Registry) SearchServers(query string, limit int) []ServerInfo {
	r.mu.RLock()
	defer r.mu.RUnlock()

	type scored struct {
		info  ServerInfo
		score int
	}

	matches := make([]scored, 0, len(r.servers))
	for _, entry := range r.servers {
		score := matchScore(query,
			weightedText{entry.info.Name, 3},
			weightedText{entry.info.Description, 1},
		)
		if query != "" && score == 0 {
			continue
		}
		matches = append(matches, scored{info: entry.info, score: score})
	}

	sort.Slice(matches, func(i, j int) bool {
		if matches[i].score != matches[j].score {
			return matches[i].score > matches[j].score
		}
		return matches[i].info.Name < matches[j].info.Name
	})

	matches = trimToLimit(matches, limit)
	out := make([]ServerInfo, len(matches))
	for i, match := range matches {
		out[i] = match.info
	}
	return out
}

// SearchTools ranks tools across all connected servers. When server is
// non-empty only that server is searched.
func (r *Registry) SearchTools(query, server string, limit int) []ToolMatch {
	r.mu.RLock()
	defer r.mu.RUnlock()

	type scored struct {
		match ToolMatch
		score int
	}

	var matches []scored
	for name, entry := range r.servers {
		if server != "" && name != server {
			continue
		}
		for _, tool := range entry.tools {
			score := matchScore(query,
				weightedText{tool.Name, 3},
				weightedText{tool.Description, 2},
				weightedText{entry.info.Name, 1},
				weightedText{entry.info.Description, 1},
			)
			if query != "" && score == 0 {
				continue
			}
			matches = append(matches, scored{
				match: ToolMatch{
					Server:      name,
					Name:        tool.Name,
					Description: tool.Description,
				},
				score: score,
			})
		}
	}

	sort.Slice(matches, func(i, j int) bool {
		if matches[i].score != matches[j].score {
			return matches[i].score > matches[j].score
		}
		if matches[i].match.Server != matches[j].match.Server {
			return matches[i].match.Server < matches[j].match.Server
		}
		return matches[i].match.Name < matches[j].match.Name
	})

	matches = trimToLimit(matches, limit)
	out := make([]ToolMatch, len(matches))
	for i, match := range matches {
		out[i] = match.match
	}
	return out
}

// ToolDetails returns full tool descriptions. An exact, case-insensitive name
// match is preferred; otherwise tools whose name contains name are returned.
// When server is non-empty only that server is searched.
func (r *Registry) ToolDetails(server, name string) []ToolDetail {
	r.mu.RLock()
	defer r.mu.RUnlock()

	var exact, partial []ToolDetail
	for serverName, entry := range r.servers {
		if server != "" && serverName != server {
			continue
		}
		for _, tool := range entry.tools {
			detail := ToolDetail{
				Server:      serverName,
				Name:        tool.Name,
				Description: tool.Description,
				InputSchema: tool.InputSchema,
			}
			switch {
			case name != "" && strings.EqualFold(tool.Name, name):
				exact = append(exact, detail)
			case name != "" && strings.Contains(strings.ToLower(tool.Name), strings.ToLower(name)):
				partial = append(partial, detail)
			}
		}
	}

	sort.Slice(exact, func(i, j int) bool {
		if exact[i].Server != exact[j].Server {
			return exact[i].Server < exact[j].Server
		}
		return exact[i].Name < exact[j].Name
	})
	if len(exact) > 0 {
		return exact
	}

	sort.Slice(partial, func(i, j int) bool {
		if partial[i].Server != partial[j].Server {
			return partial[i].Server < partial[j].Server
		}
		return partial[i].Name < partial[j].Name
	})
	if partial == nil {
		return []ToolDetail{}
	}
	return partial
}

func (r *Registry) ensure(name string) *serverEntry {
	entry, ok := r.servers[name]
	if !ok {
		entry = &serverEntry{info: ServerInfo{Name: name}}
		r.servers[name] = entry
	}
	return entry
}

type weightedText struct {
	text   string
	weight int
}

// matchScore returns a relevance score for query against fields. A whole-query
// substring match is worth three times a single token match.
func matchScore(query string, fields ...weightedText) int {
	q := strings.ToLower(strings.TrimSpace(query))
	if q == "" {
		return 0
	}

	tokens := tokenize(q)
	score := 0
	for _, field := range fields {
		text := strings.ToLower(field.text)
		if text == "" {
			continue
		}
		if strings.Contains(text, q) {
			score += 3 * field.weight
		}
		for _, token := range tokens {
			if strings.Contains(text, token) {
				score += field.weight
			}
		}
	}
	return score
}

func tokenize(s string) []string {
	seen := make(map[string]struct{})
	var tokens []string
	for _, token := range strings.FieldsFunc(s, func(r rune) bool {
		return !unicode.IsLetter(r) && !unicode.IsNumber(r)
	}) {
		if _, ok := seen[token]; ok {
			continue
		}
		seen[token] = struct{}{}
		tokens = append(tokens, token)
	}
	return tokens
}

func trimToLimit[T any](items []T, limit int) []T {
	limit = limitOrDefault(limit)
	if len(items) > limit {
		return items[:limit]
	}
	return items
}

func limitOrDefault(limit int) int {
	switch {
	case limit <= 0:
		return defaultLimit
	case limit > maxLimit:
		return maxLimit
	default:
		return limit
	}
}
