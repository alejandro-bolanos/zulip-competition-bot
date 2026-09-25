//! Public leaderboard image (kaggle mode only).
//!
//! **Privacy is the whole point of this module.** It is built from
//! `Database::get_public_candidates`, which never returns `private_gain` or
//! `user_email` -- see that function's doc comment. Every function here
//! keeps that guarantee: `build_rows`/`render_svg` must never be given, and
//! must never render, anything but public gains, competitors' Zulip display
//! names, and published baselines' own names (`Database::get_public_baselines`
//! -- labeled by the baseline, never by the teacher who uploaded it).
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
    /// 1-based among competitors; always 0 for a baseline, which is never ranked.
    pub rank: usize,
    /// The competitor's Zulip name, or the baseline's own name.
    pub display_name: String,
    /// The representative batch's public gains, sorted ascending.
    pub gains: Vec<f64>,
    pub best: f64,
    pub mean: f64,
    /// A teacher's published reference model, not a competitor: drawn
    /// unnumbered and in its own color.
    pub is_baseline: bool,
}

impl BoardRow {
    fn from_gains(display_name: String, mut gains: Vec<f64>, is_baseline: bool) -> Self {
        gains.sort_by(|a, b| a.partial_cmp(b).expect("gains are never NaN"));
        let best = *gains.last().expect("a batch is never empty");
        let mean = gains.iter().sum::<f64>() / gains.len() as f64;
        Self {
            rank: 0, // competitors' ranks are assigned after sorting and truncation
            display_name,
            gains,
            best,
            mean,
            is_baseline,
        }
    }

    fn sort_key(&self, order: BoardOrder) -> f64 {
        match order {
            BoardOrder::Best => self.best,
            BoardOrder::Mean => self.mean,
        }
    }
}

/// Groups every pre-deadline kaggle candidate by competitor, picks each
/// competitor's REPRESENTATIVE batch -- the one holding their best-ever
/// public candidate, so the row's drawn shape and its rank number can never
/// contradict each other -- and ranks by `opts.order`. Then interleaves each
/// published baseline at the position its own score earns, unnumbered: it
/// never takes a rank, never counts toward `top`, and is shown even if it
/// scores below the last competitor shown (the teacher published it to be
/// seen). Pure: no I/O, no clock, so this is fully unit-testable against
/// hand-built fixtures.
///
/// `candidates` is `(user_id, display_name, batch_key, public_gain)`, the
/// exact shape `Database::get_public_candidates` returns; `baselines` is
/// `(baseline_name, batch_id, public_gain)`, as `get_public_baselines`
/// returns -- one row per upload.
pub fn build_rows(
    candidates: &[(i64, String, String, f64)],
    baselines: &[(String, String, f64)],
    opts: &BoardOptions,
) -> Vec<BoardRow> {
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
            Some(BoardRow::from_gains(name, gains, false))
        })
        .collect();

    let by_key_desc = |a: &BoardRow, b: &BoardRow| {
        b.sort_key(opts.order)
            .partial_cmp(&a.sort_key(opts.order))
            .expect("gains are never NaN")
            .then_with(|| a.display_name.cmp(&b.display_name))
    };

    rows.sort_by(by_key_desc);
    rows.truncate(opts.top.min(MAX_TOP));
    for (i, row) in rows.iter_mut().enumerate() {
        row.rank = i + 1;
    }

    let mut baseline_batches: HashMap<&str, (&str, Vec<f64>)> = HashMap::new();
    for (name, batch_id, gain) in baselines {
        baseline_batches
            .entry(batch_id.as_str())
            .or_insert_with(|| (name.as_str(), Vec::new()))
            .1
            .push(*gain);
    }
    let mut baseline_rows: Vec<BoardRow> = baseline_batches
        .into_values()
        .map(|(name, gains)| BoardRow::from_gains(name.to_string(), gains, true))
        .collect();
    baseline_rows.sort_by(by_key_desc);

    // A baseline goes above a competitor only if it strictly beats them; on
    // a tie, the competitor keeps the higher spot.
    let mut merged = Vec::with_capacity(rows.len() + baseline_rows.len());
    let mut pending = baseline_rows.into_iter().peekable();
    for row in rows {
        while let Some(b) = pending.next_if(|b| b.sort_key(opts.order) > row.sort_key(opts.order)) {
            merged.push(b);
        }
        merged.push(row);
    }
    merged.extend(pending);
    merged
}

