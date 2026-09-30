package config

import (
	"strings"
	"testing"
	"time"
)

func env(m map[string]string) func(string) string {
	return func(k string) string { return m[k] }
}

func TestDefaultsAreValid(t *testing.T) {
	c, err := Load(env(nil), "dev")
	if err != nil {
		t.Fatalf("defaults must load: %v", err)
	}
	if c.Addr != "0.0.0.0:__PORT__" || c.Env != Development || c.ShutdownTimeout != 15*time.Second {
		t.Fatalf("unexpected defaults: %+v", c)
	}
}

func TestEveryBadValueIsReportedAtOnce(t *testing.T) {
	_, err := Load(env(map[string]string{
		"PORT":             "http",
		"APP_ENV":          "staging",
		"LOG_LEVEL":        "loud",
		"SHUTDOWN_TIMEOUT": "soon",
	}), "dev")
	if err == nil {
		t.Fatal("invalid configuration was accepted")
	}
	for _, key := range []string{"PORT", "APP_ENV", "LOG_LEVEL", "SHUTDOWN_TIMEOUT"} {
		if !strings.Contains(err.Error(), key) {
			t.Errorf("error does not name %s: %v", key, err)
		}
	}
}

func TestOutboundWebhookNeedsASecretAndHTTPSInProduction(t *testing.T) {
	_, err := Load(env(map[string]string{"WEBHOOK_TARGET_URL": "http://hooks.example"}), "dev")
	if err == nil || !strings.Contains(err.Error(), "WEBHOOK_TARGET_SECRET") {
		t.Fatalf("a target without a secret must be refused: %v", err)
	}
	_, err = Load(env(map[string]string{
		"APP_ENV":               "production",
		"WEBHOOK_TARGET_URL":    "http://hooks.example",
		"WEBHOOK_TARGET_SECRET": "s",
	}), "dev")
	if err == nil || !strings.Contains(err.Error(), "https") {
		t.Fatalf("plain http in production must be refused: %v", err)
	}
}
