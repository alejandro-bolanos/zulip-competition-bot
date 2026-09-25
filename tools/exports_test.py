#!/usr/bin/env python3
"""End-to-end test of the kaggle-mode teacher exports, against a real bot.

Covers what integration_test.py doesn't: the `public leaderboard` image, the
`all submits` and `grades` CSV attachments, teacher baselines (upload,
publish/hide, and that they're never ranked or graded), and the public
board's roster filtering. Downloads every file the bot uploads and checks its
contents.

Uses the same integration_config.json as integration_test.py. The bot must be
running in kaggle mode with its deadline still in the future (unlike
integration_test.py, this never waits for the deadline, so a far-off one is
fine). Each run makes 2 submits per student, so each needs 2 units of daily
quota left.

Side effect: to test roster filtering, this temporarily removes student A
from the bot's roster.csv and runs `roster reload`, then restores the file
and reloads again -- even if a check fails. Point it only at a throwaway
config, like integration_test.py.

Usage (from inside tools/):
  uv run exports_test.py --config ../tests/integration_config.json
"""

from __future__ import annotations

import argparse
import csv
import io
import json
import re
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import requests

from integration_test import Identity, Results, load_fixtures, parse_gate

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


def download(teacher: Identity, site: str, reply: str) -> bytes | None:
    """Fetches the file linked from a bot reply (`[name](url)`), as the teacher."""
    match = re.search(r"\]\(([^)]+)\)", reply)
    if match is None:
        return None
    url = match.group(1)
    if url.startswith("/"):
        url = site.rstrip("/") + url
    response = requests.get(url, auth=teacher.auth, timeout=30)
    response.raise_for_status()
    return response.content


def competitor_count(reply: str) -> int | None:
    match = re.search(r"\((\d+) competitors?", reply)
    return int(match.group(1)) if match else None


def baseline_count(reply: str) -> int:
    match = re.search(r"(\d+) baselines?\)", reply)
    return int(match.group(1)) if match else 0


def reply_id(reply: str, label: str) -> str | None:
    match = re.search(label + r"\*\*: `([^`]+)`", reply)
    return match.group(1) if match else None


def csv_rows(data: bytes) -> list[dict[str, str]]:
    return list(csv.DictReader(io.StringIO(data.decode())))


def phase_seed(r: Results, bot: str, a: Identity, b: Identity, fx, run: str) -> dict:
    print("\n== Seeding: two submits per student ==")

    def ids(n_pos: int, n_neg: int):
        return fx.prediction_ids(n_pos, n_neg)

    # A: a 5-candidate batch, then a weaker single-candidate batch LAST --
    # graded on the last one, drawn from the best one.
    a_specs = [(20, 0), (14, 3), (10, 6), (6, 10), (2, 14)]
    batches = {
        f"exports-a1-{run}": (a, {f"a{i}.csv": ids(p, n) for i, (p, n) in enumerate(a_specs)}),
        f"exports-a2-{run}": (a, {"a-last.csv": ids(1, 20)}),
        f"exports-b1-{run}": (b, {f"b{i}.csv": ids(p, n) for i, (p, n) in enumerate([(9, 1), (7, 2), (5, 4)])}),
        f"exports-b2-{run}": (b, {"b-last.csv": ids(16, 0)}),
    }
    student_batch_id = None
    for name, (who, candidates) in batches.items():
        reply = who.submit_many(bot, name, {f: fx.csv_from_ids(i) for f, i in candidates.items()})
        if not r.contains(f"{name} ({len(candidates)} candidates) accepted", reply, "submission received"):
            if "limit" in reply.lower():
                sys.exit("error: a student hit the daily submission limit; raise daily_limit or retry tomorrow")
        student_batch_id = student_batch_id or reply_id(reply, "Submission ID")
    return {name: len(candidates) for name, (_, candidates) in batches.items()} | {
        "_a_last_private": fx.private_gain(ids(1, 20)),
        "_student_batch_id": student_batch_id,
    }


