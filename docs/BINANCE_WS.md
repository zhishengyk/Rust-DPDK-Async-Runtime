# BTCUSDT perpetual: DPDK TCP/TLS/WebSocket latency

The new `binance-ws` binary extends the existing receive path with a user-space TCP connection, verified TLS, a streaming WebSocket codec, and typed BTCUSDT market parsing. It uses the existing single-core runtime to hand the parsed market to the application task. It does not use a kernel TCP socket for market traffic. DNS is resolved before the measured receive loop.

## Run

```bash
cargo build --release -p binance-ws --locked
./scripts/run-binance-ws.sh --duration-sec 180 --warmup-sec 5 \
  --output results/binance-ws/book-180.json --csv results/binance-ws/book-180.csv

./scripts/run-binance-ws.sh --stream agg-trade --duration-sec 180 \
  --output results/binance-ws/trade-180.json --csv results/binance-ws/trade-180.csv
```

Only one process may own the DPDK port at once, including the existing A/B clients. The normal host preparation still applies. These commands use `env.sh` and the second ENI, preserving the first ENI for SSH. The default gateway is `10.202.0.1`, subnet prefix 20, network core 2 and statistics core 3. These defaults are specific to this EC2 configuration; override them when moving the test. The second ENI must have a usable Internet route, security group permissions and public IP/NAT access. No AWS resources or NIC bindings are changed by this binary.

The default stream is `wss://fstream.binance.com/public/ws/btcusdt@bookTicker`. Aggregate trades use `wss://fstream.binance.com/market/ws/btcusdt@aggTrade`. Use `--remote-ip` to hold the endpoint fixed between trials; TLS continues to verify `--host` with trusted roots. TLS verification cannot be disabled through this CLI. No compression is negotiated.

## What is measured

The requested ideal interval is **DMA completion / queue-ready -> application has a fully parsed market**. Ordinary DPDK does not expose a generic per-packet DMA-completion timestamp. Hardware RX timestamps are a different measurement point, normally before host-side DMA, and need clock-domain calibration before comparison with CPU TSC.

This binary probes the installed PMD/device's `RTE_ETH_RX_OFFLOAD_TIMESTAMP` capability. It records that capability, but does not enable or consume hardware timestamps. `dma_completion_exactly_measured` is always false. It does not infer that a newer PMD or a different ENA device lacks hardware timestamp support.

| Metric, in nanoseconds | Boundary |
|---|---|
| `rx_last_to_json` | RX burst returned the last required packet -> typed JSON fields ready in reactor |
| `rx_last_to_app` | Same RX point -> application task owns the parsed market |
| `completion_to_json_upper_bound` | Previous empty RX poll start -> typed JSON fields ready |
| `completion_to_app_upper_bound` | Previous empty RX poll start -> application owns parsed market |
| `completion_observation_window` | Non-empty RX return minus previous empty RX poll start |
| `rx_first_to_json`, `rx_first_to_app` | Earliest contributing RX -> corresponding endpoint; includes waiting for more fragments |
| `rx_to_tcp_record` | Last required RX -> TCP bytes copied/assembled into a complete TLS record |
| `tls` | Complete encrypted TLS record ready -> rustls processes/authenticates the record |
| `websocket` | TLS processing done -> WebSocket frame/message available for JSON parsing |
| `json` | Frame/message available -> borrowed JSON parsed and numeric fields converted |
| `dispatch` | Parsed market ready -> application receives it, including reactor remainder/queue cleanup/wakeup/scheduling |
| `tcp_record_to_app` | Complete encrypted record ready -> application receives parsed market |

For CPU-observable descriptor readiness, a preceding empty poll and a later successful poll provide an observation interval. The latency is bracketed by `rx_last_to_*` and `completion_to_*_upper_bound`. **This brackets observable CQ readiness, not the exact instant of the final DMA write.** There is no directly observed hardware arrival or DMA-completion time in these measurements. The interval can widen when the application has work to process or the CPU is interrupted. Empty-poll start is used rather than empty-poll return, avoiding a false lower bound when readiness changes during the empty poll.