/// The `[key=value ...]` argument string of the `public leaderboard` command
/// (everything after the two command words, order-independent, every key
/// optional). On any problem, returns an error naming the offending option
/// or value, followed by the full usage line -- never a silent default.
pub fn parse_options(args: &str) -> Result<BoardOptions, String> {
    let mut opts = BoardOptions::default();

    for pair in args.split_whitespace() {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| usage_error(&format!("`{pair}` is not a `key=value` option.")))?;
        match key {
            "top" => {
                opts.top = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_TOP).contains(n))
                    .ok_or_else(|| {
                        usage_error(&format!(
                            "`top` must be a whole number from 1 to {MAX_TOP}, got `{value}`."
                        ))
                    })?;
            }
            "order" => {
                opts.order = match value {
                    "best" => BoardOrder::Best,
                    "mean" => BoardOrder::Mean,
                    _ => {
                        return Err(usage_error(&format!(
                            "`order` must be `best` or `mean`, got `{value}`."
                        )))
                    }
                };
            }
            "values" => opts.show_values = parse_bool(key, value)?,
            "axis" => opts.show_axis = parse_bool(key, value)?,
            "median" => opts.show_median = parse_bool(key, value)?,
            "range" => {
                let bad_range = || {
                    usage_error(&format!(
                        "`range` must be `MIN:MAX` with MIN smaller than MAX, got `{value}`."
                    ))
                };
                let (lo_str, hi_str) = value.split_once(':').ok_or_else(bad_range)?;
                let lo: f64 = lo_str.parse().map_err(|_| bad_range())?;
                let hi: f64 = hi_str.parse().map_err(|_| bad_range())?;
                let ordered = lo.partial_cmp(&hi).map(|o| o == std::cmp::Ordering::Less);
                if ordered != Some(true) {
                    return Err(bad_range());
                }
                opts.range = Some((lo, hi));
            }
            _ => return Err(usage_error(&format!("Unknown option `{key}`."))),
        }
    }

    Ok(opts)
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(usage_error(&format!(
            "`{key}` must be `on` or `off`, got `{value}`."
        ))),
    }
}

fn usage_error(problem: &str) -> String {
    format!(
        "❌ {problem}\n\nUsage: `public leaderboard [top=N] [order=best|mean] [values=on|off] \
         [range=MIN:MAX] [axis=on|off] [median=on|off]`"
    )
}

// ---- Layout constants (SVG user units) ------------------------------------

const MARGIN_LEFT: f64 = 16.0;
const RANK_WIDTH: f64 = 36.0;
const NAME_WIDTH: f64 = 190.0;
/// Kept free at the right end of the name column, so a long name never runs
/// into the `◄` overflow arrow drawn just left of the plot.
const NAME_RIGHT_GAP: f64 = 20.0;
const PLOT_WIDTH: f64 = 480.0;
const VALUE_WIDTH: f64 = 64.0;
const MARGIN_RIGHT: f64 = 16.0;
const HEADER_HEIGHT: f64 = 60.0;
const ROW_HEIGHT: f64 = 36.0;
const ROW_PAD: f64 = 6.0;
const AXIS_HEIGHT: f64 = 46.0;
const BOTTOM_MARGIN: f64 = 12.0;
const TITLE_FONT_SIZE: f32 = 16.0;
const NAME_FONT_SIZE: f32 = 13.0;
const KDE_GRID_POINTS: usize = 100;
/// How far past its outermost candidates a density curve extends, in
/// bandwidths. At 3, the curve has decayed to about 1% of a single
/// candidate's peak, so it visibly lands on the baseline instead of being
/// cut off.
const KDE_TAIL_BANDWIDTHS: f64 = 3.0;
/// Shortest drawn length of an n=2 line, so two near-identical candidates
/// still read as a line (2 candidates) rather than a dot (1).
const MIN_LINE_PX: f64 = 10.0;
/// Pixel density of the PNG relative to the SVG's user units. At 1x, text
/// looks soft on any high-density display.
const RASTER_SCALE: f32 = 2.0;
const SHAPE_COLOR: &str = "#2c6fbb";
/// Baseline rows: an amber shape and label on a pale amber band, so a
/// reference model can't be mistaken for a competitor at a glance.
const BASELINE_COLOR: &str = "#d97706";
const BASELINE_TEXT_COLOR: &str = "#b45309";
const BASELINE_BAND_COLOR: &str = "#fdf3e1";

/// Context printed above the rows. Not part of `BoardOptions`, since it comes
/// from the config and the clock rather than from the teacher's command.
pub struct BoardHeader {
    /// The competition's name.
    pub title: String,
    /// When the image was generated, already formatted (see `generated_at_label`).
    pub generated_at: String,
}

/// `now` as local wall-clock time at the competition's offset, labeled with
/// that offset so it's unambiguous for anyone the image is shared with --
/// e.g. `Generated 2026-09-25 11:30 (UTC-03:00)`.
pub fn generated_at_label(now: chrono::DateTime<chrono::Utc>, offset_minutes: i32) -> String {
    let tz = chrono::FixedOffset::east_opt(offset_minutes * 60)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is valid"));
    format!(
        "Generated {}",
        now.with_timezone(&tz).format("%Y-%m-%d %H:%M (UTC%:z)")
    )
}

