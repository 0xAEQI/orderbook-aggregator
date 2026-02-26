//! Ratatui TUI client for the order book aggregator.
//!
//! Connects to the `BookSummary` gRPC stream and renders a live-updating
//! terminal display with colored depth bars, spread, and exchange attribution.
//!
//! ```bash
//! cargo run --release --bin client                                  # localhost:50051
//! cargo run --release --bin client -- http://server:50051           # custom address
//! cargo run --release --bin client -- http://localhost:50051 btcusdt # custom symbol
//! ```

use std::time::Instant;

use arrayvec::ArrayVec;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
};
use tonic::Request;

#[allow(clippy::pedantic)] // Generated code.
pub mod proto {
    include!("gen/orderbook.rs");
}

use proto::orderbook_aggregator_client::OrderbookAggregatorClient;

// ── App state ───────────────────────────────────────────────────────────────

struct App {
    summary: Option<proto::Summary>,
    updates: u64,
    start: Instant,
    symbol: String,
}

impl App {
    fn new(symbol: String) -> Self {
        Self {
            summary: None,
            updates: 0,
            start: Instant::now(),
            symbol,
        }
    }

    fn updates_per_sec(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.updates as f64 / elapsed
        } else {
            0.0
        }
    }
}

// ── Terminal guard ──────────────────────────────────────────────────────────

/// Ensures the terminal is restored even on panic.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

const MAX_DISPLAY: usize = 10;
const RED: Color = Color::Rgb(235, 87, 87);
const GREEN: Color = Color::Rgb(81, 207, 102);
const YELLOW: Color = Color::Rgb(229, 192, 72);
const DIM: Color = Color::DarkGray;
const BAR_WIDTH: usize = 25;

fn render(frame: &mut Frame, app: &App) {
    let Some(summary) = &app.summary else {
        let msg = Paragraph::new("Waiting for data...")
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Order Book ")
                    .title_style(Style::new().bold()),
            );
        frame.render_widget(msg, frame.area());
        return;
    };

    // Cumulative depth per side -- stack-allocated, no per-frame heap alloc.
    let ask_cumulative: ArrayVec<f64, MAX_DISPLAY> = {
        let mut cum: ArrayVec<f64, MAX_DISPLAY> = ArrayVec::new();
        let mut total = 0.0;
        for level in &summary.asks {
            total += level.amount;
            let _ = cum.try_push(total);
        }
        let mut reversed: ArrayVec<f64, MAX_DISPLAY> = ArrayVec::new();
        for &v in cum.iter().rev() {
            let _ = reversed.try_push(v);
        }
        reversed
    };
    let bid_cumulative: ArrayVec<f64, MAX_DISPLAY> = {
        let mut cum: ArrayVec<f64, MAX_DISPLAY> = ArrayVec::new();
        let mut total = 0.0;
        for level in &summary.bids {
            total += level.amount;
            let _ = cum.try_push(total);
        }
        cum
    };

    let max_depth = ask_cumulative
        .iter()
        .chain(bid_cumulative.iter())
        .copied()
        .fold(0.0_f64, f64::max);

    // ── Column header ────────────────────────────────────────────────────
    let header = Line::from(vec![
        Span::styled("  VENUE     ", Style::new().fg(Color::White).bold()),
        Span::styled("         QUOTE", Style::new().fg(Color::White).bold()),
        Span::styled("           QTY", Style::new().fg(Color::White).bold()),
        Span::styled("         TOTAL", Style::new().fg(Color::White).bold()),
        Span::styled("  DEPTH", Style::new().fg(Color::White).bold()),
    ]);

    // ── Ask rows (highest at top → best ask at bottom) ──────────────────
    let mut ask_lines: Vec<Line<'_>> = Vec::with_capacity(summary.asks.len() + 2);
    ask_lines.push(Line::from(Span::styled(
        "  ASKS",
        Style::new().fg(RED).bold(),
    )));
    ask_lines.push(header.clone());
    // asks come sorted best-first from the server; reverse so highest is at top
    for (i, level) in summary.asks.iter().rev().enumerate() {
        let cum = ask_cumulative.get(i).copied().unwrap_or(0.0);
        ask_lines.push(format_level(level, RED, cum, max_depth));
    }

    // ── Spread ──────────────────────────────────────────────────────────
    let best_bid = summary.bids.first().map_or(0.0, |l| l.price);
    let best_ask = summary.asks.first().map_or(0.0, |l| l.price);
    let mid = f64::midpoint(best_bid, best_ask);
    let spread_abs = summary.spread;
    let spread_pct = if mid > 0.0 {
        (spread_abs / mid) * 100.0
    } else {
        0.0
    };
    let spread_text = format!(" Spread: {spread_abs:.8} ({spread_pct:.3}%) ");
    let pad_total = 70_usize.saturating_sub(spread_text.len());
    let pad = "─".repeat(pad_total / 2);
    let spread_line = Line::from(Span::styled(
        format!("  {pad}{spread_text}{pad}"),
        Style::new().fg(YELLOW),
    ));

    // ── Bid rows (best bid at top → lowest at bottom) ───────────────────
    let mut bid_lines: Vec<Line<'_>> = Vec::with_capacity(summary.bids.len() + 2);
    for (i, level) in summary.bids.iter().enumerate() {
        let cum = bid_cumulative.get(i).copied().unwrap_or(0.0);
        bid_lines.push(format_level(level, GREEN, cum, max_depth));
    }
    bid_lines.push(header);
    bid_lines.push(Line::from(Span::styled(
        "  BIDS",
        Style::new().fg(GREEN).bold(),
    )));

    // ── Layout: asks | spread | bids ────────────────────────────────────
    let status = format!(" Updates: {:.0}/s  │  q to quit ", app.updates_per_sec());

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Order Book ─ {} ", app.symbol))
        .title_style(Style::new().bold())
        .title_bottom(Line::from(status).alignment(Alignment::Center))
        .padding(Padding::new(1, 1, 1, 0));

    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let chunks = Layout::vertical([
        Constraint::Length(ask_lines.len() as u16),
        Constraint::Length(1), // spread
        Constraint::Length(bid_lines.len() as u16),
        Constraint::Min(0), // absorb remaining space
    ])
    .split(inner);

    frame.render_widget(Paragraph::new(ask_lines), chunks[0]);
    frame.render_widget(
        Paragraph::new(spread_line).alignment(Alignment::Left),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(bid_lines), chunks[2]);
}