def phase_public_board(r: Results, bot: str, a: Identity, teacher: Identity, site: str) -> int | None:
    print("\n== public leaderboard ==")

    reply = a.ask(bot, "public leaderboard")
    r.contains("students cannot run it (unknown command)", reply, "don't recognize")

    reply = teacher.ask(bot, "public leaderboard orden=mean")
    r.contains("an unknown option is named in the error", reply, "Unknown option `orden`")
    r.contains("the error still shows the usage line", reply, "Usage:")
    reply = teacher.ask(bot, "public leaderboard order=median")
    r.contains("a bad value is named in the error", reply, "`median`")

    count = None
    for args, label in [("", "default"), ("median=on", "median"), ("order=mean values=off", "mean, no values")]:
        reply = teacher.ask(bot, f"public leaderboard {args}".strip(), timeout=60)
        r.excludes(f"[{label}] reply never leaks an email", reply, "@")
        png = download(teacher, site, reply)
        r.check(f"[{label}] the attachment is a real PNG", png is not None and png[:8] == PNG_MAGIC, reply)
        if label == "default":
            count = competitor_count(reply)
            r.check("reply reports a competitor count", count is not None, reply)
    return count


def phase_all_submits(r: Results, bot: str, teacher: Identity, site: str, seeded: dict) -> None:
    print("\n== all submits (CSV) ==")

    reply = teacher.ask(bot, "all submits", timeout=60)
    r.check("reply is a file link, not a markdown table", ".csv" in reply and "|" not in reply, reply)
    data = download(teacher, site, reply)
    if not r.check("the CSV downloads", data is not None, reply):
        return

    rows = csv_rows(data)
    expected_columns = {
        "id", "user_email", "user_full_name", "submission_name", "timestamp",
        "batch_id", "candidates_in_batch", "expected_gain", "actual_gain",
        "public_gain", "private_gain", "tp", "tn", "fp", "fn", "positives_predicted",
        "threshold_category", "file_checksum", "file_path", "after_deadline", "used_golden_bullet",
        "is_baseline", "baseline_published",
    }
    r.check("every expected column is present", expected_columns <= set(rows[0]), str(list(rows[0])))

    for name, size in ((k, v) for k, v in seeded.items() if not k.startswith("_")):
        mine = [row for row in rows if row["submission_name"] == name]
        r.check(f"{name}: one CSV row per candidate ({size})", len(mine) == size, str(mine))
        r.check(
            f"{name}: every row reports candidates_in_batch={size}",
            all(row["candidates_in_batch"] == str(size) for row in mine),
            str([row["candidates_in_batch"] for row in mine]),
        )
        r.check(f"{name}: all rows share one batch_id", len({row["batch_id"] for row in mine}) == 1, str(mine))
        r.check(f"{name}: expected_gain is blank (kaggle never collects it)", all(row["expected_gain"] == "" for row in mine))
        r.check(f"{name}: file_path is filled in", all(row["file_path"] for row in mine))


