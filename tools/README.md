# tools

Test and diagnostic scripts for zulip-competition-bot, driven through a real
Zulip realm. Managed as a [uv](https://docs.astral.sh/uv/) project, separate
from the Rust crate at the repo root.

## Setup

```bash
cd tools
uv sync
```

This creates `tools/.venv` and installs the pinned dependencies from
`uv.lock`. Requires Python 3.12+ (uv downloads it if not already installed).

## Scripts

### `integration_test.py`

End-to-end test: acts as a student (and a teacher) sending real DMs to a bot
instance running against a throwaway config. See the module docstring for
what it covers and what accounts it needs.

```bash
cp integration_config.example.json integration_config.json
# fill in integration_config.json with real API keys -- it is gitignored

uv run integration_test.py --config integration_config.json
```

The bot itself must already be running against its own throwaway config
(short deadline / reveal date, disposable database and master data) before
you start this script. As of the roster feature, that config's `roster.path`
must also point at a real CSV file — the bot now refuses to boot without one,
the same as it does for a missing `master_data.csv`. The roster's header is
`email,name,daily_limit,golden_bullets,max_files_per_submission` — all five columns
are required regardless of `competition.mode` (`max_files_per_submission` only
matters in kaggle mode, but the roster schema doesn't change with the mode).
The two student accounts in `integration_config.json` must be listed in that
roster CSV (their emails, lowercase) with a `daily_limit` (and, for kaggle
mode, `max_files_per_submission`) high enough to cover every submit the script
makes in one run, or the quota/file gates will reject them partway through.

**The script is mode-aware.** It reads `competition.mode` from the bot config
it's pointed at and runs the matching phase set automatically — blind
(single-CSV submit, `reveal`, the reveal-date gate) or kaggle (multi-CSV
submit, the public/private split, no separate pick step). To exercise both modes, run two
separate bot instances, each against its own throwaway config/database/roster
(one `blind`, one `kaggle`, the latter needing a `split` column on its
`master_data.csv`), and invoke this script once per instance with a
`--config` pointing at the matching `integration_config.json`.

### `exports_test.py`

Kaggle mode only. Covers the teacher exports `integration_test.py` doesn't:
the `public leaderboard` image, the `all submits` and `grades` CSV
attachments (it downloads each file and checks its contents), and the public
board's roster filtering. Uses the same `integration_config.json`.

```bash
uv run exports_test.py --config integration_config.json
```

Unlike `integration_test.py`, it never waits for the deadline, so the bot's
deadline can be far in the future — it only has to not have passed. Each run
makes 2 submits per student (with unique names, so it works against a
database that already has data), so each student needs 2 units of daily quota
left. To test roster filtering it **temporarily edits the bot's
`roster.csv`** (removes student A, runs `roster reload`), then restores the
file and reloads again, even if a check fails — so, as with the other script,
point it only at a throwaway config.