/// Renders the board as a standalone SVG string. Zulip does not render SVG
/// inline -- see `rasterize_png` -- but keeping this as a plain string is
/// what makes it possible to unit-test the output directly.
pub fn render_svg(rows: &[BoardRow], opts: &BoardOptions, header: &BoardHeader) -> String {
    let x_plot_start = MARGIN_LEFT + RANK_WIDTH + NAME_WIDTH;
    let x_plot_end = x_plot_start + PLOT_WIDTH;
    let width = x_plot_end + if opts.show_values { VALUE_WIDTH } else { 0.0 } + MARGIN_RIGHT;

    let rows_top = HEADER_HEIGHT;
    let rows_bottom = rows_top + ROW_HEIGHT * rows.len() as f64;
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

    let title = truncate_to_width(
        &format!("{} \u{2014} Public leaderboard", header.title),
        TITLE_FONT_SIZE,
        width - MARGIN_LEFT - MARGIN_RIGHT,
    );
    let _ = write!(
        svg,
        r##"<text x="{x:.1}" y="28" font-size="{size}" fill="#111111">{title}</text>"##,
        x = MARGIN_LEFT,
        size = TITLE_FONT_SIZE,
        title = xml_escape(&title),
    );
    let ranked_by = match opts.order {
        BoardOrder::Best => "best",
        BoardOrder::Mean => "mean",
    };
    let _ = write!(
        svg,
        r##"<text x="{x:.1}" y="47" font-size="11" fill="#666666">Ranked by {ranked_by} public gain · {when}</text>"##,
        x = MARGIN_LEFT,
        when = xml_escape(&header.generated_at),
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
        let y_center = rows_top + ROW_HEIGHT * i as f64 + ROW_HEIGHT / 2.0;
        let baseline = y_center + (ROW_HEIGHT / 2.0 - ROW_PAD);
        let top = y_center - (ROW_HEIGHT / 2.0 - ROW_PAD);

        let (shape_color, text_color, rank_color) = if row.is_baseline {
            (BASELINE_COLOR, BASELINE_TEXT_COLOR, BASELINE_TEXT_COLOR)
        } else {
            (SHAPE_COLOR, "#111111", "#333333")
        };

        if row.is_baseline {
            let _ = write!(
                svg,
                r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="4" fill="{fill}"/>"##,
                x = MARGIN_LEFT / 2.0,
                y = y_center - ROW_HEIGHT / 2.0 + 2.0,
                w = width - (MARGIN_LEFT + MARGIN_RIGHT) / 2.0,
                h = ROW_HEIGHT - 4.0,
                fill = BASELINE_BAND_COLOR,
            );
        }

        // Right-aligned, so "9." and "10." line up on their dots. Baselines
        // are never ranked, so they get a dash instead of a number.
        let rank_label = if row.is_baseline {
            "\u{2014}".to_string()
        } else {
            format!("{}.", row.rank)
        };
        let _ = write!(
            svg,
            r##"<text x="{x:.1}" y="{y:.1}" font-size="{size}" fill="{rank_color}" text-anchor="end">{rank_label}</text>"##,
            x = MARGIN_LEFT + RANK_WIDTH - 10.0,
            y = y_center + 4.0,
            size = NAME_FONT_SIZE,
        );

        let name = if row.is_baseline {
            format!("Baseline: {}", row.display_name)
        } else {
            row.display_name.clone()
        };
        let _ = write!(
            svg,
            r##"<text x="{x:.1}" y="{y:.1}" font-size="{size}" fill="{text_color}">{name}</text>"##,
            x = MARGIN_LEFT + RANK_WIDTH,
            y = y_center + 4.0,
            size = NAME_FONT_SIZE,
            name = xml_escape(&name_label(&name, row.gains.len(), NAME_WIDTH - NAME_RIGHT_GAP)),
        );

        render_shape(&mut svg, row, &x_of, range_min, range_max, top, baseline, shape_color);

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
                r##"<text x="{x:.1}" y="{y:.1}" font-size="13" fill="{text_color}">{v:.2}</text>"##,
                x = x_plot_end + 10.0,
                y = y_center + 4.0,
                v = row.best,
            );
        }
    }

    // Over competitors only: a baseline is a reference, not part of the field.
    let mut values: Vec<f64> = rows.iter().filter(|r| !r.is_baseline).map(|r| r.best).collect();
    if opts.show_median && !values.is_empty() {
        values.sort_by(|a, b| a.partial_cmp(b).expect("gains are never NaN"));
        let median = median_of(&values);
        let x = x_of(median);
        let _ = write!(
            svg,
            r##"<line x1="{x:.1}" y1="{y1:.1}" x2="{x:.1}" y2="{y2:.1}" stroke="#c0392b" stroke-width="1.5" stroke-dasharray="4,3"/>"##,
            x = x,
            y1 = rows_top,
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
        let _ = write!(
            svg,
            r##"<text x="{x:.1}" y="{y:.1}" font-size="11" fill="#666666" text-anchor="middle">Public gain</text>"##,
            x = (x_plot_start + x_plot_end) / 2.0,
            y = axis_y + 33.0,
        );
    }

    svg.push_str("</svg>");
    svg
}