def phase_baseline(
    r: Results, bot: str, a: Identity, teacher: Identity, site: str, fx, run: str, seeded: dict
) -> None:
    print("\n== baselines ==")
    name = f"exports-baseline-{run}"

    reply = teacher.ask(bot, "submit something")
    r.contains("a teacher's `submit` points them to `baseline`", reply, "baseline <name>")
    reply = a.ask(bot, f"baseline {name}")
    r.contains("students cannot upload a baseline (unknown command)", reply, "don't recognize")
    reply = teacher.ask(bot, "baseline")
    r.contains("a bare `baseline` shows its usage", reply, "Usage:")

    # Stronger than every student on both splits: if it were ever counted as
    # a competitor, it would take rank 1 and become the grades' max.
    candidates = {"strong.csv": fx.prediction_ids(40, 0), "weak.csv": fx.prediction_ids(3, 3)}
    links = [f"[{f}]({teacher.upload(f, fx.csv_from_ids(i))})" for f, i in candidates.items()]
    reply = teacher.ask(bot, f"baseline {name}\n\n" + "\n".join(links))
    r.contains("the baseline is stored", reply, "Baseline stored")
    r.contains("it starts hidden", reply, "Hidden from the public leaderboard")
    baseline_id = reply_id(reply, "Baseline ID")
    if not r.check("the reply gives its ID", baseline_id is not None, reply):
        return

    reply = teacher.ask(bot, "baseline list")
    r.check("`baseline list` shows it as hidden", baseline_id in reply and "hidden" in reply, reply)

    reply = teacher.ask(bot, "public leaderboard", timeout=60)
    competitors_before = competitor_count(reply)
    r.check("hidden: the public image has no baseline", baseline_count(reply) == 0, reply)

    if seeded.get("_student_batch_id"):
        reply = teacher.ask(bot, f"baseline publish {seeded['_student_batch_id']}")
        r.contains("a student's submission can't be published as a baseline", reply, "No baseline with ID")

    reply = teacher.ask(bot, f"baseline publish {baseline_id}")
    r.contains("publish succeeds", reply, "now shown")
    reply = teacher.ask(bot, "public leaderboard", timeout=60)
    r.check("published: the image counts 1 baseline", baseline_count(reply) == 1, reply)
    r.check(
        "published: the competitor count is unchanged",
        competitor_count(reply) == competitors_before,
        f"before: {competitors_before}\n{reply}",
    )
    png = download(teacher, site, reply)
    r.check("published: the attachment is a real PNG", png is not None and png[:8] == PNG_MAGIC, reply)

    reply = teacher.ask(bot, "leaderboard")
    line = next((l for l in reply.splitlines() if f"Baseline: {name}" in l), "")
    r.check("the private leaderboard lists it, unnumbered and marked shown", line.startswith("| — |") and "shown" in line, reply)
    r.check("it doesn't take position 1 from a student", "| 1 |" in reply, reply)

    reply = teacher.ask(bot, "all submits", timeout=60)
    data = download(teacher, site, reply)
    if r.check("the all-submits CSV downloads", data is not None, reply):
        mine = [row for row in csv_rows(data) if row["batch_id"] == baseline_id]
        r.check("the CSV has one row per baseline candidate", len(mine) == len(candidates), str(mine))
        r.check(
            "they're flagged is_baseline=yes, baseline_published=yes",
            all(row["is_baseline"] == "yes" and row["baseline_published"] == "yes" for row in mine),
            str(mine),
        )

    reply = teacher.ask(bot, "grades", timeout=60)
    data = download(teacher, site, reply)
    if r.check("the grades CSV downloads", data is not None, reply):
        grade_rows = csv_rows(data)
        r.check("the baseline isn't a grades row", all(row["email"] != teacher.email for row in grade_rows), str(grade_rows))
        r.check("the top STUDENT still gets 10", "10" in [row["grade"] for row in grade_rows], str(grade_rows))

    reply = teacher.ask(bot, f"baseline hide {baseline_id}")
    r.contains("hide succeeds", reply, "now hidden")
    reply = teacher.ask(bot, "public leaderboard", timeout=60)
    r.check("hidden again: gone from the public image", baseline_count(reply) == 0, reply)


def phase_grades(r: Results, bot: str, teacher: Identity, site: str) -> None:
    print("\n== grades (CSV) ==")

    reply = teacher.ask(bot, "grades", timeout=60)
    data = download(teacher, site, reply)
    if not r.check("the grades CSV downloads", data is not None, reply):
        return
    grades = [row["grade"] for row in csv_rows(data)]
    print(f"        grades: {grades}")
    # `f64::to_string()` never pads with trailing zeros; the old `{:.2}`
    # formatting always produced exactly two decimals (e.g. "10.00").
    r.check(
        "grades are written at full precision, not padded to 2 decimals",
        not any(re.fullmatch(r"-?\d+\.\d0", g) for g in grades),
        str(grades),
    )
    r.check("the top competitor gets exactly 10", "10" in grades, str(grades))


