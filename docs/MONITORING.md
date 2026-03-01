# Monitoring

Grafana dashboard shows: exchange connectivity, messages/sec, errors/sec, decode/e2e latency histograms (P50/P99/P99.9), uptime.

## Health and Metrics Endpoints

```bash
curl localhost:9090/health    # OK, DEGRADED, or DOWN
curl localhost:9090/metrics   # Prometheus text format
```

## Docker Stack

`docker compose up` starts the full monitoring stack:

| Service | Port | Description |
|---------|------|-------------|
| Prometheus | [localhost:9091](http://localhost:9091) | Metrics scraping |
| Grafana | [localhost:3000](http://localhost:3000) | Pre-built dashboard (anonymous access) |

Dashboard config: `monitoring/grafana/dashboards/orderbook.json`

## Exposed Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `orderbook_messages_total{exchange}` | Counter | WebSocket messages received |
| `orderbook_errors_total{exchange}` | Counter | Parse/connection errors |
| `orderbook_reconnections_total{exchange}` | Counter | Reconnection attempts |
| `orderbook_merges_total` | Counter | Merge operations |
| `orderbook_exchange_up{exchange}` | Gauge | Connection status (1=up) |
| `orderbook_uptime_seconds` | Gauge | Process uptime |
| `orderbook_decode_duration_seconds{exchange}` | Histogram | JSON decode latency |
| `orderbook_e2e_duration_seconds` | Histogram | End-to-end latency |

## Health Logic

- **OK** (200): All exchanges connected
- **DEGRADED** (200): At least one exchange connected
- **DOWN** (503): No exchanges connected

Implementation: `src/metrics.rs` -- lock-free `AtomicU64` counters, O(1) histogram `record()`, cumulative sums computed lazily on scrape.