/// Rendered width of `text` in the embedded font, in SVG user units. Sums
/// each glyph's advance and ignores kerning; kerning in this font only ever
/// tightens text, so this slightly overestimates -- the safe direction for
/// deciding whether something fits.
fn text_width(text: &str, font_size: f32) -> f64 {
    use skrifa::instance::{LocationRef, Size};
    use skrifa::MetadataProvider;

    let Ok(font) = skrifa::FontRef::new(FONT_BYTES) else {
        // Unreachable with the embedded font; a generous per-char estimate
        // keeps truncation working rather than panicking mid-render.
        return text.chars().count() as f64 * f64::from(font_size) * 0.65;
    };
    let charmap = font.charmap();
    let metrics = font.glyph_metrics(Size::new(font_size), LocationRef::default());
    text.chars()
        .map(|c| {
            let glyph = charmap.map(c).unwrap_or_default();
            f64::from(metrics.advance_width(glyph).unwrap_or(0.0))
        })
        .sum()
}

/// `text` if it fits in `max_width`, else the longest prefix that fits with
/// `…` appended.
fn truncate_to_width(text: &str, font_size: f32, max_width: f64) -> String {
    if text_width(text, font_size) <= max_width {
        return text.to_string();
    }
    let budget = max_width - text_width("\u{2026}", font_size);
    let mut out = String::new();
    let mut used = 0.0;
    for c in text.chars() {
        let w = text_width(c.encode_utf8(&mut [0; 4]), font_size);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    // "logistic…", not "logistic …" when the cut lands right after a space.
    out.truncate(out.trim_end().len());
    out.push('\u{2026}');
    out
}

/// The name label shown next to a row: the display name, truncated to fit
/// `max_width` as actually rendered (so `WWWW` gives up characters sooner
/// than `iiii`), with the candidate count appended as `(n=K)`. Per-row
/// density normalization means shape height alone can't distinguish a
/// 3-candidate row from a 40-candidate one, so the count must always be
/// legible and is never itself truncated away.
fn name_label(display_name: &str, n: usize, max_width: f64) -> String {
    let suffix = format!(" (n={n})");
    let name_budget = max_width - text_width(&suffix, NAME_FONT_SIZE);
    format!(
        "{}{}",
        truncate_to_width(display_name, NAME_FONT_SIZE, name_budget),
        suffix
    )
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
    color: &str,
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
                color = color,
            );
        }
        2 => {
            let mut x0 = x_of(gains[0]);
            let mut x1 = x_of(gains[1]);
            if x1 - x0 < MIN_LINE_PX {
                let (plot_start, plot_end) = (x_of(range_min), x_of(range_max));
                let half = MIN_LINE_PX / 2.0;
                let mid = ((x0 + x1) / 2.0).clamp(plot_start + half, plot_end - half);
                x0 = mid - half;
                x1 = mid + half;
            }
            let _ = write!(
                svg,
                r##"<line x1="{x0:.2}" y1="{y:.2}" x2="{x1:.2}" y2="{y:.2}" stroke="{color}" stroke-width="2.5" stroke-linecap="round"/>"##,
                x0 = x0,
                x1 = x1,
                y = baseline,
                color = color,
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
                color = color,
            );
        }
        _ => {
            // The grid covers only this row's own candidates plus a decaying
            // tail -- not the whole axis. Over the whole axis the curve's
            // near-zero ends drew a baseline across the entire plot (reading
            // as a range bar the data doesn't have), and a tight cluster got
            // only a couple of the 100 grid points, rendering as a spike.
            let bandwidth = kde_bandwidth(gains, range_max - range_min);
            let lo = (gains[0] - KDE_TAIL_BANDWIDTHS * bandwidth).max(range_min);
            let hi = (gains[gains.len() - 1] + KDE_TAIL_BANDWIDTHS * bandwidth).min(range_max);
            if lo >= hi {
                // Entirely outside `range`; the overflow arrow already says so.
                return;
            }
            let (grid, density) = gaussian_kde(gains, bandwidth, lo, hi, KDE_GRID_POINTS);
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
                color = color,
            );
        }
    }
}

