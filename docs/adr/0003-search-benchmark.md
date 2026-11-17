# ADR 0003: M4 search benchmark against the Tantivy trigger

- Status: **recorded; the trigger did not fire for one searcher at a time, and was crossed with eight at once**.
- Date: 2026-11-18
- Tests: decision 12 in `docs/architecture.md`. Tantivy ships next if p95 search latency on the M4 benchmark corpus is above 500 ms.

## Method

Command:

```
cargo test --release -p server --test search_bench -- --ignored --nocapture
```

The test is `crates/server/tests/search_bench.rs`. It generates the corpus, then times searches through `GET /v1/workspaces/{id}/search` over HTTP, end to end.

The corpus is generated with a fixed seed and bulk-loaded with COPY. The GIN indexes are built after the load.

- One workspace with 200 channels, 50 private and 150 public.
- The searcher has joined 50 public and 10 private channels. Search therefore covers 160 channels: every public channel, because the searcher is not a guest, plus the 10 private ones.
- 1,000,000 messages spread over two years. Each has 4 to 40 words drawn from a 50,000-word vocabulary with a Zipf distribution (s = 1.07), the usual shape of natural-language text.
- 20,000 files, one on every 50th message. Each has a two-word name and about 300 words of extracted text.
- The database is 848 MB in total.

The query mix is 60 queries of each kind, run one at a time after 20 warm-up queries:

- **rare word:** frequency rank 20,000 to 50,000, a handful of hits;
- **mid word:** rank 1,000 to 5,000;
- **common word:** rank 5 to 100. By the distribution, each matches an estimated 15,000 to 300,000 of the searchable messages;
- **two words:** one common word and one mid word;
- **phrase:** two quoted words from the top 200.

A second phase runs 320 queries from the same mix with eight in flight at once.

Machine: Apple silicon Mac (18 cores, 48 GB). Postgres 16 ran in Docker Desktop's VM (4 vCPU, 8 GB) on tmpfs, with default settings (128 MB shared_buffers). `commonsd` and the client shared the host. This is not the 2 vCPU / 4 GB VPS.

## Results

| Query kind | p50 | p95 | max |
|---|---|---|---|
| rare word | 3.3 ms | 3.9 ms | 4.2 ms |
| mid word | 7.3 ms | 12.3 ms | 16.2 ms |
| common word | 154.5 ms | 329.0 ms | 482.8 ms |
| two words | 4.6 ms | 6.8 ms | 11.9 ms |
| phrase | 15.7 ms | 90.2 ms | 137.1 ms |
| **all, one at a time** (300 queries) | 7.5 ms | **257.3 ms** | p99 329.0 ms |
| **eight at a time** (320 queries in 5.0 s) | 17.4 ms | **527.6 ms** | |

The corpus loaded in 39 s and indexed in 7 s.

## Findings

1. **One searcher at a time, the trigger does not fire.** The p95 is 257 ms against the 500 ms threshold.
2. **The cost is common words.** Results sort by recency with rank as the tiebreaker, so for a common word every match is fetched and ranked before the top 20 come out. Rare and mid-frequency words, and any query with a second, narrower word, stay under 20 ms.
3. **Eight concurrent searchers cross 500 ms at p95 (528 ms).** Decision 12 doesn't say whether its p95 is measured with one searcher or under load. The run shared four VM cores with nothing else, but the client and server shared the host.

Whether finding 3 fires the trigger is for the user to decide. This run doesn't settle it. The VPS rerun in step 6 should measure both modes on the target hardware.

## Follow-ups

- Rerun on the 2 vCPU / 4 GB VPS with tuned Postgres memory, in both one-at-a-time and concurrent modes. Record the result against the trigger.
- If common words are the problem there, two fixes keep Postgres:
  - cap the matches considered per query, for example the newest 10,000;
  - or move rank out of the sort.

  Either changes the ordering rule in decision 12, so neither is applied here.
