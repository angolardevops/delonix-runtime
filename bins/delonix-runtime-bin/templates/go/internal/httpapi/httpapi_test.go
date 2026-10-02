package httpapi_test

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"sort"
	"strings"
	"testing"
	"time"

	"__NAME__/internal/health"
	"__NAME__/internal/httpapi"
	"__NAME__/internal/memstore"
	"__NAME__/internal/notes"
	"__NAME__/internal/webhook"
)

var testKey = bytes.Repeat([]byte{7}, 32)

func newServer(t *testing.T, pub notes.Publisher) (*httptest.Server, *health.Readiness) {
	t.Helper()
	ready := health.New(nil)
	ready.SetReady()
	h := httpapi.New(httpapi.Deps{
		Notes:        notes.NewService(memstore.New(), pub),
		Readiness:    ready,
		Log:          slog.New(slog.NewJSONHandler(io.Discard, nil)),
		MaxBodyBytes: 1024,
		WebhookKey:   testKey,
		Dedup:        webhook.NewDedup(16),
	})
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)
	return srv, ready
}

func do(t *testing.T, method, url string, body string, headers map[string]string) (*http.Response, map[string]any) {
	t.Helper()
	req, _ := http.NewRequest(method, url, strings.NewReader(body))
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var out map[string]any
	_ = json.NewDecoder(resp.Body).Decode(&out)
	return resp, out
}

func TestNoteLifecycle(t *testing.T) {
	srv, _ := newServer(t, nil)
	resp, created := do(t, "POST", srv.URL+"/api/v1/notes", `{"title":"first","body":"hello"}`, nil)
	if resp.StatusCode != 201 || created["id"] == "" || resp.Header.Get("Location") == "" {
		t.Fatalf("create: %d %v", resp.StatusCode, created)
	}
	resp, got := do(t, "GET", srv.URL+"/api/v1/notes/"+created["id"].(string), "", nil)
	if resp.StatusCode != 200 || got["title"] != "first" {
		t.Fatalf("get: %d %v", resp.StatusCode, got)
	}
	resp, page := do(t, "GET", srv.URL+"/api/v1/notes?limit=1", "", nil)
	if resp.StatusCode != 200 || page["total"].(float64) != 1 {
		t.Fatalf("list: %d %v", resp.StatusCode, page)
	}
}

func TestErrorsShareOneShape(t *testing.T) {
	srv, _ := newServer(t, nil)
	cases := []struct {
		method, path, body string
		status             int
		code               string
	}{
		{"POST", "/api/v1/notes", `{"title":""}`, 422, "validation_failed"},
		{"POST", "/api/v1/notes", `{"title":"x","extra":1}`, 400, "malformed_json"},
		{"POST", "/api/v1/notes", `{"title":"` + strings.Repeat("x", 2000) + `"}`, 413, "body_too_large"},
		{"GET", "/api/v1/notes?limit=500", "", 422, "validation_failed"},
		{"GET", "/api/v1/notes?offset=abc", "", 422, "validation_failed"},
		{"GET", "/api/v1/notes/missing", "", 404, "not_found"},
		{"GET", "/nowhere", "", 404, "not_found"},
	}
	for _, c := range cases {
		resp, body := do(t, c.method, srv.URL+c.path, c.body, nil)
		errObj, _ := body["error"].(map[string]any)
		if resp.StatusCode != c.status || errObj["code"] != c.code || errObj["request_id"] == "" {
			t.Errorf("%s %s: got %d %v, want %d %s", c.method, c.path, resp.StatusCode, body, c.status, c.code)
		}
	}
}

func TestReadinessFollowsTheLifecycle(t *testing.T) {
	srv, ready := newServer(t, nil)
	if resp, _ := do(t, "GET", srv.URL+"/api/v1/health/ready", "", nil); resp.StatusCode != 200 {
		t.Fatalf("ready: %d", resp.StatusCode)
	}
	ready.SetDraining()
	resp, body := do(t, "GET", srv.URL+"/api/v1/health/ready", "", nil)
	if resp.StatusCode != 503 || body["status"] != "draining" {
		t.Fatalf("draining: %d %v", resp.StatusCode, body)
	}
	if resp, _ := do(t, "GET", srv.URL+"/api/v1/health/live", "", nil); resp.StatusCode != 200 {
		t.Fatal("liveness must not follow readiness")
	}
}

