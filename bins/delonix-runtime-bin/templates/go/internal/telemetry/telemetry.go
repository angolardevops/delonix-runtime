// Package telemetry sets up OpenTelemetry traces and metrics.
//
// Spans are always recorded in-process, so every log line of a request
// carries its trace_id even with no collector anywhere. Export (OTLP over
// HTTP) is switched on only when an endpoint is configured through the
// standard variables (OTEL_EXPORTER_OTLP_ENDPOINT, or the per-signal
// _TRACES_/_METRICS_ variants), and never blocks startup: the exporters
// connect lazily, and a collector that is down costs dropped batches, not
// requests. OTEL_SDK_DISABLED=true turns everything off. Sampling and
// batching follow OTEL_TRACES_SAMPLER / OTEL_BSP_* as the SDK reads them.
package telemetry

import (
	"context"
	"errors"
	"log/slog"
	"strings"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/exporters/otlp/otlpmetric/otlpmetrichttp"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/propagation"
	sdkmetric "go.opentelemetry.io/otel/sdk/metric"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
)

// Shutdown flushes and stops the providers; it respects ctx's deadline.
type Shutdown func(ctx context.Context) error

// Setup installs the global tracer/meter providers and the W3C propagator.
// getenv is os.Getenv outside tests.
func Setup(ctx context.Context, getenv func(string) string, log *slog.Logger, service, version, env string) (Shutdown, error) {
	// Export failures (a collector that is down) become structured warnings
	// instead of the SDK's default plain-text line on stderr.
	otel.SetErrorHandler(otel.ErrorHandlerFunc(func(err error) {
		log.Warn("telemetry export failed", "error", err.Error())
	}))
	otel.SetTextMapPropagator(propagation.NewCompositeTextMapPropagator(
		propagation.TraceContext{}, propagation.Baggage{},
	))
	if strings.EqualFold(getenv("OTEL_SDK_DISABLED"), "true") {
		return func(context.Context) error { return nil }, nil
	}
	res, err := resource.New(ctx,
		resource.WithFromEnv(),
		resource.WithAttributes(
			attribute.String("service.name", service),
			attribute.String("service.version", version),
			attribute.String("deployment.environment.name", env),
		),
	)
	if err != nil {
		return nil, err
	}

	topts := []sdktrace.TracerProviderOption{sdktrace.WithResource(res)}
	var shutdowns []Shutdown
	if exporting(getenv, "TRACES") {
		exp, err := otlptracehttp.New(ctx)
		if err != nil {
			return nil, err
		}
		topts = append(topts, sdktrace.WithBatcher(exp))
	}
	tp := sdktrace.NewTracerProvider(topts...)
	otel.SetTracerProvider(tp)
	shutdowns = append(shutdowns, tp.Shutdown)

	if exporting(getenv, "METRICS") {
		exp, err := otlpmetrichttp.New(ctx)
		if err != nil {
			return nil, err
		}
		mp := sdkmetric.NewMeterProvider(
			sdkmetric.WithResource(res),
			sdkmetric.WithReader(sdkmetric.NewPeriodicReader(exp)),
		)
		otel.SetMeterProvider(mp)
		shutdowns = append(shutdowns, mp.Shutdown)
	}

	return func(ctx context.Context) error {
		var errs []error
		for _, s := range shutdowns {
			errs = append(errs, s(ctx))
		}
		return errors.Join(errs...)
	}, nil
}

// exporting: is an OTLP endpoint configured for this signal?
func exporting(getenv func(string) string, signal string) bool {
	return getenv("OTEL_EXPORTER_OTLP_ENDPOINT") != "" ||
		getenv("OTEL_EXPORTER_OTLP_"+signal+"_ENDPOINT") != ""
}