fn format_level<'a>(
    level: &proto::Level,
    color: Color,
    cumulative: f64,
    max_depth: f64,
) -> Line<'a> {
    let bar_len = if max_depth > 0.0 {
        #[allow(clippy::cast_sign_loss)]
        {
            ((cumulative / max_depth) * BAR_WIDTH as f64).round() as usize
        }
    } else {
        0
    };
    let bar = "█".repeat(bar_len);

    Line::from(vec![
        Span::styled(format!("  {:<10}", level.exchange), Style::new().fg(DIM)),
        Span::styled(format!("{:>14.8}", level.price), Style::new().fg(color)),
        Span::styled(format!("{:>14.8}", level.amount), Style::new().fg(color)),
        Span::styled(format!("{cumulative:>14.8}"), Style::new().fg(DIM)),
        Span::raw("  "),
        Span::styled(bar, Style::new().fg(color)),
    ])
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:50051".to_string());
    let symbol = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "ETHBTC".to_string())
        .to_uppercase();

    let mut client = OrderbookAggregatorClient::connect(addr).await?;
    let mut stream = client
        .book_summary(Request::new(proto::Empty {}))
        .await?
        .into_inner();

    // Init terminal. TermGuard must be constructed before any fallible calls
    // so Drop restores the terminal even if clear() fails.
    let mut terminal = ratatui::init();
    let _guard = TermGuard;
    terminal.clear()?;

    let mut app = App::new(symbol);

    loop {
        // Wait for next gRPC message or tick for counter refresh.
        tokio::select! {
            msg = stream.message() => {
                match msg {
                    Ok(Some(summary)) => {
                        app.summary = Some(summary);
                        app.updates += 1;
                    }
                    Ok(None) => break, // stream ended
                    Err(e) => {
                        // _guard's Drop restores the terminal on return.
                        return Err(e.into());
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        // Render after state change (or 1s tick for counter refresh).
        terminal.draw(|frame| render(frame, &app))?;

        // Check for quit keys (non-blocking).
        if event::poll(std::time::Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break;
                }
                _ => {}
            }
        }
    }

    Ok(())
}
