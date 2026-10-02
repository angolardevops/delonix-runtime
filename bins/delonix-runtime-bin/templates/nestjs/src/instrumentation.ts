// Loaded before the application: `node --require ./dist/instrumentation.js
// dist/main.js`. Nothing else belongs here — the HTTP instrumentation has to
// be installed before `http`, Express and Nest are first required.
import { startTelemetry } from "./telemetry/otel";

startTelemetry(process.env);
