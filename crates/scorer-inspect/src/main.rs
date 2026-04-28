//! Offline diagnostics for LDK serialized scorer files.
//!
//! Reads a binary file produced by either:
//!   - LDK's external scorer / `ProbabilisticScorer::write`, or
//!   - ldk-node's `Node::export_pathfinding_scores`.
//! Both are wire-compatible — they both serialize a `ChannelLiquidities` —
//! so this tool always reads via `ChannelLiquidities::read`. The `--source`
//! flag is purely metadata for the report.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use lightning::io::Cursor;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use humansize::{format_size, BINARY};
use lightning::routing::scoring::{ChannelLiquidities, ChannelLiquidityDiagnostic};
use lightning::util::ser::Readable;
use serde::Serialize;

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Source {
    /// File came from a remote URL such as `https://api.blocktank.to/scorer-prod`
    /// or `https://scores.zeusln.com/latest.bin`.
    Served,
    /// File came from `Node::export_pathfinding_scores`.
    Exported,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Csv,
    Json,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Sort {
    /// Smallest `[min_offset, max_offset]` window first (highest-information entries).
    Narrow,
    /// Most recently updated first.
    Recent,
    /// Highest historical-bucket weight first (most probe-derived signal).
    History,
}

#[derive(Parser, Debug)]
#[command(
    name = "scorer-inspect",
    about = "Offline diagnostics for LDK ProbabilisticScorer / ChannelLiquidities files",
    long_about = None,
)]
struct Cli {
    /// Path to a binary scorer file.
    file: PathBuf,
    /// Metadata tag for the report. The wire format is identical for both today.
    #[arg(long, value_enum, default_value_t = Source::Served)]
    source: Source,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output: OutputFormat,
    /// Write output to a file instead of stdout.
    #[arg(long)]
    save: Option<PathBuf>,
    /// Limit per-channel rows to this number. Ignored if `--all` is set.
    #[arg(long)]
    top: Option<usize>,
    /// Dump every entry. Overrides `--top`.
    #[arg(long, default_value_t = false)]
    all: bool,
    /// Sort key for per-channel rows.
    #[arg(long, value_enum, default_value_t = Sort::Narrow)]
    sort: Sort,
}

#[derive(Debug, Serialize)]
struct Summary {
    file: String,
    source: String,
    file_size_bytes: u64,
    file_size_human: String,
    entry_count: usize,
    history_populated_count: usize,
    history_empty_count: usize,
    history_populated_pct: f64,
    min_offset_msat_p50: u64,
    min_offset_msat_p95: u64,
    min_offset_msat_max: u64,
    max_offset_msat_p50: u64,
    max_offset_msat_p95: u64,
    max_offset_msat_max: u64,
    total_valid_points_tracked_p50: f64,
    total_valid_points_tracked_p95: f64,
    total_valid_points_tracked_max: f64,
}

#[derive(Debug, Serialize)]
struct ChannelRow {
    scid: u64,
    min_liquidity_offset_msat: u64,
    max_liquidity_offset_msat: u64,
    has_history: bool,
    total_valid_points_tracked: f64,
    last_updated_secs: u64,
    offset_history_last_updated_secs: u64,
    last_datapoint_time_secs: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    summary: Summary,
    channels: Vec<ChannelRow>,
}

