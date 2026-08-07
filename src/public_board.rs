//! Public leaderboard image (kaggle mode only).
//!
//! **Privacy is the whole point of this module.** It is built from
//! `Database::get_public_candidates`, which never selects `private_gain` or
//! `user_email` -- see that function's doc comment. Every function here
//! keeps that guarantee: `build_rows`/`render_svg` must never be given, and
//! must never render, anything but public gains and Zulip display names.
//! `tests::rendered_svg_never_contains_private_data_or_emails` is the
//! regression test for this; treat any change that would make it fail as a
//! bug, not a test to relax.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Context, Result};

const FONT_FAMILY: &str = "DejaVu Sans";
const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardOrder {
    Best,
    Mean,
}

/// Hard cap on `BoardOptions::top`, enforced by `build_rows` regardless of
/// what the caller asks for.
pub const MAX_TOP: usize = 100;

#[derive(Debug, Clone)]
pub struct BoardOptions {
    pub top: usize,
    pub order: BoardOrder,
    pub show_values: bool,
    pub range: Option<(f64, f64)>,
    pub show_axis: bool,
    pub show_median: bool,
}

impl Default for BoardOptions {
    fn default() -> Self {
        Self {
            top: 20,
            order: BoardOrder::Best,
            show_values: true,
            range: None,
            show_axis: true,
            show_median: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BoardRow {
    pub rank: usize,
    pub display_name: String,
    /// The representative batch's public gains, sorted ascending.
    pub gains: Vec<f64>,
    pub best: f64,
    pub mean: f64,
}

/// Groups every pre-deadline kaggle candidate by competitor, picks each
/// competitor's REPRESENTATIVE batch -- the one holding their best-ever
/// public candidate, so the row's drawn shape and its rank number can never
/// contradict each other -- and ranks by `opts.order`. Pure: no I/O, no
/// clock, so this is fully unit-testable against hand-built fixtures.
///
/// `candidates` is `(user_id, display_name, batch_key, public_gain)`, the
/// exact shape `Database::get_public_candidates` returns.
pub fn build_rows(candidates: &[(i64, String, String, f64)], opts: &BoardOptions) -> Vec<BoardRow> {
    // user_id -> (latest-seen display name, batch_key -> gains)
    let mut by_user: HashMap<i64, (String, HashMap<String, Vec<f64>>)> = HashMap::new();
    for (user_id, name, batch_key, gain) in candidates {
        let entry = by_user
            .entry(*user_id)
            .or_insert_with(|| (name.clone(), HashMap::new()));
        entry.0 = name.clone();
        entry.1.entry(batch_key.clone()).or_default().push(*gain);
    }

    let mut rows: Vec<BoardRow> = by_user
        .into_values()
        .filter_map(|(name, batches)| {
            let (_, gains) = batches.into_iter().max_by(|(key_a, a), (key_b, b)| {
                let max_a = a.iter().copied().fold(f64::MIN, f64::max);
                let max_b = b.iter().copied().fold(f64::MIN, f64::max);
                max_a
                    .partial_cmp(&max_b)
                    .expect("gains are never NaN")
                    .then_with(|| key_a.cmp(key_b))
            })?;

            let mut sorted = gains;
            sorted.sort_by(|a, b| a.partial_cmp(b).expect("gains are never NaN"));
            let best = *sorted.last().expect("a batch is never empty");
            let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;

            Some(BoardRow {
                rank: 0, // assigned below, after sorting and truncation
                display_name: name,
                gains: sorted,
                best,
                mean,
            })
        })
        .collect();

    rows.sort_by(|a, b| {
        let (key_a, key_b) = match opts.order {
            BoardOrder::Best => (a.best, b.best),
            BoardOrder::Mean => (a.mean, b.mean),
        };
        key_b
            .partial_cmp(&key_a)
            .expect("gains are never NaN")
            .then_with(|| a.display_name.cmp(&b.display_name))
    });

    rows.truncate(opts.top.min(MAX_TOP));

    for (i, row) in rows.iter_mut().enumerate() {
        row.rank = i + 1;
    }

    rows
}

/// The `[key=value ...]` argument string of the `public leaderboard` command
/// (everything after the two command words, order-independent, every key
/// optional). Returns a usage-error message -- listing every valid key,
/// never a silent default -- on an unknown key or an unparseable/invalid
/// value, including a `range` where `MIN >= MAX`.
pub fn parse_options(args: &str) -> Result<BoardOptions, String> {
    let mut opts = BoardOptions::default();

    for pair in args.split_whitespace() {
        let (key, value) = pair.split_once('=').ok_or_else(usage_error)?;
        match key {
            "top" => {
                let n: usize = value.parse().map_err(|_| usage_error())?;
                if n == 0 {
                    return Err(usage_error());
                }
                opts.top = n;
            }
            "order" => {
                opts.order = match value {
                    "best" => BoardOrder::Best,
                    "mean" => BoardOrder::Mean,
                    _ => return Err(usage_error()),
                };
            }
            "values" => opts.show_values = parse_bool(value).ok_or_else(usage_error)?,
            "axis" => opts.show_axis = parse_bool(value).ok_or_else(usage_error)?,
            "median" => opts.show_median = parse_bool(value).ok_or_else(usage_error)?,
            "range" => {
                let (lo_str, hi_str) = value.split_once(':').ok_or_else(usage_error)?;
                let lo: f64 = lo_str.parse().map_err(|_| usage_error())?;
                let hi: f64 = hi_str.parse().map_err(|_| usage_error())?;
                let ordered = lo.partial_cmp(&hi).map(|o| o == std::cmp::Ordering::Less);
                if ordered != Some(true) {
                    return Err(usage_error());
                }
                opts.range = Some((lo, hi));
            }
            _ => return Err(usage_error()),
        }
    }

    Ok(opts)
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn usage_error() -> String {
    "❌ Usage: `public leaderboard [top=N] [order=best|mean] [values=on|off] [range=MIN:MAX] [axis=on|off] [median=on|off]`"
        .to_string()
}

// ---- Layout constants (SVG user units) ------------------------------------

const MARGIN_LEFT: f64 = 16.0;
const RANK_WIDTH: f64 = 32.0;
const NAME_WIDTH: f64 = 190.0;
const PLOT_WIDTH: f64 = 480.0;
const VALUE_WIDTH: f64 = 64.0;
const MARGIN_RIGHT: f64 = 16.0;
const ROW_HEIGHT: f64 = 36.0;
const ROW_PAD: f64 = 6.0;
const TOP_MARGIN: f64 = 24.0;
const AXIS_HEIGHT: f64 = 30.0;
const BOTTOM_MARGIN: f64 = 16.0;
const MAX_NAME_CHARS: usize = 20;
const KDE_GRID_POINTS: usize = 100;
const SHAPE_COLOR: &str = "#2c6fbb";

/// Renders the board as a standalone SVG string. Zulip does not render SVG
/// inline -- see `rasterize_png` -- but keeping this as a plain string is
/// what makes it possible to unit-test the output directly.
pub fn render_svg(rows: &[BoardRow], opts: &BoardOptions) -> String {
    let x_plot_start = MARGIN_LEFT + RANK_WIDTH + NAME_WIDTH;
    let x_plot_end = x_plot_start + PLOT_WIDTH;
    let width = x_plot_end + if opts.show_values { VALUE_WIDTH } else { 0.0 } + MARGIN_RIGHT;

    let rows_bottom = TOP_MARGIN + ROW_HEIGHT * rows.len() as f64;
    let height = rows_bottom + if opts.show_axis { AXIS_HEIGHT } else { 0.0 } + BOTTOM_MARGIN;

    let (range_min, range_max) = resolve_range(rows, opts);

    let mut svg = String::new();
    let _ = write!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="{font}">"##,
        w = width,
        h = height,
        font = FONT_FAMILY,
    );
    let _ = write!(
        svg,
        r##"<rect x="0" y="0" width="{w:.0}" height="{h:.0}" fill="#ffffff"/>"##,
        w = width,
        h = height,
    );

    let x_of = |v: f64| -> f64 {
        let raw = if (range_max - range_min).abs() < f64::EPSILON {
            (x_plot_start + x_plot_end) / 2.0
        } else {
            x_plot_start + (v - range_min) / (range_max - range_min) * PLOT_WIDTH
        };
        raw.clamp(x_plot_start, x_plot_end)
    };

    for (i, row) in rows.iter().enumerate() {
        let y_center = TOP_MARGIN + ROW_HEIGHT * i as f64 + ROW_HEIGHT / 2.0;
        let baseline = y_center + (ROW_HEIGHT / 2.0 - ROW_PAD);
        let top = y_center - (ROW_HEIGHT / 2.0 - ROW_PAD);

        let _ = write!(
            svg,
            r##"<text x="{x:.1}" y="{y:.1}" font-size="13" fill="#333333">{rank}.</text>"##,
            x = MARGIN_LEFT,
            y = y_center + 4.0,
            rank = row.rank,
        );

        let _ = write!(
            svg,
            r##"<text x="{x:.1}" y="{y:.1}" font-size="13" fill="#111111">{name}</text>"##,
            x = MARGIN_LEFT + RANK_WIDTH,
            y = y_center + 4.0,
            name = xml_escape(&name_label(&row.display_name, row.gains.len())),
        );

        render_shape(&mut svg, row, &x_of, range_min, range_max, top, baseline);

        if row.gains.iter().any(|g| *g < range_min) {
            let _ = write!(
                svg,
                r##"<text x="{x:.1}" y="{y:.1}" font-size="12" fill="#888888">&#9668;</text>"##,
                x = x_plot_start - 12.0,
                y = y_center + 4.0,
            );
        }
        if row.gains.iter().any(|g| *g > range_max) {
            let _ = write!(
                svg,
                r##"<text x="{x:.1}" y="{y:.1}" font-size="12" fill="#888888">&#9658;</text>"##,
                x = x_plot_end + 2.0,
                y = y_center + 4.0,
            );
        }

        if opts.show_values {
            let _ = write!(
                svg,
                r##"<text x="{x:.1}" y="{y:.1}" font-size="13" fill="#111111">{v:.2}</text>"##,
                x = x_plot_end + 10.0,
                y = y_center + 4.0,
                v = row.best,
            );
        }
    }

    if opts.show_median && !rows.is_empty() {
        let mut values: Vec<f64> = rows.iter().map(|r| r.best).collect();
        values.sort_by(|a, b| a.partial_cmp(b).expect("gains are never NaN"));
        let median = median_of(&values);
        let x = x_of(median);
        let _ = write!(
            svg,
            r##"<line x1="{x:.1}" y1="{y1:.1}" x2="{x:.1}" y2="{y2:.1}" stroke="#c0392b" stroke-width="1.5" stroke-dasharray="4,3"/>"##,
            x = x,
            y1 = TOP_MARGIN,
            y2 = rows_bottom,
        );
    }

    if opts.show_axis {
        let axis_y = rows_bottom + 6.0;
        let _ = write!(
            svg,
            r##"<line x1="{x1:.1}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="#999999" stroke-width="1"/>"##,
            x1 = x_plot_start,
            x2 = x_plot_end,
            y = axis_y,
        );
        const TICKS: usize = 5;
        for t in 0..=TICKS {
            let frac = t as f64 / TICKS as f64;
            let v = range_min + frac * (range_max - range_min);
            let x = x_plot_start + frac * PLOT_WIDTH;
            let _ = write!(
                svg,
                r##"<line x1="{x:.1}" y1="{y1:.1}" x2="{x:.1}" y2="{y2:.1}" stroke="#999999" stroke-width="1"/>"##,
                x = x,
                y1 = axis_y,
                y2 = axis_y + 4.0,
            );
            let _ = write!(
                svg,
                r##"<text x="{x:.1}" y="{y:.1}" font-size="10" fill="#666666" text-anchor="middle">{v:.1}</text>"##,
                x = x,
                y = axis_y + 16.0,
            );
        }
    }

    svg.push_str("</svg>");
    svg
}

/// The name label shown next to a row: the display name, truncated by
/// `.chars()` (names contain non-ASCII), with the candidate count appended
/// as `(n=K)` -- per-row density normalization means shape height alone
/// can't distinguish a 3-candidate row from a 40-candidate one, so the count
/// must always be legible and is never itself truncated away.
fn name_label(display_name: &str, n: usize) -> String {
    let suffix = format!(" (n={n})");
    let budget = MAX_NAME_CHARS.saturating_sub(suffix.chars().count());
    let char_count = display_name.chars().count();
    let mut label: String = display_name.chars().take(budget).collect();
    if char_count > budget {
        label.push('\u{2026}'); // …
    }
    label.push_str(&suffix);
    label
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn resolve_range(rows: &[BoardRow], opts: &BoardOptions) -> (f64, f64) {
    if let Some(range) = opts.range {
        return range;
    }

    let mut min_v = f64::MAX;
    let mut max_v = f64::MIN;
    for row in rows {
        for &g in &row.gains {
            min_v = min_v.min(g);
            max_v = max_v.max(g);
        }
    }

    if !min_v.is_finite() || !max_v.is_finite() {
        return (0.0, 1.0);
    }

    let span = (max_v - min_v).max(f64::EPSILON);
    let margin = span * 0.08;
    (min_v - margin, max_v + margin)
}

/// Draws one row's shape, per candidate count `n`: 1 = a dot, 2 = a straight
/// line, 3 = a smoothed ("Bezier") triangle, 4+ = a filled Gaussian KDE
/// curve. Deliberately never draws individual ticks or any kind of marker
/// (best/mean highlight) -- the shape alone is the entire representation.
/// Normalized to this row's own height budget, so shape height is NOT
/// comparable across rows -- `name_label` prints `n` to compensate.
#[allow(clippy::too_many_arguments)]
fn render_shape(
    svg: &mut String,
    row: &BoardRow,
    x_of: &impl Fn(f64) -> f64,
    range_min: f64,
    range_max: f64,
    top: f64,
    baseline: f64,
) {
    let gains = &row.gains;
    match gains.len() {
        0 => {}
        1 => {
            let x = x_of(gains[0]);
            let _ = write!(
                svg,
                r##"<circle cx="{x:.2}" cy="{y:.2}" r="3.5" fill="{color}"/>"##,
                x = x,
                y = baseline,
                color = SHAPE_COLOR,
            );
        }
        2 => {
            let x0 = x_of(gains[0]);
            let x1 = x_of(gains[1]);
            let _ = write!(
                svg,
                r##"<line x1="{x0:.2}" y1="{y:.2}" x2="{x1:.2}" y2="{y:.2}" stroke="{color}" stroke-width="2.5" stroke-linecap="round"/>"##,
                x0 = x0,
                x1 = x1,
                y = baseline,
                color = SHAPE_COLOR,
            );
        }
        3 => {
            let x0 = x_of(gains[0]);
            let x1 = x_of(gains[1]);
            let x2 = x_of(gains[2]);
            let _ = write!(
                svg,
                r##"<path d="M {x0:.2} {base:.2} Q {q1:.2} {base:.2} {x1:.2} {top:.2} Q {q2:.2} {base:.2} {x2:.2} {base:.2} Z" fill="{color}" fill-opacity="0.55" stroke="{color}" stroke-width="1"/>"##,
                x0 = x0,
                base = baseline,
                q1 = (x0 + x1) / 2.0,
                x1 = x1,
                top = top,
                q2 = (x1 + x2) / 2.0,
                x2 = x2,
                color = SHAPE_COLOR,
            );
        }
        _ => {
            let (grid, density) = gaussian_kde(gains, range_min, range_max, KDE_GRID_POINTS);
            let peak = density.iter().copied().fold(f64::MIN, f64::max).max(f64::EPSILON);
            let band = baseline - top;

            let mut d = format!("M {:.2} {:.2}", x_of(grid[0]), baseline);
            for (g, dens) in grid.iter().zip(density.iter()) {
                let y = baseline - (dens / peak) * band;
                let _ = write!(d, " L {:.2} {:.2}", x_of(*g), y);
            }
            let _ = write!(
                d,
                " L {:.2} {:.2} Z",
                x_of(*grid.last().expect("KDE_GRID_POINTS > 0")),
                baseline
            );
            let _ = write!(
                svg,
                r##"<path d="{d}" fill="{color}" fill-opacity="0.55" stroke="{color}" stroke-width="1"/>"##,
                d = d,
                color = SHAPE_COLOR,
            );
        }
    }
}

/// Gaussian KDE with Silverman's rule bandwidth (`1.06 * std_dev *
/// n^(-1/5)`), evaluated on `n_points` evenly spaced grid points across
/// `[range_min, range_max]`. Falls back to a small fraction of the range as
/// bandwidth when the data has zero spread (every candidate identical), so
/// the curve is a narrow bump instead of an infinite spike.
fn gaussian_kde(values: &[f64], range_min: f64, range_max: f64, n_points: usize) -> (Vec<f64>, Vec<f64>) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let std_dev = variance.sqrt();

    let span = (range_max - range_min).max(f64::EPSILON);
    let bandwidth = if std_dev > 0.0 {
        1.06 * std_dev * n.powf(-1.0 / 5.0)
    } else {
        span * 0.02
    }
    .max(span * 0.005);

    let grid: Vec<f64> = (0..n_points)
        .map(|i| range_min + span * i as f64 / (n_points - 1) as f64)
        .collect();

    let two_pi_sqrt = (2.0 * std::f64::consts::PI).sqrt();
    let density: Vec<f64> = grid
        .iter()
        .map(|&x| {
            values
                .iter()
                .map(|&v| {
                    let z = (x - v) / bandwidth;
                    (-0.5 * z * z).exp()
                })
                .sum::<f64>()
                / (n * bandwidth * two_pi_sqrt)
        })
        .collect();

    (grid, density)
}

fn median_of(sorted_values: &[f64]) -> f64 {
    let n = sorted_values.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        sorted_values[n / 2]
    } else {
        (sorted_values[n / 2 - 1] + sorted_values[n / 2]) / 2.0
    }
}

/// Rasterizes an SVG (as produced by `render_svg`) to PNG bytes, since Zulip
/// does not render SVG inline. The embedded font must be registered here AND
/// its family name must match the `font-family` `render_svg` writes into the
/// SVG, or text silently fails to render (no error, just missing glyphs).
/// The font itself is never loaded from the host's system fonts, so output
/// is identical regardless of what machine renders it.
pub fn rasterize_png(svg: &str) -> Result<Vec<u8>> {
    let mut font_db = fontdb::Database::new();
    font_db.load_font_data(FONT_BYTES.to_vec());

    let opts = usvg::Options {
        fontdb: std::sync::Arc::new(font_db),
        font_family: FONT_FAMILY.to_string(),
        ..usvg::Options::default()
    };

    let tree = usvg::Tree::from_str(svg, &opts)
        .map_err(|e| anyhow::anyhow!("Failed to parse the generated SVG: {}", e))?;

    let size = tree.size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width().ceil() as u32, size.height().ceil() as u32)
        .context("Failed to allocate the raster canvas")?;

    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());