TSC is ordered with the existing `LFENCE/RDTSC/LFENCE` sequence and converted using `rte_get_tsc_hz`. Timing reads and tracing overhead are included, not subtracted. HDR uses four significant figures, 1 ns bins through 32767 ns, but bin width is not absolute measurement accuracy.

Each row in the optional CSV corresponds to one parsed message. For that same message, the TCP-record, TLS, WS, JSON and dispatch components add up to `rx_last_to_app` in TSC ticks. Independent percentile summaries must not be added. Nanosecond conversion rounds down, so the sum of converted components may differ by a few ns.

## Timestamp provenance and low latency

`json_payload_bytes` separately reports the count, total, minimum and maximum size of successfully parsed complete JSON messages, including warmup. It counts skipped fields and reassembled WebSocket fragments, excludes protocol headers and control frames, and records its scope explicitly. It does not change the fixed-size `Market` or `Delivery` structures. Raw market JSON is not written to disk.

RX timestamps are carried with observed TCP sequence spans and matched to bytes actually drained from smoltcp. The bounded sidecar handles ordered delivery, out-of-order packets, sequence-number wrapping, duplicate arrivals and partial reads. First received copies win over retransmissions. A missing timestamp span or capacity overflow fails the benchmark rather than silently substituting the most recent unrelated packet timestamp.

Complete TLS records are authenticated before plaintext is consumed. All messages in one TLS record share that record's contributing RX provenance, since the full authenticated record is required. A fragmented WebSocket message merges its contributing records. First-RX timing can include network gaps while waiting for remaining TCP/TLS/WS pieces. Last-required-RX timing isolates processing after the contributing packets have been observed. Handshake and warmup are excluded from statistics.

The network/crypto/codec/parser/reactor stay on core 2; histogram insertion and optional CSV output run on core 3 through a bounded batched SPSC queue. Buffer and metadata capacities are preallocated, Nagle and delayed ACK are disabled, and there is no per-message terminal output. The custom buffers do not grow silently on the receive path. This is not a claim that every internal rustls operation is allocation-free. Capacity errors, transmission failures, statistics backpressure, NIC errors, message counts and mempool ownership are reported.

CPU affinity is not full CPU isolation. Before interpreting tail latency, verify the active and saved boot parameters (`isolcpus`, `nohz_full`, `rcu_nocbs`), irqbalance's current and boot state, CPU exclusions, IRQ affinity, and DPDK helper-thread affinity. `scripts/capture-binance-environment.py results/binance-ws/environment.json` captures these settings without changing them. Local timer, reschedule and cross-CPU interrupts can still affect a pinned thread. TSC intervals include guest scheduling, interruptions and any virtual-machine pauses occurring inside a measured segment; a long TLS segment is not by itself evidence of slow cryptographic computation.

The WS codec handles split/coalesced frames, text continuations, interleaved ping/pong and masked client controls. It rejects masked server frames, unrequested compression, invalid frame lengths and oversized messages. A fully parsed market is queued before waking the application; parsing inside the reactor avoids copying raw text across the task boundary.

This is a benchmark client, not a complete trading gateway: it has one TCP connection/stream, bounded buffers, a finite run, no reconnection policy, no trading authentication and no order submission. Any connection/protocol failure produces diagnostics and a failing exit code. Shutdown closes the connection, drains statistics and checks that all mbufs are returned.

## Validate

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
```

Tests cover TCP timestamp provenance across wrapping/reordering/retransmission, split and coalesced frames, fragmented text with interleaved controls, handshake/frame rejection, and paired component arithmetic. Live trials validate TCP routing, trusted TLS, WebSocket upgrade, real BTCUSDT market reception and benchmark termination.

References: [Binance endpoint mapping](https://developers.binance.com/en/docs/products/derivatives-trading-usds-futures/websocket-market-streams/Important-WebSocket-Change-Notice), [DPDK software/hardware callback timestamps](https://doc.dpdk.org/guides/sample_app_ug/rxtx_callbacks.html), [smoltcp 0.12](https://docs.rs/smoltcp/0.12.0/smoltcp/), [rustls](https://docs.rs/rustls/).