def phase_roster_filter(
    r: Results, bot: str, a: Identity, teacher: Identity, roster_path: Path, count_before: int | None
) -> None:
    print("\n== public leaderboard follows the roster ==")

    original = roster_path.read_text()
    try:
        roster_path.write_text(
            "".join(line for line in original.splitlines(keepends=True) if a.email.lower() not in line.lower())
        )
        reply = teacher.ask(bot, "roster reload")
        r.contains("the smaller roster is reloaded", reply, "reloaded")

        reply = teacher.ask(bot, "public leaderboard", timeout=60)
        count_after = competitor_count(reply) or 0
        r.check(
            "removing a student from the roster removes them from the image",
            count_before is not None and count_after == count_before - 1,
            f"before: {count_before}, after: {count_after}\n{reply}",
        )

        reply = teacher.ask(bot, "leaderboard")
        r.contains("the PRIVATE leaderboard still lists them", reply, a.display_name)
    finally:
        roster_path.write_text(original)
        teacher.ask(bot, "roster reload")
        print("  (roster restored)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", default="tools/integration_config.json")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    config_path = Path(args.config)
    if not config_path.is_absolute():
        config_path = (Path.cwd() / config_path) if (Path.cwd() / config_path).exists() else repo / config_path
    if not config_path.exists():
        sys.exit(f"error: {config_path} not found (see integration_config.example.json)")
    config = json.loads(config_path.read_text())

    bot_config_path = Path(config["bot_config"])
    if not bot_config_path.is_absolute():
        bot_config_path = repo / bot_config_path
    bot_config = json.loads(bot_config_path.read_text())

    if bot_config.get("competition", {}).get("mode") != "kaggle":
        sys.exit("error: this script needs a bot running in kaggle mode")
    offset = int(bot_config["competition"].get("timezone_offset_minutes", 0))
    if parse_gate(bot_config["competition"]["deadline"], offset) <= datetime.now(timezone.utc):
        sys.exit("error: the bot's deadline has passed, so no submission would count")

    roster_path = Path(bot_config["roster"]["path"])
    if not roster_path.is_absolute():
        roster_path = repo / roster_path

    site = config["site"]
    bot = bot_config["zulip"]["email"]

    def identity(spec: dict, label: str) -> Identity:
        return Identity(site, spec["email"], spec["api_key"], spec["display_name"], label)

    a = identity(config["students"][0], "student-a")
    b = identity(config["students"][1], "student-b")
    teacher = identity(config["teacher"], "teacher")
    fx = load_fixtures(bot_config, repo)
    run = str(int(time.time()))

    print(f"bot under test: {bot} at {site} (run {run})")
    r = Results()

    seeded = phase_seed(r, bot, a, b, fx, run)
    count = phase_public_board(r, bot, a, teacher, site)
    phase_baseline(r, bot, a, teacher, site, fx, run, seeded)
    phase_all_submits(r, bot, teacher, site, seeded)
    phase_grades(r, bot, teacher, site)

    print("\n== private leaderboard: last batch still wins ==")
    reply = teacher.ask(bot, "leaderboard")
    r.contains(
        "student A is graded on their LAST batch, not the best one drawn on the image",
        reply,
        f"{seeded['_a_last_private']:.2f}",
    )

    phase_roster_filter(r, bot, a, teacher, roster_path, count)

    print(f"\n{r.passed} passed, {len(r.failures)} failed")
    for name in r.failures:
        print(f"  - {name}")
    return 1 if r.failures else 0


if __name__ == "__main__":
    sys.exit(main())
