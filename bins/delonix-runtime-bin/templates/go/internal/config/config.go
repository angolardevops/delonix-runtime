// Package config loads the service configuration from the environment and
// validates it once, at startup. A bad value stops the process before it
// listens, with every problem listed at once.
package config

import (
	"errors"
	"fmt"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"
)

// Environment is where the process runs. Production turns the dev
// shortcuts off.
type Environment string

const (
	Development Environment = "development"
	Test        Environment = "test"
	Production  Environment = "production"
)

// Config is the whole runtime configuration.
type Config struct {
	ServiceName string
	Version     string
	Env         Environment
	LogLevel    string

	Addr              string
	ReadHeaderTimeout time.Duration
	ReadTimeout       time.Duration
	WriteTimeout      time.Duration
	IdleTimeout       time.Duration
	MaxBodyBytes      int64
	ShutdownTimeout   time.Duration
	// DrainDelay keeps serving (with readiness at 503) before the listener
	// closes, so a load balancer has time to stop routing here.
	DrainDelay time.Duration

	// InboundWebhookSecret enables POST /api/v1/webhooks/inbound. Empty:
	// the endpoint answers 404.
	InboundWebhookSecret string
	// OutboundWebhookURL receives a signed event per created note. Empty:
	// nothing is sent.
	OutboundWebhookURL    string
	OutboundWebhookSecret string
}

// Load reads the environment. `version` is the build's own version (set by
// the linker), not something the environment may override.
func Load(getenv func(string) string, version string) (Config, error) {
	var errs []error
	get := func(key, def string) string {
		if v := strings.TrimSpace(getenv(key)); v != "" {
			return v
		}
		return def
	}
	dur := func(key string, def time.Duration) time.Duration {
		raw := get(key, "")
		if raw == "" {
			return def
		}
		d, err := time.ParseDuration(raw)
		if err != nil || d < 0 {
			errs = append(errs, fmt.Errorf("%s: %q is not a non-negative duration (e.g. 15s)", key, raw))
			return def
		}
		return d
	}

	c := Config{
		ServiceName:       get("SERVICE_NAME", "__NAME__"),
		Version:           version,
		Env:               Environment(get("APP_ENV", string(Development))),
		LogLevel:          strings.ToLower(get("LOG_LEVEL", "info")),
		ReadHeaderTimeout: dur("HTTP_READ_HEADER_TIMEOUT", 5*time.Second),
		ReadTimeout:       dur("HTTP_READ_TIMEOUT", 15*time.Second),
		WriteTimeout:      dur("HTTP_WRITE_TIMEOUT", 15*time.Second),
		IdleTimeout:       dur("HTTP_IDLE_TIMEOUT", 60*time.Second),
		ShutdownTimeout:   dur("SHUTDOWN_TIMEOUT", 15*time.Second),
		DrainDelay:        dur("DRAIN_DELAY", 0),

		InboundWebhookSecret:  get("WEBHOOK_INBOUND_SECRET", ""),
		OutboundWebhookURL:    get("WEBHOOK_TARGET_URL", ""),
		OutboundWebhookSecret: get("WEBHOOK_TARGET_SECRET", ""),
	}

	switch c.Env {
	case Development, Test, Production:
	default:
		errs = append(errs, fmt.Errorf("APP_ENV: %q is not one of development, test, production", c.Env))
	}
	switch c.LogLevel {
	case "debug", "info", "warn", "error":
	default:
		errs = append(errs, fmt.Errorf("LOG_LEVEL: %q is not one of debug, info, warn, error", c.LogLevel))
	}

	port := get("PORT", "__PORT__")
	if n, err := strconv.Atoi(port); err != nil || n < 1 || n > 65535 {
		errs = append(errs, fmt.Errorf("PORT: %q is not a port number (1-65535)", port))
	}
	c.Addr = get("HTTP_HOST", "0.0.0.0") + ":" + port

	c.MaxBodyBytes = 1 << 20
	if raw := get("HTTP_MAX_BODY_BYTES", ""); raw != "" {
		n, err := strconv.ParseInt(raw, 10, 64)
		if err != nil || n < 1 {
			errs = append(errs, fmt.Errorf("HTTP_MAX_BODY_BYTES: %q is not a positive integer", raw))
		} else {
			c.MaxBodyBytes = n
		}
	}

	if c.OutboundWebhookURL != "" {
		u, err := url.Parse(c.OutboundWebhookURL)
		if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
			errs = append(errs, fmt.Errorf("WEBHOOK_TARGET_URL: %q is not an http(s) URL", c.OutboundWebhookURL))
		} else if c.Env == Production && u.Scheme != "https" {
			errs = append(errs, errors.New("WEBHOOK_TARGET_URL: production sends signed events over https only"))
		}
		if c.OutboundWebhookSecret == "" {
			errs = append(errs, errors.New("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set"))
		}
	}

	return c, errors.Join(errs...)
}

// FromEnv is Load over the process environment.
func FromEnv(version string) (Config, error) {
	return Load(os.Getenv, version)
}
