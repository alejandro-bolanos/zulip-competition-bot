# zulip-competition-bot

A Zulip bot that runs Kaggle-style machine learning competitions, written in Rust. Students submit CSV predictions over private Zulip messages; the bot scores them against a master dataset, tracks a leaderboard, and can export final grades.

## Features

- Two competition modes: **blind** (threshold-only feedback, single CSV per submission) and **kaggle** (multiple candidate CSVs per submission, scored against a public/private data split)
- Automatic gain calculation from a configurable confusion-matrix gain formula
- Full leaderboard with per-competitor statistics
- Duplicate-submission detection (same file from two different accounts)
- Roster-based authorization: only competitors and teachers listed in a CSV can talk to the bot
- Per-competitor daily submission quota
- Golden bullets: a limited, per-competitor allowance to see a submission's real gain immediately (blind mode only)
- On-demand grade export to CSV, uploaded straight to Zulip
- Deadline and results-reveal-date gating, both timezone-aware
- SHA-256 checksums on every stored file
- Per-message panic isolation — one malformed message can't take the whole bot down

## Requirements

- Rust 1.70 or newer
- A Zulip account and API key for the bot

## Installation

```bash
cd zulip-competition-bot
cargo build --release
```

## Configuration

### 1. Generate a config template

```bash
./target/release/zulip-competition-bot create-config
```

This writes an example `config.json` in the current directory.

### 2. Edit `config.json`

```json
{
  "zulip": {
    "email": "your-bot@example.com",
    "api_key": "your-api-key",
    "site": "https://your-org.zulipchat.com"
  },
  "database": {
    "path": "zulip_competition.db"
  },
  "logs": {
    "path": "logs"
  },
  "teachers": [
    "teacher1@example.com",
    "teacher2@example.com"
  ],
  "master_data": {
    "path": "master_data.csv"
  },
  "submissions": {
    "path": "./submissions"
  },
  "roster": {
    "path": "roster.csv"
  },
  "gain_matrix": {
    "tp": 1.0,
    "tn": 0.5,
    "fp": -0.1,
    "fn_": -0.5
  },
  "gain_thresholds": [
    {
      "min_gain": 100,
      "category": "excellent",
      "message": "Outstanding model!",
      "gifs": [
        "https://media.giphy.com/media/your-gif/giphy.gif"
      ]
    }
  ],
  "competition": {
    "name": "ML Competition 2025",
    "description": "Competition description",
    "deadline": "2025-12-31T23:59:59",
    "results_reveal_date": "2026-01-01T23:59:59",
    "timezone_offset_minutes": -180,
    "mode": "blind"
  }
}
```

Notes on a few fields:

- `deadline` and `results_reveal_date` accept either RFC3339 (`2025-12-31T23:59:59Z`, own offset always wins) or a naive `%Y-%m-%dT%H:%M:%S` string, read at `timezone_offset_minutes` (minutes to *add* to UTC to get local time — e.g. `-180` for Argentina). This same offset also defines the calendar-day boundary used by the daily submission quota. It defaults to `0` (UTC) if omitted.
- `mode` is `"blind"` (default) or `"kaggle"` — see [Competition modes](#competition-modes) below. Both require `roster.csv` and `master_data.csv`; kaggle mode additionally requires `master_data.csv` to carry a `split` column (see below). The bot refuses to start if a required file is missing or malformed.

### 3. Prepare the master dataset

`master_data.csv`, blind mode:

```csv
id,label
1,0
2,1
3,0
```

`master_data.csv`, kaggle mode — a third `split` column marks each id as `public` or `private`:

```csv
id,label,split
1,0,public
2,1,private
3,0,public
```

`label` is `1` for a true positive, `0` otherwise.

### 4. Prepare the roster

`roster.csv` lists every competitor allowed to talk to the bot. Anyone not on this list (and not in `teachers`) gets a single rejection message and nothing else — they can't even see the command list.

```csv
email,name,daily_limit,golden_bullets,max_files_per_submission
ana@example.com,Ana Gomez,5,3,1
beto@example.com,Beto Diaz,5,3,1
```

| Column | Meaning |
|---|---|
| `email` | Zulip login email, matched case-insensitively |
| `name` | Display name |
| `daily_limit` | Max submissions per local calendar day |
| `golden_bullets` | Total golden-bullet budget for the whole competition (blind mode only, see below) |
| `max_files_per_submission` | Max candidate CSVs per submission (kaggle mode only; ignored in blind mode, but the column is still required) |

All five columns are required regardless of competition mode. See `roster.example.csv` for a ready-to-copy template.

## Running the bot

```bash
# Development, with detailed logs
RUST_LOG=info cargo run -- run --config config.json

# Production
./target/release/zulip-competition-bot run --config config.json
```

Logs go to both stdout (compact) and `logs/zulip_competition_bot_YYYYMMDD.log` (detailed, appended). Test connectivity (credentials, a test DM, the event queue) with:

```bash
cargo run --bin diagnose
```

## Bot commands

All commands are sent as Zulip private messages to the bot.

### Students — blind mode

- `submit <name> <expected_gain>` — submit a model (attach one CSV)
- `reveal <name> <expected_gain>` — like `submit`, but spends one golden bullet to see the real gain and threshold category immediately, instead of waiting for `results_reveal_date`
- `list submits` — list your own submissions
- `help` — show this help

Your final result is your **last** pre-deadline submission, not your best one — choose carefully what you send last.

### Students — kaggle mode

- `submit <name>` — submit one or more candidate CSVs (one Zulip message, multiple attachments = one entry). No expected gain to type — the reply immediately shows the mean and standard deviation of the *public* gain across your candidates, never any individual candidate's score.
- `list submits` — list your own submissions, one row per batch (candidate count and public mean/std, not per-candidate detail)
- `help` — show this help

Your final result is your **last** pre-deadline submission, not your best one — choose carefully what you send last. Within that submission, the candidate with the best *public* gain is the one scored against the *private* split.

`reveal` is not available in kaggle mode: the public gain is already visible on every submit, and the private gain is exactly the value that would otherwise stay hidden until grading.

Both modes share the same daily submission quota — in kaggle mode, one entry (however many candidate files it has) spends exactly one unit of quota, not one per file.

**CSV format:** one column of predicted-positive IDs, no header.

```
123
456
789
```

### Teachers

- `duplicates` — list duplicate submissions (same file from two different accounts)
- `leaderboard [gain|datetime]` — full leaderboard with statistics, sorted by gain or by date
- `public leaderboard [top=N] [order=best|mean] [values=on|off] [range=MIN:MAX] [axis=on|off] [median=on|off]` — kaggle mode only, see [Public leaderboard image](#public-leaderboard-image)
- `all submits` — generate and upload a CSV of every submission in the system (every `Submission` column, plus a `candidates_in_batch` count)
- `no submits` — roster members with no submissions at all
- `user submits @user` — a specific user's submissions (use a real Zulip `@`-mention). In kaggle mode, one row per submit however many CSVs it had, with its candidate count, best public gain, that candidate's private gain, and the public mean ± std
- `roster reload` — reload the roster from disk without restarting the bot
- `grades` — generate and upload the grade CSV (see [Grading](#grading))
- `baseline <name>` (attach one or more CSVs), `baseline list`, `baseline publish <id>`, `baseline hide <id>` — kaggle mode only, see [Baselines](#baselines)
- `help` — show this help

Teachers cannot `submit` models as competitors. In kaggle mode they can upload reference models with `baseline` instead, which are never ranked or graded.

## Competition modes

**blind**: the classic mode. Each submission is one CSV, scored once against the whole dataset. Students only ever see which threshold category (and its message/GIF) their submission landed in, never the raw gain — they compete blind until `results_reveal_date`, or earlier via a golden bullet.

**kaggle**: closer to a real Kaggle competition. `master_data.csv` is split into a public and a private portion. A submission can bundle several candidate CSVs at once; each is scored against both splits, but students only see the *public* side, and only as a batch mean/std, never per-candidate. There is no expected gain to type either — the public gain is already shown in the submit reply itself. There is no separate pick step — the leaderboard and grades use the competitor's **last** pre-deadline submission's best-on-public candidate's *private* gain, the same "last submission wins" rule blind mode uses. Golden bullets don't exist in this mode.

## Grading

Teachers generate grades on demand with `grades` — nothing is exported automatically. The formula:

```
grade = 8 + 2 × (gain − median) / (max − median)
```

floored at 0. Median and max are computed only over competitors with at least one valid pre-deadline entry; a competitor with none gets a flat grade of 0. [Baselines](#baselines) never count, published or not. The maximum gain scores 10, the median scores 8, and everything scales linearly in between (below the median, this can go negative before the floor).

The exported CSV mirrors the private leaderboard row for that same competitor, so a grade and its leaderboard row can never disagree:

```csv
email,name,gain,expected_gain,submission_date,submissions,max,golden_bullet,grade
ana@example.com,Ana Gomez,132.5,120,2025-12-30T18:04:02Z,3,140,no,9.25
```

`gain`, `expected_gain`, and `grade` are all written at full precision, never rounded to a fixed number of decimals. `expected_gain` is blank in kaggle mode, which doesn't collect it. `submissions` and `max` are the same "total submissions" and "best-ever gain" columns `leaderboard` shows; `golden_bullet` marks whether the competitor's ranking submission was made with a golden bullet (always `no` in kaggle mode, where golden bullets don't exist). A competitor with no valid entry gets empty/zero values in every column except `email` and `name`, and a `grade` of `0`.

## Public leaderboard image

Kaggle mode only. Teachers generate a PNG leaderboard image with `public leaderboard`, safe to share directly with students — unlike `leaderboard`, it is built from a query that never returns `private_gain` or an email, only each competitor's Zulip display name and *public* gains. That same query only considers competitors currently on the roster, so removing someone from `roster.csv` also removes them from this image, even if their old submissions are still in the database. The bot uploads the PNG and DMs the teacher the link; the bot cannot post to a stream itself, so sharing it with the class is a manual step.

Each row shows a competitor's rank, display name (with `(n=K)` for their candidate count — shape height is normalized per row, so it alone can't tell a 3-candidate row from a 40-candidate one), and a shape summarizing the *public* gain of every candidate in their best-ever batch: a dot for a single candidate, a line for two, a smoothed triangle for three, and a filled density curve for four or more, drawn only over that competitor's own candidates rather than across the whole axis. No individual candidate ticks and no highlighted marker are ever drawn — the shape is the entire representation. The image carries its own context: the competition name, the ranking criterion, and when it was generated (in the competition's timezone), so it still makes sense once it's shared on its own.

Options (all optional, `key=value`, any order):

| Key | Values | Default | Meaning |
|---|---|---|---|
| `top` | 1–100 | 20 | How many competitors to show |
| `order` | `best`, `mean` | `best` | Rank by best-ever public gain, or by the mean of the representative batch |
| `values` | `on`, `off` | `on` | Show the numeric best-gain value per row |
| `range` | `MIN:MAX` | auto | X-axis bounds; candidates outside it are clipped, marked with a `◄`/`►` overflow arrow rather than silently dropped |
| `axis` | `on`, `off` | `on` | Show the numeric x-axis |
| `median` | `on`, `off` | `off` | Draw a vertical line at the median best *public* gain of the competitors shown (baselines excluded), so it moves with `top`. A visual reference only — it is **not** the median the grade formula uses, which is computed from *private* gains over every competitor |

Published [baselines](#baselines) appear as extra rows, placed where their own best public gain puts them, on a pale amber band with an amber shape, a `—` instead of a rank number, and the label `Baseline: <name>` — labeled by the baseline's name, never the teacher's. They never take a rank (the competitors around them keep theirs), never count toward `top`, and are shown even if they score below the last competitor shown.

## Baselines

Kaggle mode only. A baseline is a teacher's reference model — a simple benchmark such as "logistic regression" or "predict all positive" — for students to measure themselves against.

- `baseline <name>` with one or more CSVs attached uploads one. It goes through the same scoring as a student's `submit` (both splits, best-on-public candidate), but with no daily quota, no deadline gate, and up to 20 files. The reply shows its best public gain, that candidate's private gain, and its ID.
- It starts **hidden** from the public leaderboard image. `baseline publish <id>` shows it and `baseline hide <id>` hides it again, one baseline at a time.
- `baseline list` shows every baseline with its ID, candidate count, best public gain, that candidate's private gain, and whether it's shown.

A baseline is never a competitor: it's never ranked, never graded, never counted as the median or max, and never listed by `no submits`. Teachers always see every baseline in `leaderboard`, published or not: each is interleaved where its private gain puts it, with `—` instead of a position number, marked `📐 Baseline` plus 🌐 shown or 🔒 hidden. In the `all submits` CSV, `is_baseline` and `baseline_published` identify baseline rows. `list`, `publish` and `hide` are subcommands, so they can't be used as a baseline name.

## Golden bullets

Blind mode only. Each competitor has a fixed budget of golden bullets (`golden_bullets` in the roster) for the whole competition — not renewed daily. Using `reveal` instead of `submit` spends one bullet (only if the submission is actually stored — a rejected attempt costs nothing) and reveals the real gain and category right away. The leaderboard marks with 🌟 whenever a competitor's ranking submission was made with a golden bullet.

## Gain calculation

```
gain = TP × gain_tp + TN × gain_tn + FP × gain_fp + FN × gain_fn
```

Where TP/TN/FP/FN come from comparing the submitted predicted-positive IDs against the master dataset's true labels. In kaggle mode this confusion matrix (and gain) is computed twice per candidate CSV — once over the public ids, once over the private ones.

## Threshold categories

Submissions are classified by gain into `gain_thresholds` buckets (the highest `min_gain` at or below the score wins). Each category can have its own message and a pool of GIFs, one of which is picked at random for the reply. Kaggle mode does not use threshold categories.

## Checks

- Predicted IDs are validated against the master dataset; any unknown ID rejects the whole submission
- SHA-256 checksums on every stored file, used to detect duplicate submissions across accounts
- Deadline and results-reveal-date enforcement, both timezone-aware
- Per-competitor daily submission quota, and (kaggle mode) a per-submission cap on candidate files

## Development

```bash
cargo build --release
cargo test
cargo test test_gain_calculation   # a single test
cargo clippy
```

`tools/` is a separate uv-managed Python project with an end-to-end test that drives a running bot instance through real Zulip DMs — see [tools/README.md](tools/README.md). It's mode-aware: it reads which mode the bot instance it's pointed at is running and exercises that one; to cover both, run it against two separate bot instances.

See [CLAUDE.md](CLAUDE.md) for a detailed architecture and module-by-module walkthrough.

## License

MIT License
