// Cache backed by Redis (go-redis). Used only when REDIS_URL is set. Every
// call is bounded by a short timeout and every backend error is swallowed: a
// miss on Get, a no-op on Set. A down Redis therefore turns the cache off
// (every lookup falls through to the store) instead of failing requests. The
// URL — which carries the password — is NEVER logged; only the error type is,
// once per health transition, so an outage does not flood the log.
package cache

import (
	"context"
	"errors"
	"log/slog"
	"sync"
	"time"

	"github.com/redis/go-redis/v9"
)

// Redis is a Cache over a go-redis client.
type Redis struct {
	client  *redis.Client
	timeout time.Duration
	log     *slog.Logger

	mu       sync.Mutex
	degraded bool
}

// RedisOption tunes a Redis cache.
type RedisOption func(*Redis)

// WithTimeout bounds every Redis command (default 2s).
func WithTimeout(d time.Duration) RedisOption {
	return func(r *Redis) {
		if d > 0 {
			r.timeout = d
		}
	}
}

// WithLogger sets the logger for degrade/recover transitions.
func WithLogger(log *slog.Logger) RedisOption {
	return func(r *Redis) {
		if log != nil {
			r.log = log
		}
	}
}

// NewRedis builds a Cache from a redis:// / rediss:// URL. No connection is
// opened here — the first command connects, bounded by the timeout; the client
// reconnects by itself afterwards.
func NewRedis(url string, opts ...RedisOption) (*Redis, error) {
	opt, err := redis.ParseURL(url)
	if err != nil {
		return nil, err
	}
	r := &Redis{timeout: 2 * time.Second, log: slog.Default()}
	for _, o := range opts {
		o(r)
	}
	opt.DialTimeout = r.timeout
	opt.ReadTimeout = r.timeout
	opt.WriteTimeout = r.timeout
	// A down Redis must fail fast so the lookup falls through; do not retry.
	opt.MaxRetries = -1
	r.client = redis.NewClient(opt)
	return r, nil
}

func (r *Redis) Get(ctx context.Context, key string) ([]byte, bool) {
	ctx, cancel := context.WithTimeout(ctx, r.timeout)
	defer cancel()
	value, err := r.client.Get(ctx, key).Bytes()
	if errors.Is(err, redis.Nil) {
		r.recover()
		return nil, false // a real miss, not an error
	}
	if err != nil {
		r.degrade(err)
		return nil, false
	}
	r.recover()
	return value, true
}

func (r *Redis) Set(ctx context.Context, key string, value []byte, ttl time.Duration) {
	if ttl <= 0 {
		return
	}
	ctx, cancel := context.WithTimeout(ctx, r.timeout)
	defer cancel()
	if err := r.client.Set(ctx, key, value, ttl).Err(); err != nil {
		r.degrade(err)
		return
	}
	r.recover()
}

func (r *Redis) Healthy(ctx context.Context) bool {
	ctx, cancel := context.WithTimeout(ctx, r.timeout)
	defer cancel()
	return r.client.Ping(ctx).Err() == nil
}

func (r *Redis) Close() error { return r.client.Close() }

func (r *Redis) degrade(err error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if !r.degraded {
		r.degraded = true
		// The message carries the error type only, never the URL/password.
		r.log.Warn("cache backend unavailable, serving from the store",
			"error", errType(err))
	}
}

func (r *Redis) recover() {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.degraded {
		r.degraded = false
		r.log.Info("cache backend recovered")
	}
}

func errType(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()[:min(len(err.Error()), 64)]
}

var _ Cache = (*Redis)(nil)
