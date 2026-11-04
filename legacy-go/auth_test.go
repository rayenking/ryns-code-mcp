package main

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// fakeMCP returns a stand-in for the Streamable HTTP handler so tests can
// observe authMiddleware routing without spinning up a real MCP server.
func fakeMCP() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/api/save/test" && r.Method == http.MethodPost {
			w.Header().Set("Content-Type", "application/json")
			w.Write([]byte(`{"ok":true}`))
			return
		}
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("mcp-ok"))
	})
}

// applyAuth installs the given token list in authTokens (restoring the
// original on test cleanup) and wraps an authenticated mux.
func applyAuth(t *testing.T, tokens []string, mcpHandler http.Handler) http.Handler {
	t.Helper()
	orig := authTokens
	authTokens = tokens
	t.Cleanup(func() { authTokens = orig })
	return authMiddleware(buildMux(t.TempDir(), mcpHandler))
}

func TestAuthMiddlewareUnauthorizedWithoutHeader(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	resp, err := http.Post(srv.URL+"/api/save/test", "application/json", strings.NewReader(`{}`))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("no header: status = %d, want 401", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
		t.Errorf("content-type = %q, want application/json", ct)
	}
	buf := make([]byte, 100)
	n, _ := resp.Body.Read(buf)
	if got := strings.TrimSpace(string(buf[:n])); got != `{"error":"unauthorized"}` {
		t.Errorf("body = %q, want unauthorized JSON", got)
	}
}

func TestAuthMiddlewareBearerAccepted(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
	req.Header.Set("Authorization", "Bearer testtok")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("valid bearer: status = %d, want 200", resp.StatusCode)
	}
}

func TestAuthMiddlewareXRynsAuthAccepted(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
	req.Header.Set("X-RYNS-Auth", "testtok")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("valid X-RYNS-Auth: status = %d, want 200", resp.StatusCode)
	}
}

func TestAuthMiddlewareWrongToken(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
	req.Header.Set("Authorization", "Bearer nope")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("wrong token: status = %d, want 401", resp.StatusCode)
	}
}

func TestAuthMiddlewareBearerCaseInsensitive(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
	req.Header.Set("Authorization", "bearer testtok")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("lowercase scheme: status = %d, want 200", resp.StatusCode)
	}
}

func TestAuthMiddlewareEmptyTokenList(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, nil, fakeMCP()))
	defer srv.Close()
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
	req.Header.Set("Authorization", "Bearer testtok")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("empty token list: status = %d, want 401", resp.StatusCode)
	}
}

func TestAuthMiddlewareAssetsRoute(t *testing.T) {
	assetsDir := t.TempDir()
	if err := os.WriteFile(filepath.Join(assetsDir, "sheet.html"), []byte("<h1>ok</h1>"), 0644); err != nil {
		t.Fatal(err)
	}
	orig := authTokens
	authTokens = []string{"testtok"}
	t.Cleanup(func() { authTokens = orig })
	srv := httptest.NewServer(authMiddleware(buildMux(assetsDir, fakeMCP())))
	defer srv.Close()
	// without token -> 401
	resp, err := http.Get(srv.URL + "/assets/sheet.html")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("/assets/ without token: status = %d, want 401", resp.StatusCode)
	}
	// with token -> 200 (served file)
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/assets/sheet.html", nil)
	req.Header.Set("Authorization", "Bearer testtok")
	resp, err = http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("/assets/ with token: status = %d, want 200", resp.StatusCode)
	}
}

func TestAuthMiddlewareMultiTokenSecondMatches(t *testing.T) {
	srv := httptest.NewServer(applyAuth(t, []string{"first", "second"}, fakeMCP()))
	defer srv.Close()
	for i, tok := range []string{"first", "second"} {
		req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
		req.Header.Set("Authorization", "Bearer "+tok)
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		// Both the first and the SECOND (last) configured token must be accepted.
		if i == 1 && resp.StatusCode != http.StatusOK {
			t.Fatalf("second token: status = %d, want 200", resp.StatusCode)
		}
		if resp.StatusCode != http.StatusOK {
			t.Fatalf("token %q: status = %d, want 200", tok, resp.StatusCode)
		}
	}
}

func TestAuthMiddlewareBadBearerGoodXRynsAuth(t *testing.T) {
	// Candidates from both headers are validated together: a bad Bearer value
	// must not cancel a valid X-RYNS-Auth value (fix 5).
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/mcp", nil)
	req.Header.Set("Authorization", "Bearer nope")
	req.Header.Set("X-RYNS-Auth", "testtok")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("bad Bearer + good X-RYNS-Auth: status = %d, want 200", resp.StatusCode)
	}
}

