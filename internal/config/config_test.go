package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParse(t *testing.T) {
	tests := []struct {
		name    string
		data    string
		wantErr string
	}{
		{
			name: "stdio server",
			data: `{"mcpServers":{"fs":{"command":"npx","args":["-y","srv"],"env":{"A":"b"}}}}`,
		},
		{
			name: "http server",
			data: `{"mcpServers":{"remote":{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer x"}}}}`,
		},
		{
			name: "no servers",
			data: `{}`,
		},
		{
			name: "unknown fields ignored",
			data: `{"mcpServers":{"s":{"command":"x","autoApprove":true}}}`,
		},
		{
			name:    "both command and url",
			data:    `{"mcpServers":{"s":{"command":"x","url":"https://example.com"}}}`,
			wantErr: "not both",
		},
		{
			name:    "neither command nor url",
			data:    `{"mcpServers":{"s":{"args":["x"]}}}`,
			wantErr: "is required",
		},
		{
			name:    "unsupported url scheme",
			data:    `{"mcpServers":{"s":{"url":"ftp://example.com"}}}`,
			wantErr: "scheme must be http or https",
		},
		{
			name:    "url without host",
			data:    `{"mcpServers":{"s":{"url":"https://"}}}`,
			wantErr: "host is required",
		},
		{
			name:    "empty server name",
			data:    `{"mcpServers":{"  ":{"command":"x"}}}`,
			wantErr: "name must not be empty",
		},
		{
			name:    "invalid json",
			data:    `{`,
			wantErr: "parse config",
		},
		{
			name:    "trailing data",
			data:    `{} {}`,
			wantErr: "unexpected data",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, err := Parse([]byte(tt.data))
			if tt.wantErr == "" {
				if err != nil {
					t.Fatalf("Parse() error = %v, want nil", err)
				}
				return
			}
			if err == nil {
				t.Fatalf("Parse() = nil, want error containing %q", tt.wantErr)
			}
			if !strings.Contains(err.Error(), tt.wantErr) {
				t.Fatalf("Parse() error = %q, want error containing %q", err, tt.wantErr)
			}
		})
	}
}

func TestParseReportsAllServerErrors(t *testing.T) {
	_, err := Parse([]byte(`{"mcpServers":{
		"a":{"command":"x","url":"https://example.com"},
		"b":{}
	}}`))
	if err == nil {
		t.Fatal("Parse() = nil, want error")
	}
	for _, want := range []string{`server "a"`, `server "b"`} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("Parse() error = %q, want it to contain %q", err, want)
		}
	}
}

func TestNamesSorted(t *testing.T) {
	cfg, err := Parse([]byte(`{"mcpServers":{
		"zeta":{"command":"z"},
		"alpha":{"command":"a"},
		"mid":{"command":"m"}
	}}`))
	if err != nil {
		t.Fatalf("Parse() error = %v", err)
	}
	got := cfg.Names()
	want := []string{"alpha", "mid", "zeta"}
	if len(got) != len(want) {
		t.Fatalf("Names() = %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("Names() = %v, want %v", got, want)
		}
	}
}

func TestAutoConnect(t *testing.T) {
	tests := []struct {
		name string
		data string
		want bool
	}{
		{
			name: "defaults to true",
			data: `{"mcpServers":{"s":{"command":"x"}}}`,
			want: true,
		},
		{
			name: "explicit true",
			data: `{"mcpServers":{"s":{"command":"x","autoConnect":true}}}`,
			want: true,
		},
		{
			name: "explicit false",
			data: `{"mcpServers":{"s":{"command":"x","autoConnect":false}}}`,
			want: false,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg, err := Parse([]byte(tt.data))
			if err != nil {
				t.Fatalf("Parse() error = %v", err)
			}
			if got := cfg.Servers["s"].AutoConnectEnabled(); got != tt.want {
				t.Fatalf("AutoConnectEnabled() = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestTransport(t *testing.T) {
	tests := []struct {
		name   string
		server Server
		want   Transport
	}{
		{name: "stdio", server: Server{Command: "npx"}, want: TransportStdio},
		{name: "http", server: Server{URL: "https://example.com"}, want: TransportHTTP},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := tt.server.Transport()
			if err != nil {
				t.Fatalf("Transport() error = %v", err)
			}
			if got != tt.want {
				t.Fatalf("Transport() = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestLoad(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.json")
	data := `{"mcpServers":{"fs":{"command":"npx","args":["-y","srv"]}}}`
	if err := os.WriteFile(path, []byte(data), 0o600); err != nil {
		t.Fatal(err)
	}

	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if len(cfg.Servers) != 1 {
		t.Fatalf("Load() servers = %d, want 1", len(cfg.Servers))
	}

	if _, err := Load(filepath.Join(dir, "missing.json")); err == nil {
		t.Fatal("Load() of missing file = nil, want error")
	}

	badPath := filepath.Join(dir, "bad.json")
	if err := os.WriteFile(badPath, []byte(`{"mcpServers":{"s":{}}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(badPath); err == nil || !strings.Contains(err.Error(), badPath) {
		t.Fatalf("Load() error = %v, want it to mention %s", err, badPath)
	}
}

func TestResolvePath(t *testing.T) {
	t.Run("explicit path wins", func(t *testing.T) {
		t.Setenv(EnvConfigPath, "/from/env.json")
		got, err := ResolvePath("/explicit.json")
		if err != nil {
			t.Fatal(err)
		}
		if got != "/explicit.json" {
			t.Fatalf("ResolvePath() = %q, want /explicit.json", got)
		}
	})

	t.Run("environment variable", func(t *testing.T) {
		t.Setenv(EnvConfigPath, "/from/env.json")
		got, err := ResolvePath("")
		if err != nil {
			t.Fatal(err)
		}
		if got != "/from/env.json" {
			t.Fatalf("ResolvePath() = %q, want /from/env.json", got)
		}
	})

	t.Run("default user config dir", func(t *testing.T) {
		t.Setenv(EnvConfigPath, "")
		got, err := ResolvePath("")
		if err != nil {
			t.Fatal(err)
		}
		want := filepath.Join("tokenmaxing", "config.json")
		if !strings.HasSuffix(got, want) {
			t.Fatalf("ResolvePath() = %q, want suffix %q", got, want)
		}
	})
}
