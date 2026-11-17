# ADR 0002: M2 load test, 500 clients on one workspace

- Status: **recorded; the proposed latency budget is not met on this setup**.
- Date: 2026-11-18
- Tests: decision 9 (throughput of the per-workspace seq row) and acceptance criterion 7 (proposed p95 send-to-receive under 300 ms).

## Method

Command:

```
ulimit -n 10240
cargo test --release -p server --test load -- --ignored --nocapture
```

The test is `crates/server/tests/load.rs`. It sets up one workspace with 500 members, each holding one WebSocket that receives `#general`. Message bodies carry the send time, so each receiver records send-to-receive latency. goose drives the senders. The run has three phases:

- **A, paced:** 500 goose users each send once every 10 s, about 50 sends per second, for 30 s.
- **B, unpaced:** 500 goose users send as fast as they can for 15 s. Commits per second is the change in the workspace seq divided by elapsed time.
- **C, burst:** 1,000 concurrent writes. Drain time is measured until every one of the 500 sockets has received all of them.

Everything ran on one Apple silicon Mac (18 cores, 48 GB): `commonsd`, Postgres 16 in Docker, the 500 receiving sockets and the goose senders. This is not the 2 vCPU / 4 GB VPS the criterion names, so the client side competes with the server for CPU. Two Postgres storage setups were run:

- **tmpfs:** the testcontainers default. Commit fsync is nearly free.
- **Disk, bind-mounted to the host through Docker Desktop's virtiofs**, with `COMMONS_LOAD_DATABASE_URL`. `pg_test_fsync` reported open_datasync at 67 µs per operation. Docker's own VM disk is full on this machine, so a VM volume could not be used.

## Results

| Measure | tmpfs | virtiofs disk |
|---|---|---|
| A: sends per second (goose counter, after ramp-up) | 53 | 48 |
| A: deliveries recorded | 986,500 (every message reached all 500 sockets) | 810,000 |
| A: send-to-receive p50 / p95 / p99 | 73 / **727** / 831 ms | 1,508 / **3,393** / 3,723 ms |
| A: POST latency, mean | 134 ms | 1,193 ms |
| B: sustained commits per second, one workspace | **271** (earlier runs: 363 and 405) | **65** |
| B: POST latency, mean | 1,716 ms | 5,814 ms |
| C: 1,000-write burst, all committed | 3.1 s | over 120 s; the run did not finish draining |
| C: every socket has all 1,000 | **5.7 s** | not drained within 120 s |
| Sockets dropped as slow consumers | 0 | 0 |

## Findings

1. **The first fan-out design failed under load and was replaced.** Every socket re-ran the filtered replay query on every wakeup, so one commit cost 500 queries, and the burst never drained. Decision 11 says each *node* reads the rows. The implementation now has one feed per workspace per node (`crates/server/src/feed.rs`):
   - the feed reads each batch once and shares it with that node's sockets;
   - each socket filters the batch in memory against its user's access set, by the same rule as the SQL replay (inside an interval at the event's seq, and still open at the batch's watermark);
   - a socket that falls behind catches up with its own replay.

   `crates/server/tests/socket_filter.rs` checks that socket delivery equals HTTPS replay after every step of random membership histories.
2. **The proposed budget is missed on this setup.** p95 is 727 ms against the proposed 300 ms, even with free fsync. The median is 73 ms, so the tail is queueing. The load generator shares the machine and decodes every frame on all 500 sockets. The budget should be set from the VPS run in step 6, with the load generator on another host, as the plan's testing strategy says.
3. **The throughput assumption in decision 9 doesn't hold here.** Decision 9 assumes about 500 commits per second per workspace. With every send fanned out to 500 sockets, this run reached 271 to 405 on tmpfs and 65 on virtiofs. A send also inserts a notify job and an idempotency key, and the pool is capped at 32 connections; connection acquisition hit the 2 s slow threshold under phase B. Plan §7 says to revisit the per-workspace counter if this assumption fails. That needs the VPS measurement first, because virtiofs fsync is not representative of a VPS SSD.

## Follow-ups

- Rerun on the 2 vCPU / 4 GB VPS, with the senders and receivers on a separate host, and set the budget from that run (step 6).
- Profile phase A's tail. Likely candidates:
  - the 32-connection pool;
  - one notify job per message, even when the policy notifies nobody;
  - per-socket JSON framing.
