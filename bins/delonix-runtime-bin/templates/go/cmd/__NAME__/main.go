// Command __NAME__ is the composition root: it reads the configuration,
// builds every adapter, hands them to the use cases and the HTTP transport,
// and owns the process lifecycle (signals, readiness, ordered shutdown).
package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"

	"__NAME__/internal/cache"
	"__NAME__/internal/config"
	"__NAME__/internal/health"
	"__NAME__/internal/httpapi"
	"__NAME__/internal/logging"
	"__NAME__/internal/memstore"
	"__NAME__/internal/messaging"
	"__NAME__/internal/notes"
	"__NAME__/internal/telemetry"
	"__NAME__/internal/webhook"
)

// version is set at build time: go build -ldflags "-X main.version=1.2.3".
var version = "dev"

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "fatal:", err)
		os.Exit(1)
	}
}

func run() error {
	cfg, err := config.FromEnv(version)
	if err != nil {
		return fmt.Errorf("configuration:\n%w", err)
	}
	log := logging.New(os.Stdout, cfg.LogLevel, cfg.ServiceName, cfg.Version, string(cfg.Env))

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()

	shutdownTelemetry, err := telemetry.Setup(ctx, os.Getenv, log, cfg.ServiceName, cfg.Version, string(cfg.Env))
	if err != nil {
		return fmt.Errorf("telemetry: %w", err)
	}

	var inboundKey []byte
	if cfg.InboundWebhookSecret != "" {
		if inboundKey, err = webhook.ParseSecret(cfg.InboundWebhookSecret); err != nil {
			return fmt.Errorf("WEBHOOK_INBOUND_SECRET: %w", err)
		}
	}

	// The note publishers: the outbound webhook dispatcher and/or RabbitMQ.
	// Wiring both is a fan-out, not a choice (CompositePublisher). Each is
	// non-blocking and bounded, so a slow or down target never stalls a request.
	var publishers []notes.Publisher
	var dispatcher *webhook.Dispatcher
	if cfg.OutboundWebhookURL != "" {
		key, err := webhook.ParseSecret(cfg.OutboundWebhookSecret)
		if err != nil {
			return fmt.Errorf("WEBHOOK_TARGET_SECRET: %w", err)
		}
		dispatcher = webhook.NewDispatcher(webhook.DispatcherConfig{
			URL:         cfg.OutboundWebhookURL,
			Key:         key,
			Client:      &http.Client{Transport: otelhttp.NewTransport(http.DefaultTransport)},
			Timeout:     5 * time.Second,
			MaxAttempts: 4,
			BaseBackoff: 500 * time.Millisecond,
			QueueSize:   100,
		}, log)
		publishers = append(publishers, dispatcher)
	}
	var rabbitPub *messaging.RabbitPublisher
	if cfg.RabbitURL != "" {
		rabbitPub = messaging.NewRabbitPublisher(cfg.RabbitURL, messaging.DialSink, log)
		rabbitPub.Start()
		publishers = append(publishers, rabbitPub)
	}
	var publisher notes.Publisher
	switch len(publishers) {
	case 0:
	case 1:
		publisher = publishers[0]
	default:
		publisher = messaging.NewComposite(publishers...)
	}

	// The read-through cache: Redis when REDIS_URL is set, the in-process cache
	// otherwise. It wraps the (traced) use cases from the outside.
	var noteCache cache.Cache = cache.NewInMemory()
	if cfg.RedisURL != "" {
		rc, err := cache.NewRedis(cfg.RedisURL, cache.WithLogger(log))
		if err != nil {
			return fmt.Errorf("REDIS_URL: %w", err)
		}
		noteCache = rc
	}

	// The consumer side of messaging: a down broker logs and is skipped, never
	// crashing startup.
	var consumer *messaging.RabbitConsumer
	if cfg.RabbitURL != "" {
		consumer = messaging.NewRabbitConsumer(cfg.RabbitURL, messaging.LogHandler(log), log)
		consumer.Start(ctx)
	}

	svc := notes.NewService(memstore.New(), publisher)
	cachedNotes := cache.NewCachedNotes(telemetry.TracedNotes{Next: svc}, noteCache, cfg.CacheTTL, log)
	ready := health.New(nil)
	srv := &http.Server{
		Addr: cfg.Addr,
		Handler: httpapi.New(httpapi.Deps{
			Notes:        cachedNotes,
			Readiness:    ready,
			Log:          log,
			MaxBodyBytes: cfg.MaxBodyBytes,
			WebhookKey:   inboundKey,
			Dedup:        webhook.NewDedup(4096),
		}),
		ReadHeaderTimeout: cfg.ReadHeaderTimeout,
		ReadTimeout:       cfg.ReadTimeout,
		WriteTimeout:      cfg.WriteTimeout,
		IdleTimeout:       cfg.IdleTimeout,
		MaxHeaderBytes:    1 << 20,
		ErrorLog:          slog.NewLogLogger(log.Handler(), slog.LevelWarn),
	}

	ln, err := net.Listen("tcp", cfg.Addr)
	if err != nil {
		return err
	}
	serveErr := make(chan error, 1)
	go func() { serveErr <- srv.Serve(ln) }()
	ready.SetReady()
	log.Info("listening", "addr", ln.Addr().String())

	select {
	case err := <-serveErr:
		return err
	case <-ctx.Done():
	}
	stop() // a second signal now kills the process the default way

	// Ordered shutdown, each step bounded by the same deadline: stop being
	// ready, let the load balancer notice, stop accepting and finish
	// in-flight requests, drain the outbound queue, flush telemetry.
	log.Info("shutting down", "timeout", cfg.ShutdownTimeout.String())
	ready.SetDraining()
	time.Sleep(cfg.DrainDelay)
	sctx, cancel := context.WithTimeout(context.Background(), cfg.ShutdownTimeout)
	defer cancel()
	var errs []error
	if err := srv.Shutdown(sctx); err != nil {
		errs = append(errs, fmt.Errorf("http: %w", err))
		_ = srv.Close()
	}
	if dispatcher != nil {
		if err := dispatcher.Close(sctx); err != nil {
			errs = append(errs, err)
		}
	}
	if rabbitPub != nil {
		if !rabbitPub.Close(sctx) {
			log.Warn("rabbit publish queue not drained before the deadline")
		}
	}
	if consumer != nil {
		if err := consumer.Close(sctx); err != nil {
			log.Warn("rabbit consumer close failed", "error", err.Error())
		}
	}
	if err := noteCache.Close(); err != nil {
		log.Warn("cache close failed", "error", err.Error())
	}
	// A collector that is down loses the last batch; it does not turn a
	// clean stop into a failed one.
	if err := shutdownTelemetry(sctx); err != nil {
		log.Warn("telemetry flush failed", "error", err.Error())
	}
	if err := errors.Join(errs...); err != nil {
		return err
	}
	log.Info("stopped")
	return nil
}