func TestAuthMiddlewareGuardsOddPaths(t *testing.T) {
	// The middleware is path-independent and runs before ServeMux dispatch
	// (we wrap the whole mux), so URL-cleaning or encoding tricks such as
	// `/assets/%2e%2e/x` or `//mcp` cannot bypass auth. ServeMux's own
	// cleaning of already-authenticated odd paths is out of scope here:
	// httptest's listen loop may normalize RequestURI before the handler
	// sees it, so we only assert that every such variant still hits the 401
	// gate, not the post-auth routing behavior.
	srv := httptest.NewServer(applyAuth(t, []string{"testtok"}, fakeMCP()))
	defer srv.Close()
	for _, p := range []string{"/assets/%2e%2e/x", "//mcp", "/assets/..%2fx"} {
		req, _ := http.NewRequest(http.MethodGet, srv.URL+p, nil)
		// Use DefaultTransport directly (DefaultClient.Transport is nil and
		// only resolved inside Client.Do) and no-follow semantics so any
		// path-cleaning redirect the server might add for an authenticated
		// route is not followed during this no-auth assertion.
		resp, err := http.DefaultTransport.RoundTrip(req)
		if err != nil {
			t.Fatalf("%s: %v", p, err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusUnauthorized {
			t.Fatalf("%s without token: status = %d, want 401", p, resp.StatusCode)
		}
	}
}

func TestFilterAuthEnv(t *testing.T) {
	cases := []struct {
		name string
		in   []string
		want []string
	}{
		{
			name: "exact key with value dropped",
			in:   []string{"HOME=/x", "RYNS_AUTH_TOKEN=secret", "OTHER=y"},
			want: []string{"HOME=/x", "OTHER=y"},
		},
		{
			name: "bare key dropped",
			in:   []string{"RYNS_AUTH_TOKEN"},
			want: []string{},
		},
		{
			name: "empty value KEY= dropped",
			in:   []string{"RYNS_AUTH_TOKEN=", "OTHER=y"},
			want: []string{"OTHER=y"},
		},
		{
			name: "lowercase variant dropped (Windows is case-insensitive)",
			in:   []string{"ryns_auth_token=secret", "OTHER=y"},
			want: []string{"OTHER=y"},
		},
		{
			name: "compound key RYNS_AUTH_TOKEN_FOO kept",
			in:   []string{"RYNS_AUTH_TOKEN_FOO=x"},
			want: []string{"RYNS_AUTH_TOKEN_FOO=x"},
		},
		{
			name: "other RYNS_* keys kept",
			in:   []string{"RYNS_WORKSPACE_ROOT=/w"},
			want: []string{"RYNS_WORKSPACE_ROOT=/w"},
		},
	}
	for _, c := range cases {
		got := filterAuthEnv(c.in)
		if len(got) != len(c.want) {
			t.Errorf("%s: got %v, want %v", c.name, got, c.want)
			continue
		}
		for i := range got {
			if got[i] != c.want[i] {
				t.Errorf("%s: got[%d]=%q, want[%d]=%q", c.name, i, got[i], i, c.want[i])
			}
		}
	}
}

func TestChildEnvReadsProcessEnv(t *testing.T) {
	t.Setenv("RYNS_AUTH_TOKEN", "s3cr3t")
	t.Setenv("RYNS_AUTH_TOKEN_FOO", "keepme")
	// Also set a lowercase variant with a raw Setenv (t.Setenv is fine for
	// it too on darwin; the assertion is about the filter, not the OS).
	if err := os.Setenv("ryns_auth_token", "s3cr3t"); err != nil {
		t.Fatal(err)
	}
	env := childEnv()
	for _, kv := range env {
		if strings.EqualFold(strings.SplitN(kv, "=", 2)[0], "RYNS_AUTH_TOKEN") {
			t.Errorf("childEnv() kept token entry %q", kv)
		}
	}
	kept := false
	for _, kv := range env {
		if kv == "RYNS_AUTH_TOKEN_FOO=keepme" {
			kept = true
		}
	}
	if !kept {
		t.Error("childEnv() dropped RYNS_AUTH_TOKEN_FOO, compound keys must be kept")
	}
}

func TestParseAuthTokens(t *testing.T) {
	orig := authTokens
	t.Cleanup(func() { authTokens = orig })
	t.Setenv("RYNS_AUTH_TOKEN", " a , b,  ,c ")
	authTokens = parseAuthTokens()
	if len(authTokens) != 3 || authTokens[0] != "a" || authTokens[1] != "b" || authTokens[2] != "c" {
		t.Fatalf("parseAuthTokens = %v, want [a b c]", authTokens)
	}
	t.Setenv("RYNS_AUTH_TOKEN", "")
	authTokens = parseAuthTokens()
	if len(authTokens) != 0 {
		t.Fatalf("empty env: %v, want no tokens", authTokens)
	}
}