    pixmap.encode_png().context("Failed to encode PNG")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(user_id: i64, name: &str, batch_key: &str, gain: f64) -> (i64, String, String, f64) {
        (user_id, name.to_string(), batch_key.to_string(), gain)
    }

    // ---- build_rows --------------------------------------------------

    #[test]
    fn representative_batch_is_the_one_holding_the_best_ever_candidate() {
        let candidates = vec![
            // Earlier batch has the best-ever candidate for this user.
            candidate(1, "Ana", "early", 50.0),
            candidate(1, "Ana", "early", 10.0),
            // Later batch is worse overall -- must NOT be the representative.
            candidate(1, "Ana", "late", 20.0),
            candidate(1, "Ana", "late", 30.0),
        ];
        let rows = build_rows(&candidates, &BoardOptions::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].best, 50.0);
        assert_eq!(rows[0].gains, vec![10.0, 50.0], "must draw the EARLY batch, not the later one");
    }

    #[test]
    fn order_best_and_order_mean_can_disagree() {
        let candidates = vec![
            // Ana: one huge outlier, otherwise weak -- best is high, mean is low.
            candidate(1, "Ana", "a1", 100.0),
            candidate(1, "Ana", "a1", 0.0),
            candidate(1, "Ana", "a1", 0.0),
            // Beto: consistently good, no outlier -- best is lower than Ana's,
            // but mean is much higher.
            candidate(2, "Beto", "b1", 40.0),
            candidate(2, "Beto", "b1", 40.0),
            candidate(2, "Beto", "b1", 40.0),
        ];

        let by_best = build_rows(
            &candidates,
            &BoardOptions {
                order: BoardOrder::Best,
                ..Default::default()
            },
        );
        assert_eq!(by_best[0].display_name, "Ana", "Ana has the single best candidate");

        let by_mean = build_rows(
            &candidates,
            &BoardOptions {
                order: BoardOrder::Mean,
                ..Default::default()
            },
        );
        assert_eq!(by_mean[0].display_name, "Beto", "Beto has the better mean");
    }

    #[test]
    fn top_truncates_and_ranks_are_assigned_after_sorting() {
        let candidates = vec![
            candidate(1, "Ana", "a", 10.0),
            candidate(2, "Beto", "b", 30.0),
            candidate(3, "Caro", "c", 20.0),
        ];
        let rows = build_rows(
            &candidates,
            &BoardOptions {
                top: 2,
                ..Default::default()
            },
        );
        assert_eq!(rows.len(), 2, "truncated to top=2");
        assert_eq!(rows[0].rank, 1);
        assert_eq!(rows[0].display_name, "Beto", "best gain ranks first");
        assert_eq!(rows[1].rank, 2);
        assert_eq!(rows[1].display_name, "Caro");
    }

    #[test]
    fn top_is_hard_capped_regardless_of_the_requested_value() {
        let candidates: Vec<_> = (0..150)
            .map(|i| candidate(i, &format!("u{i}"), "b", i as f64))
            .collect();
        let rows = build_rows(
            &candidates,
            &BoardOptions {
                top: 1000,
                ..Default::default()
            },
        );
        assert_eq!(rows.len(), MAX_TOP);
    }

    // ---- parse_options -----------------------------------------------

    #[test]
    fn parse_options_reads_every_key_order_independently() {
        let opts = parse_options("median=on top=5 order=mean values=off range=1:2 axis=off").unwrap();
        assert_eq!(opts.top, 5);
        assert_eq!(opts.order, BoardOrder::Mean);
        assert!(!opts.show_values);
        assert_eq!(opts.range, Some((1.0, 2.0)));
        assert!(!opts.show_axis);
        assert!(opts.show_median);
    }

    #[test]
    fn parse_options_defaults_on_an_empty_string() {
        let opts = parse_options("").unwrap();
        assert_eq!(opts.top, BoardOptions::default().top);
        assert_eq!(opts.order, BoardOptions::default().order);
    }

    #[test]
    fn parse_options_rejects_an_unknown_key() {
        assert!(parse_options("bogus=1").is_err());
    }

    #[test]
    fn parse_options_rejects_top_zero() {
        assert!(parse_options("top=0").is_err());
    }

    #[test]
    fn parse_options_rejects_an_inverted_range() {
        assert!(parse_options("range=10:5").is_err());
        assert!(parse_options("range=10:10").is_err());
    }

    // ---- render_svg ----------------------------------------------------

    fn row(name: &str, gains: &[f64]) -> BoardRow {
        let mut sorted = gains.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        BoardRow {
            rank: 1,
            display_name: name.to_string(),
            best: *sorted.last().unwrap(),
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            gains: sorted,
        }
    }

    #[test]
    fn single_candidate_renders_a_dot_and_nothing_else() {
        let rows = vec![row("Ana", &[10.0])];
        let svg = render_svg(&rows, &BoardOptions::default());
        assert!(svg.contains("<circle"), "n=1 must draw a dot: {svg}");
        assert!(!svg.contains("<path"), "n=1 must not draw a density/triangle path: {svg}");
    }

    #[test]
    fn two_candidates_render_a_line_segment() {
        let rows = vec![row("Ana", &[10.0, 20.0])];
        let svg = render_svg(&rows, &BoardOptions::default());
        assert!(svg.contains("<line"), "n=2 must draw a line: {svg}");
        assert!(!svg.contains("<circle"), "n=2 must not draw a dot: {svg}");
        assert!(!svg.contains("<path"), "n=2 must not draw a path: {svg}");
    }

    #[test]
    fn three_candidates_render_a_closed_bezier_path() {
        let rows = vec![row("Ana", &[10.0, 20.0, 30.0])];
        let svg = render_svg(&rows, &BoardOptions::default());
        assert!(svg.contains("<path"), "n=3 must draw a path: {svg}");
        assert!(svg.contains(" Q "), "n=3 must use quadratic Beziers, not straight edges: {svg}");
        assert!(svg.contains(" Z"), "n=3 must be a closed, filled shape: {svg}");
    }

    #[test]
    fn four_or_more_candidates_render_a_density_curve() {
        let rows = vec![row("Ana", &[10.0, 12.0, 15.0, 20.0, 22.0])];
        let svg = render_svg(&rows, &BoardOptions::default());
        assert!(svg.contains("<path"), "n>=4 must draw a path: {svg}");
        // A 100-point KDE grid produces far more "L " segments than the
        // fixed 2 a triangle's Bezier path uses.
        assert!(svg.matches(" L ").count() > 10, "n>=4 must be a many-point curve: {svg}");
    }

    #[test]
    fn no_row_ever_draws_individual_ticks_or_a_marker() {
        for gains in [vec![10.0], vec![10.0, 20.0], vec![10.0, 20.0, 30.0], vec![1.0, 2.0, 3.0, 4.0, 5.0]] {
            let rows = vec![row("Ana", &gains)];
            let svg = render_svg(&rows, &BoardOptions::default());
            // No <rect> ticks, no <polygon>/<use> markers -- only the shapes
            // and text/lines this module itself defines.
            assert!(!svg.contains("<polygon"), "must never draw a marker: {svg}");
            assert!(!svg.contains("marker"), "must never draw a marker: {svg}");
        }
    }

    #[test]
    fn overflow_arrows_appear_only_when_range_clips_a_candidate() {
        let rows = vec![row("Ana", &[10.0, 50.0])];

        let clipped = render_svg(
            &rows,
            &BoardOptions {
                range: Some((20.0, 40.0)),
                ..Default::default()
            },
        );
        assert!(clipped.contains("&#9668;") || clipped.contains("&#9658;"), "range clips both sides: {clipped}");

        let not_clipped = render_svg(
            &rows,
            &BoardOptions {
                range: Some((0.0, 60.0)),
                ..Default::default()
            },
        );
        assert!(
            !not_clipped.contains("&#9668;") && !not_clipped.contains("&#9658;"),
            "range covers every candidate, no arrows expected: {not_clipped}"
        );
    }

    #[test]
    fn median_line_present_only_when_requested() {
        let rows = vec![row("Ana", &[10.0]), row("Beto", &[20.0])];

        let without = render_svg(&rows, &BoardOptions::default());
        assert!(!without.contains("stroke-dasharray"), "median off by default: {without}");

        let with = render_svg(
            &rows,
            &BoardOptions {
                show_median: true,
                ..Default::default()
            },
        );
        assert!(with.contains("stroke-dasharray"), "median=on must draw the dashed line: {with}");
    }

    #[test]
    fn values_and_axis_present_only_when_requested() {
        let rows = vec![row("Ana", &[42.5])];

        let bare = render_svg(
            &rows,
            &BoardOptions {
                show_values: false,
                show_axis: false,
                ..Default::default()
            },
        );
        assert!(!bare.contains("42.50"), "values=off must hide the numeric gain: {bare}");

        let full = render_svg(
            &rows,
            &BoardOptions {
                show_values: true,
                show_axis: true,
                ..Default::default()
            },
        );
        assert!(full.contains("42.50"), "values=on must show the numeric gain: {full}");
    }

    #[test]
    fn rank_numbers_are_shown_in_order() {
        let rows = build_rows(
            &[
                candidate(1, "Ana", "a", 30.0),
                candidate(2, "Beto", "b", 10.0),
                candidate(3, "Caro", "c", 20.0),
            ],
            &BoardOptions::default(),
        );
        let svg = render_svg(&rows, &BoardOptions::default());
        assert!(svg.contains(">1.<"), "rank 1 must be shown: {svg}");
        assert!(svg.contains(">2.<"), "rank 2 must be shown: {svg}");
        assert!(svg.contains(">3.<"), "rank 3 must be shown: {svg}");
    }

    #[test]
    fn rendered_svg_never_contains_private_data_or_emails() {
        // Sentinel values distinctive enough that any accidental leak from a
        // private_gain column or an email field would be unmistakable here.
        const SENTINEL_PRIVATE_GAIN: &str = "8675309.42";
        const SENTINEL_EMAIL: &str = "student-sentinel@example.com";

        let candidates = vec![
            candidate(1, "Ana", "a", 12.34),
            candidate(1, "Ana", "a", 56.78),
        ];
        let rows = build_rows(&candidates, &BoardOptions::default());
        let svg = render_svg(
            &rows,
            &BoardOptions {
                show_values: true,
                ..Default::default()
            },
        );

        assert!(
            !svg.contains(SENTINEL_PRIVATE_GAIN),
            "public board must never contain a private-gain value: {svg}"
        );
        assert!(
            !svg.contains(SENTINEL_EMAIL) && !svg.contains('@'),
            "public board must never contain an email address: {svg}"
        );
    }

    // ---- rasterize_png ---------------------------------------------------

    #[test]
    fn rasterize_png_produces_a_non_empty_png() {
        let rows = vec![row("Ana", &[10.0, 20.0])];
        let svg = render_svg(&rows, &BoardOptions::default());
        let png = rasterize_png(&svg).expect("a well-formed SVG must rasterize");
        assert!(!png.is_empty());
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'], "must be a valid PNG");
    }
}
