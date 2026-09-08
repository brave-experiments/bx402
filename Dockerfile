FROM node:24-slim AS builder

WORKDIR /app
RUN corepack enable

# Dependencies first, so a source-only change reuses this layer. The workspace
# file carries the `ox` override, without which two copies of the Tempo decoder
# would install.
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile

COPY tsconfig.json tsconfig.build.json ./
COPY src ./src
RUN pnpm build

# Runtime stage: the compiled service, its runtime dependencies, and the CA
# certificates it needs to reach the upstreams over TLS.
FROM node:24-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 bx402

WORKDIR /app
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
# Only what the service runs on; the compiler and the test tools stay behind.
RUN pnpm install --frozen-lockfile --prod
COPY --from=builder /app/dist ./dist

# Expose the traffic port and the metrics port. Only the first should ever be
# reachable from outside the network.
EXPOSE 8080
EXPOSE 8090

# Run unprivileged: the proxy needs no root and binds 8080 and 8090 (>1024)
USER bx402
ENTRYPOINT ["node", "dist/main.js"]