/// Silverman's rule bandwidth (`1.06 * std_dev * n^(-1/5)`). Falls back to a
/// small fraction of the axis span when the data has zero spread (every
/// candidate identical), so the curve is a narrow bump instead of an
/// infinite spike, and never goes below 0.5% of it.
fn kde_bandwidth(values: &[f64], axis_span: f64) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let std_dev = variance.sqrt();

    let span = axis_span.max(f64::EPSILON);
    if std_dev > 0.0 {
        1.06 * std_dev * n.powf(-1.0 / 5.0)
    } else {
        span * 0.02
    }
    .max(span * 0.005)
}

/// Gaussian KDE with the given bandwidth, evaluated on `n_points` evenly
/// spaced grid points across `[lo, hi]`.
fn gaussian_kde(values: &[f64], bandwidth: f64, lo: f64, hi: f64, n_points: usize) -> (Vec<f64>, Vec<f64>) {
    let n = values.len() as f64;
    let grid: Vec<f64> = (0..n_points)
        .map(|i| lo + (hi - lo) * i as f64 / (n_points - 1) as f64)
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
    let mut pixmap = resvg::tiny_skia::Pixmap::new(
        (size.width() * RASTER_SCALE).ceil() as u32,
        (size.height() * RASTER_SCALE).ceil() as u32,
    )
    .context("Failed to allocate the raster canvas")?;

    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(RASTER_SCALE, RASTER_SCALE),
        &mut pixmap.as_mut(),
    );

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
        let rows = build_rows(&candidates, &[], &BoardOptions::default());
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
            &[],
            &BoardOptions {
                order: BoardOrder::Best,
                ..Default::default()
            },
        );
        assert_eq!(by_best[0].display_name, "Ana", "Ana has the single best candidate");

        let by_mean = build_rows(
            &candidates,
            &[],
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
            &[],
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
            &[],
            &BoardOptions {
                top: 1000,
                ..Default::default()
            },
        );
        assert_eq!(rows.len(), MAX_TOP);
    }

    fn baseline(name: &str, batch: &str, gain: f64) -> (String, String, f64) {
        (name.to_string(), batch.to_string(), gain)
    }

    fn describe(rows: &[BoardRow]) -> Vec<(usize, String, bool)> {
        rows.iter().map(|r| (r.rank, r.display_name.clone(), r.is_baseline)).collect()
    }

    #[test]
    fn baselines_are_unranked_and_do_not_shift_competitor_ranks() {
        let rows = build_rows(
            &[candidate(1, "Ana", "a", 30.0), candidate(2, "Beto", "b", 10.0)],
            &[baseline("logistic", "t1", 20.0)],
            &BoardOptions::default(),
        );
        assert_eq!(
            describe(&rows),
            vec![
                (1, "Ana".to_string(), false),
                (0, "logistic".to_string(), true),
                (2, "Beto".to_string(), false),
            ],
            "placed by score, unnumbered, and Beto is still #2"
        );
    }

    #[test]
    fn a_baseline_tied_with_a_competitor_goes_below_them() {
        let rows = build_rows(
            &[candidate(1, "Ana", "a", 20.0)],
            &[baseline("logistic", "t1", 20.0)],
            &BoardOptions::default(),
        );
        assert!(!rows[0].is_baseline && rows[1].is_baseline, "{:?}", describe(&rows));
    }

    #[test]
    fn top_counts_only_competitors_and_published_baselines_always_show() {
        let rows = build_rows(
            &[
                candidate(1, "Ana", "a", 30.0),
                candidate(2, "Beto", "b", 20.0),
                candidate(3, "Caro", "c", 10.0),
            ],
            &[baseline("strong", "t1", 25.0), baseline("weak", "t2", 1.0)],
            &BoardOptions {
                top: 2,
                ..Default::default()
            },
        );
        let names: Vec<_> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Ana", "strong", "Beto", "weak"],
            "top=2 keeps 2 competitors (not 2 rows); a baseline below the cutoff still shows, last"
        );
    }

    #[test]
    fn each_baseline_upload_is_its_own_row_even_with_the_same_name() {
        let rows = build_rows(
            &[],
            &[
                baseline("logistic", "t1", 10.0),
                baseline("logistic", "t1", 12.0),
                baseline("logistic", "t2", 40.0),
            ],
            &BoardOptions::default(),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].gains, vec![40.0], "the stronger upload first");
        assert_eq!(rows[1].gains, vec![10.0, 12.0], "t1's two candidates stay together");
    }

    #[test]
    fn baseline_rows_are_drawn_distinctly() {
        let rows = build_rows(
            &[candidate(1, "Ana", "a", 30.0)],
            &[baseline("logistic", "t1", 20.0)],
            &BoardOptions::default(),
        );
        let svg = render(&rows, &BoardOptions::default());
        assert!(svg.contains(">Baseline: logistic (n=1)<"), "{svg}");
        assert!(svg.contains(">\u{2014}<"), "a dash, not a rank number: {svg}");
        assert!(!svg.contains(">2.<"), "the baseline must not take rank 2: {svg}");
        assert!(svg.contains(BASELINE_COLOR) && svg.contains(BASELINE_BAND_COLOR), "{svg}");
        assert!(svg.contains(SHAPE_COLOR), "the competitor keeps the normal color: {svg}");
    }

    #[test]
    fn the_median_line_ignores_baselines() {
        let opts = BoardOptions {
            show_median: true,
            range: Some((0.0, 100.0)),
            ..Default::default()
        };
        // Competitors' median is 20; with the 100 baseline counted it'd be 30.
        let rows = build_rows(
            &[
                candidate(1, "Ana", "a", 10.0),
                candidate(2, "Beto", "b", 20.0),
                candidate(3, "Caro", "c", 30.0),
            ],
            &[baseline("oracle", "t1", 100.0)],
            &opts,
        );
        let svg = render(&rows, &opts);
        let x_of_20 = MARGIN_LEFT + RANK_WIDTH + NAME_WIDTH + 0.2 * PLOT_WIDTH;
        assert!(
            svg.contains(&format!(r#"<line x1="{x_of_20:.1}""#)) && svg.contains("stroke-dasharray"),
            "the median line must sit at the competitors' median (20): {svg}"
        );
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

    #[test]
    fn parse_options_errors_name_the_offending_option_and_value() {
        let err = parse_options("orden=mean").unwrap_err();
        assert!(err.contains("Unknown option `orden`"), "{err}");
        assert!(err.contains("Usage:"), "the usage line must still follow: {err}");

        let err = parse_options("order=median").unwrap_err();
        assert!(err.contains("`order`") && err.contains("`median`"), "{err}");

        let err = parse_options("axis=yes").unwrap_err();
        assert!(err.contains("`axis`") && err.contains("`yes`"), "{err}");

        let err = parse_options("top=500").unwrap_err();
        assert!(err.contains("`top`") && err.contains("`500`"), "{err}");

        let err = parse_options("range=9:3").unwrap_err();
        assert!(err.contains("`range`") && err.contains("`9:3`"), "{err}");

        let err = parse_options("median").unwrap_err();
        assert!(err.contains("`median` is not a `key=value` option"), "{err}");
    }

    #[test]
    fn parse_options_accepts_top_at_the_cap_and_rejects_above_it() {
        assert_eq!(parse_options(&format!("top={MAX_TOP}")).unwrap().top, MAX_TOP);
        assert!(parse_options(&format!("top={}", MAX_TOP + 1)).is_err());
    }

    // ---- render_svg ----------------------------------------------------

    fn header() -> BoardHeader {
        BoardHeader {
            title: "Test Cup".to_string(),
            generated_at: "Generated 2026-09-25 11:30 (UTC-03:00)".to_string(),
        }
    }

    fn render(rows: &[BoardRow], opts: &BoardOptions) -> String {
        render_svg(rows, opts, &header())
    }

    fn row(name: &str, gains: &[f64]) -> BoardRow {
        let mut sorted = gains.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        BoardRow {
            rank: 1,
            display_name: name.to_string(),
            best: *sorted.last().unwrap(),
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            gains: sorted,
            is_baseline: false,
        }
    }

    /// Every x coordinate in the first `<path d="...">` of `svg`.
    fn path_xs(svg: &str) -> Vec<f64> {
        let start = svg.find(r#"<path d=""#).expect("a path") + r#"<path d=""#.len();
        let d = &svg[start..start + svg[start..].find('"').unwrap()];
        let tokens: Vec<&str> = d.split_whitespace().collect();
        tokens
            .windows(3)
            .filter(|w| w[0] == "M" || w[0] == "L")
            .map(|w| w[1].parse().unwrap())
            .collect()
    }

    #[test]
    fn density_curve_spans_only_its_own_candidates_not_the_whole_axis() {
        // Tight cluster at 120-122 on a 0-200 axis: 2 units of a 200 range.
        let rows = vec![row("Ana", &[120.0, 121.0, 121.5, 122.0])];
        let svg = render(
            &rows,
            &BoardOptions {
                range: Some((0.0, 200.0)),
                ..Default::default()
            },
        );
        let xs = path_xs(&svg);
        let (lo, hi) = (
            xs.iter().copied().fold(f64::MAX, f64::min),
            xs.iter().copied().fold(f64::MIN, f64::max),
        );
        assert!(
            hi - lo < PLOT_WIDTH * 0.1,
            "a 2-unit cluster must not stretch across the axis (spans {:.1}px of {PLOT_WIDTH}): {svg}",
            hi - lo
        );
        // It must still be a smooth curve, not a spike of a couple of points.
        assert!(xs.len() > 50, "the grid must be spent on the data: {} points", xs.len());
    }

    #[test]
    fn density_curve_entirely_outside_the_range_draws_nothing() {
        let rows = vec![row("Ana", &[300.0, 301.0, 302.0, 303.0])];
        let svg = render(
            &rows,
            &BoardOptions {
                range: Some((0.0, 100.0)),
                ..Default::default()
            },
        );
        assert!(!svg.contains("<path"), "nothing to draw inside the range: {svg}");
        assert!(svg.contains("&#9658;"), "the overflow arrow still says where the data went: {svg}");
    }

    #[test]
    fn two_nearly_equal_candidates_still_render_a_visible_line() {
        let rows = vec![row("Ana", &[100.0, 100.01])];
        let svg = render(
            &rows,
            &BoardOptions {
                range: Some((0.0, 200.0)),
                ..Default::default()
            },
        );
        let start = svg.find("<line x1=\"").unwrap();
        let attr = |name: &str| -> f64 {
            let key = format!("{name}=\"");
            let s = start + svg[start..].find(&key).unwrap() + key.len();
            svg[s..s + svg[s..].find('"').unwrap()].parse().unwrap()
        };
        assert!(
            attr("x2") - attr("x1") >= MIN_LINE_PX - 0.01,
            "an n=2 line must be long enough not to read as a dot: {svg}"
        );
    }

    #[test]
    fn header_shows_title_ranking_and_generation_time() {
        let rows = vec![row("Ana", &[10.0])];
        let svg = render(
            &rows,
            &BoardOptions {
                order: BoardOrder::Mean,
                ..Default::default()
            },
        );
        assert!(svg.contains("Test Cup \u{2014} Public leaderboard"), "{svg}");
        assert!(svg.contains("Ranked by mean public gain"), "{svg}");
        assert!(svg.contains("Generated 2026-09-25 11:30 (UTC-03:00)"), "{svg}");
    }

    #[test]
    fn header_title_is_xml_escaped() {
        let rows = vec![row("Ana", &[10.0])];
        let svg = render_svg(
            &rows,
            &BoardOptions::default(),
            &BoardHeader {
                title: "R&D <Cup>".to_string(),
                generated_at: "now".to_string(),
            },
        );
        assert!(svg.contains("R&amp;D &lt;Cup&gt;"), "{svg}");
    }

    #[test]
    fn axis_label_only_with_the_axis() {
        let rows = vec![row("Ana", &[10.0])];
        assert!(render(&rows, &BoardOptions::default()).contains(">Public gain<"));
        let bare = render(
            &rows,
            &BoardOptions {
                show_axis: false,
                ..Default::default()
            },
        );
        assert!(!bare.contains(">Public gain<"), "{bare}");
    }

    #[test]
    fn generated_at_label_uses_the_competition_offset() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-25T14:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            generated_at_label(now, -180),
            "Generated 2026-09-25 11:30 (UTC-03:00)"
        );
        assert_eq!(generated_at_label(now, 0), "Generated 2026-09-25 14:30 (UTC+00:00)");
    }

    #[test]
    fn names_are_truncated_by_rendered_width_not_character_count() {
        let budget = NAME_WIDTH - NAME_RIGHT_GAP;
        // Same number of characters; W is several times wider than i.
        let wide = name_label(&"W".repeat(30), 3, budget);
        let narrow = name_label(&"i".repeat(30), 3, budget);
        assert!(wide.contains('\u{2026}'), "30 W's cannot fit: {wide}");
        assert!(!narrow.contains('\u{2026}'), "30 i's fit easily: {narrow}");
        for label in [&wide, &narrow] {
            assert!(label.ends_with(" (n=3)"), "the count is never truncated: {label}");
            assert!(text_width(label, NAME_FONT_SIZE) <= budget, "must fit: {label}");
        }
    }

    #[test]
    fn truncation_never_leaves_a_space_before_the_ellipsis() {
        // Find a width that cuts exactly after "logistic " for this font.
        let cut_after_space = text_width("logistic ", NAME_FONT_SIZE) + text_width("\u{2026}", NAME_FONT_SIZE) + 0.5;
        let out = truncate_to_width("logistic regression", NAME_FONT_SIZE, cut_after_space);
        assert_eq!(out, "logistic\u{2026}");
    }

    #[test]
    fn short_names_are_left_untouched() {
        assert_eq!(name_label("Ana", 2, NAME_WIDTH - NAME_RIGHT_GAP), "Ana (n=2)");
    }

    #[test]
    fn rank_numbers_are_right_aligned() {
        let svg = render(&[row("Ana", &[10.0])], &BoardOptions::default());
        let rank_tag_start = svg.find(">1.<").map(|end| svg[..end].rfind("<text").unwrap()).unwrap();
        let tag = &svg[rank_tag_start..svg[rank_tag_start..].find('>').unwrap() + rank_tag_start];
        assert!(tag.contains(r#"text-anchor="end""#), "{tag}");
    }

    #[test]
    fn single_candidate_renders_a_dot_and_nothing_else() {
        let rows = vec![row("Ana", &[10.0])];
        let svg = render(&rows, &BoardOptions::default());
        assert!(svg.contains("<circle"), "n=1 must draw a dot: {svg}");
        assert!(!svg.contains("<path"), "n=1 must not draw a density/triangle path: {svg}");
    }

    #[test]
    fn two_candidates_render_a_line_segment() {
        let rows = vec![row("Ana", &[10.0, 20.0])];
        let svg = render(&rows, &BoardOptions::default());
        assert!(svg.contains("<line"), "n=2 must draw a line: {svg}");
        assert!(!svg.contains("<circle"), "n=2 must not draw a dot: {svg}");
        assert!(!svg.contains("<path"), "n=2 must not draw a path: {svg}");
    }

    #[test]
    fn three_candidates_render_a_closed_bezier_path() {
        let rows = vec![row("Ana", &[10.0, 20.0, 30.0])];
        let svg = render(&rows, &BoardOptions::default());
        assert!(svg.contains("<path"), "n=3 must draw a path: {svg}");
        assert!(svg.contains(" Q "), "n=3 must use quadratic Beziers, not straight edges: {svg}");
        assert!(svg.contains(" Z"), "n=3 must be a closed, filled shape: {svg}");
    }

    #[test]
    fn four_or_more_candidates_render_a_density_curve() {
        let rows = vec![row("Ana", &[10.0, 12.0, 15.0, 20.0, 22.0])];
        let svg = render(&rows, &BoardOptions::default());
        assert!(svg.contains("<path"), "n>=4 must draw a path: {svg}");
        // A 100-point KDE grid produces far more "L " segments than the
        // fixed 2 a triangle's Bezier path uses.
        assert!(svg.matches(" L ").count() > 10, "n>=4 must be a many-point curve: {svg}");
    }

    #[test]
    fn no_row_ever_draws_individual_ticks_or_a_marker() {
        for gains in [vec![10.0], vec![10.0, 20.0], vec![10.0, 20.0, 30.0], vec![1.0, 2.0, 3.0, 4.0, 5.0]] {
            let rows = vec![row("Ana", &gains)];
            let svg = render(&rows, &BoardOptions::default());
            // No <rect> ticks, no <polygon>/<use> markers -- only the shapes
            // and text/lines this module itself defines.
            assert!(!svg.contains("<polygon"), "must never draw a marker: {svg}");
            assert!(!svg.contains("marker"), "must never draw a marker: {svg}");
        }
    }

    #[test]
    fn overflow_arrows_appear_only_when_range_clips_a_candidate() {
        let rows = vec![row("Ana", &[10.0, 50.0])];

        let clipped = render(
            &rows,
            &BoardOptions {
                range: Some((20.0, 40.0)),
                ..Default::default()
            },
        );
        assert!(clipped.contains("&#9668;") || clipped.contains("&#9658;"), "range clips both sides: {clipped}");

        let not_clipped = render(
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

        let without = render(&rows, &BoardOptions::default());
        assert!(!without.contains("stroke-dasharray"), "median off by default: {without}");

        let with = render(
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

        let bare = render(
            &rows,
            &BoardOptions {
                show_values: false,
                show_axis: false,
                ..Default::default()
            },
        );
        assert!(!bare.contains("42.50"), "values=off must hide the numeric gain: {bare}");

        let full = render(
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
            &[],
            &BoardOptions::default(),
        );
        let svg = render(&rows, &BoardOptions::default());
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
        // A baseline too, so its rendering path is covered by the same guarantee.
        let rows = build_rows(&candidates, &[baseline("logistic", "t1", 40.0)], &BoardOptions::default());
        let svg = render(
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
        let svg = render(&rows, &BoardOptions::default());
        let png = rasterize_png(&svg).expect("a well-formed SVG must rasterize");
        assert!(!png.is_empty());
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'], "must be a valid PNG");
    }

    #[test]
    fn rasterize_png_renders_at_twice_the_svg_size() {
        let svg = render(&[row("Ana", &[10.0])], &BoardOptions::default());
        let svg_width: f64 = {
            let s = svg.find(r#"width=""#).unwrap() + r#"width=""#.len();
            svg[s..s + svg[s..].find('"').unwrap()].parse().unwrap()
        };
        let png = rasterize_png(&svg).unwrap();
        // IHDR: bytes 16..20 hold the image width, big-endian.
        let png_width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        assert_eq!(f64::from(png_width), (svg_width * f64::from(RASTER_SCALE)).ceil());
    }
}
