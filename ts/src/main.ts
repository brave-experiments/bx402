import { serve } from "@hono/node-server";
import { app, banner } from "./app.js";
import { configFromEnv } from "./config.js";
import { initLogging, log } from "./log.js";
import { Metrics, serveMetrics } from "./metrics.js";
import { initScreener, statusLine } from "./screener.js";

/** Port serving public traffic. */
const PORT = 8080;

/**
 * Boot the service: load configuration, wire dependencies, and serve until
 * shutdown. Every startup failure leaves through a rejected promise to the
 * single exit site below.
 */
async function run(): Promise<void> {
  // Load a local `.env` for development. Real environment variables still take
  // precedence, and an absent file is not an error.
  try {
    process.loadEnvFile();
  } catch {
    // No `.env` beside the process, which is the normal case in production.
  }
  initLogging();

  log.info(banner());

  // Built before anything that records into it.
  const metrics = new Metrics();

  const config = configFromEnv();
  log.info(`brave search api: ${config.braveSearchApiBaseUrl}`);
  log.info(
    config.x402 === undefined
      ? "x402 rail: disabled by ENABLED_RAILS"
      : `x402 facilitator: ${config.x402.facilitatorUrl}`,
  );
  log.info(
    config.mpp === undefined
      ? "mpp rail: disabled by ENABLED_RAILS"
      : `mpp tempo rpc: ${config.mpp.rpcUrl}`,
  );

  // A configured but unreachable bucket aborts startup, so the service never
  // serves traffic with a broken screener.
  const { status } = await initScreener(config, metrics);
  log.info(`restricted address screening: ${statusLine(status)}`);

  const server = serve(
    { fetch: app(config, metrics).fetch, hostname: "0.0.0.0", port: PORT },
    (address) => {
      log.info(`listening on ${address.address}:${address.port}`);
    },
  );

  // Traffic and metrics are served on separate listeners, so the public port
  // never exposes the metrics. Serve until the process is stopped; a failure on
  // either listener, a port already in use above all, leaves through this
  // promise to the single exit site below instead of crashing on an unhandled
  // error event.
  await Promise.race([
    new Promise<never>((_resolve, reject) => {
      server.on("error", reject);
    }),
    serveMetrics(metrics),
  ]);
}

run().catch((err: unknown) => {
  log.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
});