fn percentile_u64(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn percentile_f64(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn build_summary(
    file_path: &PathBuf, source: Source, file_size: u64,
    diags: &[ChannelLiquidityDiagnostic],
) -> Summary {
    let entry_count = diags.len();
    let history_populated_count = diags.iter().filter(|d| d.has_history).count();
    let history_empty_count = entry_count - history_populated_count;
    let history_populated_pct = if entry_count == 0 {
        0.0
    } else {
        100.0 * history_populated_count as f64 / entry_count as f64
    };

    let mut min_offsets: Vec<u64> = diags.iter().map(|d| d.min_liquidity_offset_msat).collect();
    let mut max_offsets: Vec<u64> = diags.iter().map(|d| d.max_liquidity_offset_msat).collect();
    let mut weights: Vec<f64> =
        diags.iter().map(|d| d.total_valid_points_tracked).collect();
    min_offsets.sort_unstable();
    max_offsets.sort_unstable();
    weights.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    Summary {
        file: file_path.display().to_string(),
        source: format!("{:?}", source).to_lowercase(),
        file_size_bytes: file_size,
        file_size_human: format_size(file_size, BINARY),
        entry_count,
        history_populated_count,
        history_empty_count,
        history_populated_pct,
        min_offset_msat_p50: percentile_u64(&min_offsets, 0.50),
        min_offset_msat_p95: percentile_u64(&min_offsets, 0.95),
        min_offset_msat_max: min_offsets.last().copied().unwrap_or(0),
        max_offset_msat_p50: percentile_u64(&max_offsets, 0.50),
        max_offset_msat_p95: percentile_u64(&max_offsets, 0.95),
        max_offset_msat_max: max_offsets.last().copied().unwrap_or(0),
        total_valid_points_tracked_p50: percentile_f64(&weights, 0.50),
        total_valid_points_tracked_p95: percentile_f64(&weights, 0.95),
        total_valid_points_tracked_max: weights.last().copied().unwrap_or(0.0),
    }
}

fn sort_diagnostics(diags: &mut [ChannelLiquidityDiagnostic], by: Sort) {
    match by {
        Sort::Narrow => diags.sort_by_key(|d| {
            d.max_liquidity_offset_msat.saturating_sub(d.min_liquidity_offset_msat)
        }),
        Sort::Recent => diags.sort_by(|a, b| b.last_updated_secs.cmp(&a.last_updated_secs)),
        Sort::History => diags.sort_by(|a, b| {
            b.total_valid_points_tracked
                .partial_cmp(&a.total_valid_points_tracked)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
}

fn render_text<W: Write>(writer: &mut W, report: &Report) -> Result<()> {
    let s = &report.summary;
    writeln!(writer, "scorer-inspect — offline scorer diagnostics")?;
    writeln!(writer, "  file:                   {}", s.file)?;
    writeln!(writer, "  source:                 {}", s.source)?;
    writeln!(
        writer,
        "  file size:              {} ({} bytes)",
        s.file_size_human, s.file_size_bytes
    )?;
    writeln!(writer, "  entries:                {}", s.entry_count)?;
    writeln!(
        writer,
        "  history populated:      {} ({:.1}%)",
        s.history_populated_count, s.history_populated_pct
    )?;
    writeln!(writer, "  history empty:          {}", s.history_empty_count)?;
    writeln!(writer)?;
    writeln!(
        writer,
        "  min_offset_msat:        p50={}  p95={}  max={}",
        s.min_offset_msat_p50, s.min_offset_msat_p95, s.min_offset_msat_max
    )?;
    writeln!(
        writer,
        "  max_offset_msat:        p50={}  p95={}  max={}",
        s.max_offset_msat_p50, s.max_offset_msat_p95, s.max_offset_msat_max
    )?;
    writeln!(
        writer,
        "  history bucket weight:  p50={:.0}  p95={:.0}  max={:.0}",
        s.total_valid_points_tracked_p50,
        s.total_valid_points_tracked_p95,
        s.total_valid_points_tracked_max
    )?;
    writeln!(writer)?;

    if !report.channels.is_empty() {
        writeln!(
            writer,
            "{:>20}  {:>14}  {:>14}  {:>3}  {:>16}  {:>10}",
            "scid", "min_offset", "max_offset", "his", "history_weight", "updated"
        )?;
        for row in &report.channels {
            writeln!(
                writer,
                "{:>20}  {:>14}  {:>14}  {:>3}  {:>16.0}  {:>10}",
                row.scid,
                row.min_liquidity_offset_msat,
                row.max_liquidity_offset_msat,
                if row.has_history { "yes" } else { "no" },
                row.total_valid_points_tracked,
                row.last_updated_secs,
            )?;
        }
    }
    Ok(())
}

fn render_csv<W: Write>(mut writer: W, report: &Report) -> Result<()> {
    // Section 1: summary as a single labeled row.
    {
        let mut wtr = csv::Writer::from_writer(&mut writer);
        wtr.serialize(&report.summary).context("write summary row")?;
        wtr.flush()?;
    }
    if !report.channels.is_empty() {
        // Blank line + a fresh CSV section for the channel rows so a human can scan
        // the file in Sublime Text without confusing the two sections.
        writer.write_all(b"\n")?;
        let mut wtr = csv::Writer::from_writer(&mut writer);
        for row in &report.channels {
            wtr.serialize(row).context("write channel row")?;
        }
        wtr.flush()?;
    }
    Ok(())
}

fn render_json<W: Write>(writer: W, report: &Report) -> Result<()> {
    serde_json::to_writer_pretty(writer, report).context("write json")?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let bytes = fs::read(&cli.file)
        .with_context(|| format!("read scorer file {}", cli.file.display()))?;
    let file_size = bytes.len() as u64;

    let mut cursor = Cursor::new(&bytes);
    let liquidities = ChannelLiquidities::read(&mut cursor)
        .map_err(|e| anyhow::anyhow!("decode ChannelLiquidities: {:?}", e))?;
    let mut diags = liquidities.diagnostics();

    let summary = build_summary(&cli.file, cli.source, file_size, &diags);

    sort_diagnostics(&mut diags, cli.sort);
    let limit = if cli.all { diags.len() } else { cli.top.unwrap_or(20) };
    let channels: Vec<ChannelRow> = diags
        .into_iter()
        .take(limit)
        .map(|d| ChannelRow {
            scid: d.scid,
            min_liquidity_offset_msat: d.min_liquidity_offset_msat,
            max_liquidity_offset_msat: d.max_liquidity_offset_msat,
            has_history: d.has_history,
            total_valid_points_tracked: d.total_valid_points_tracked,
            last_updated_secs: d.last_updated_secs,
            offset_history_last_updated_secs: d.offset_history_last_updated_secs,
            last_datapoint_time_secs: d.last_datapoint_time_secs,
        })
        .collect();

    let report = Report { summary, channels };

    match cli.save {
        Some(path) => {
            let f = fs::File::create(&path)
                .with_context(|| format!("create output file {}", path.display()))?;
            let mut buf = BufWriter::new(f);
            match cli.output {
                OutputFormat::Text => render_text(&mut buf, &report)?,
                OutputFormat::Csv => render_csv(&mut buf, &report)?,
                OutputFormat::Json => render_json(&mut buf, &report)?,
            }
            buf.flush()?;
            eprintln!("wrote {} ({})", path.display(), format_size(file_size, BINARY));
        }
        None => {
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            match cli.output {
                OutputFormat::Text => render_text(&mut handle, &report)?,
                OutputFormat::Csv => render_csv(&mut handle, &report)?,
                OutputFormat::Json => render_json(&mut handle, &report)?,
            }
            handle.flush()?;
        }
    }

    Ok(())
}