func signed(id, body string, ts time.Time, key []byte) map[string]string {
	return map[string]string{
		webhook.HeaderID:        id,
		webhook.HeaderTimestamp: fmt.Sprint(ts.Unix()),
		webhook.HeaderSignature: webhook.Sign(key, id, ts, []byte(body)),
	}
}

func TestInboundWebhookVerifiesAndDeduplicates(t *testing.T) {
	srv, _ := newServer(t, nil)
	url := srv.URL + "/api/v1/webhooks/inbound"
	body := `{"type":"note.create","data":{"title":"from a webhook"}}`
	now := time.Now()

	if resp, _ := do(t, "POST", url, body, nil); resp.StatusCode != 400 {
		t.Fatalf("unsigned: %d", resp.StatusCode)
	}
	if resp, _ := do(t, "POST", url, body, signed("m1", body, now, []byte("wrong-key-wrong-key-wrong-key"))); resp.StatusCode != 401 {
		t.Fatalf("bad key: %d", resp.StatusCode)
	}
	if resp, _ := do(t, "POST", url, body, signed("m1", body, now.Add(-10*time.Minute), testKey)); resp.StatusCode != 401 {
		t.Fatalf("stale timestamp (replay): %d", resp.StatusCode)
	}
	tampered := signed("m1", body, now, testKey)
	if resp, _ := do(t, "POST", url, strings.Replace(body, "from", "FROM", 1), tampered); resp.StatusCode != 401 {
		t.Fatalf("tampered body: %d", resp.StatusCode)
	}
	for i := 0; i < 2; i++ {
		if resp, _ := do(t, "POST", url, body, signed("m1", body, now, testKey)); resp.StatusCode != 204 {
			t.Fatalf("delivery %d: %d", i, resp.StatusCode)
		}
	}
	_, page := do(t, "GET", srv.URL+"/api/v1/notes", "", nil)
	if page["total"].(float64) != 1 {
		t.Fatalf("a duplicate delivery was processed twice: %v", page)
	}
}

func TestParseSecretRequiresTheStandardForm(t *testing.T) {
	good := "whsec_" + base64.StdEncoding.EncodeToString(testKey)
	if _, err := webhook.ParseSecret(good); err != nil {
		t.Fatal(err)
	}
	for _, bad := range []string{"", "plain", "whsec_!!", "whsec_" + base64.StdEncoding.EncodeToString([]byte("short"))} {
		if _, err := webhook.ParseSecret(bad); err == nil {
			t.Errorf("accepted %q", bad)
		}
	}
}

// TestOpenAPIMatchesRoutes is the contract drift gate: api/openapi.yaml and
// the route table must list the same method+path pairs.
func TestOpenAPIMatchesRoutes(t *testing.T) {
	raw, err := os.ReadFile("../../api/openapi.yaml")
	if err != nil {
		t.Fatal(err)
	}
	pathRe := regexp.MustCompile(`^  (/\S+):\s*$`)
	methodRe := regexp.MustCompile(`^    (get|post|put|patch|delete):\s*$`)
	var documented []string
	var current string
	for _, line := range strings.Split(string(raw), "\n") {
		if m := pathRe.FindStringSubmatch(line); m != nil {
			current = m[1]
		} else if m := methodRe.FindStringSubmatch(line); m != nil && current != "" {
			documented = append(documented, strings.ToUpper(m[1])+" "+current)
		} else if !strings.HasPrefix(line, " ") && line != "" {
			current = ""
		}
	}
	var served []string
	for _, r := range httpapi.Routes(httpapi.Deps{WebhookKey: testKey}) {
		served = append(served, r.Method+" "+r.Pattern)
	}
	sort.Strings(documented)
	sort.Strings(served)
	if strings.Join(documented, "\n") != strings.Join(served, "\n") {
		t.Fatalf("contract drift\n documented: %v\n served:     %v", documented, served)
	}
}
