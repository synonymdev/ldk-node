# scorer-inspect

Offline diagnostics CLI for LDK serialized scorer files.

Reads a binary file produced by either:
- LDK's external scorer / `ProbabilisticScorer::write` (e.g. `https://api.blocktank.to/scorer-prod`, `https://scores.zeusln.com/latest.bin`), or
- ldk-node's `Node::export_pathfinding_scores`.

Both wire formats are identical in current LDK — `ProbabilisticScorer::write` just calls `self.channel_liquidities.write(w)`. The tool always reads via `ChannelLiquidities::read` and the `--source` flag is purely metadata for the report.

## Build

```
cargo build -p scorer-inspect --release
```

## Usage

```
scorer-inspect <FILE>
  [--source served|exported]
  [--output text|csv|json]
  [--save PATH]
  [--top N]
  [--all]
  [--sort narrow|recent|history]
```

- `--source` — annotates the report; doesn't affect parsing.
- `--output text` (default) — short summary + top-N channel rows for human review.
- `--output csv` — summary row, blank line, then one row per channel. Editor-friendly.
- `--output json` — `{ summary: {...}, channels: [...] }`. Machine-readable.
- `--save PATH` — write to file instead of stdout.
- `--top N` (default 20) — limit channel rows.
- `--all` — dump every entry; overrides `--top`.
- `--sort` — `narrow` (smallest offset window first; highest information density), `recent` (most recently updated first), `history` (largest historical-bucket weight first; most probe-derived signal).

## Examples

```
# Quick eyeball of Zeus's served file
curl -o /tmp/zeus.bin https://scores.zeusln.com/latest.bin
scorer-inspect /tmp/zeus.bin --source served

# Full per-channel CSV diff between Zeus and Bitkit
curl -o /tmp/blocktank.bin https://api.blocktank.to/scorer-prod
scorer-inspect /tmp/zeus.bin       --source served --all --output csv --save /tmp/zeus.csv
scorer-inspect /tmp/blocktank.bin  --source served --all --output csv --save /tmp/blocktank.csv
```

## What the columns mean

- `min_liquidity_offset_msat` / `max_liquidity_offset_msat` — non-directional offsets relative to the channel's node ordering. Resolving them into directional `min_liquidity_sat` / `max_liquidity_sat` requires a `NetworkGraph` (capacity + node-id ordering), which this tool doesn't yet take.
- `has_history` — whether either historical-bucket array is non-zero. The single best signal for distinguishing probe-derived data from synthetic graph seeding.
- `total_valid_points_tracked` (CSV/JSON), `history_weight` (text) — LDK-internal scalar weight summarizing the historical bucket distribution. Stored as `f64`; not an integer payment count.
- `last_updated_secs` — seconds since the unix epoch when either liquidity bound was last modified.

## Distinguishing rich probe data from synthetic seeding

A scorer file can be large for two very different reasons:

- **Probe-rich**: many entries with `has_history=true`, narrow `[min_offset, max_offset]` windows, sizable `total_valid_points_tracked`. This is what real probing produces.
- **Synthetic-coverage**: many entries with `has_history=false`, `min_offset=0`, `max_offset` close to channel capacity, zero bucket weights. This is what gossip-graph seeding produces — big file, low pathfinding signal.

Compare the two by running the tool against any scorer file and checking the `history populated` percentage and the offset-window distribution.

## Limits

- v1 is graph-free: directional output and capacity-resolved sats are not available without a `NetworkGraph`. Adding `--graph PATH` is tracked as future work.
