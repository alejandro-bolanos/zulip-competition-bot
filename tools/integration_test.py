#!/usr/bin/env python3
"""End-to-end test: drives the bot through a real Zulip realm, as a student would.

This is the only layer that can validate the assumptions the bot makes about
Zulip's own output -- above all the attachment markdown link regex (including
the multi-attachment case in kaggle mode) and the `@**Name**` mention format,
none of which any unit test can reach.

The script is mode-aware: it reads `competition.mode` from the bot's own
config and runs whichever phase set matches (blind or kaggle). Point it at
whichever bot instance is currently running -- to exercise both modes, run
two bot instances (each with its own throwaway config/database/roster) and
invoke this script once per instance.

Requires (see tools/integration_config.example.json):
  * 1 bot account  -- the bot under test, running against a throwaway config
  * 2 student accounts -- must both be listed in the bot's roster.csv (by
    email, lowercase), with daily_limit and max_files_per_submission high enough
    to cover every submit this script makes in one run, or the quota/file
    gates reject them partway through. Being a bot account or a human account
    no longer matters (that constraint existed only for the old
    presence-based `no submits`, which is gone -- authorization is roster
    membership now).
  * 1 teacher account -- may be your own; must be listed in the bot's `teachers`

The bot's own config must use throwaway paths for `database`, `submissions`,
`master_data` and `roster`, and must set `deadline` / `results_reveal_date` a
few minutes out so this script can cross both gates in one run. For kaggle
mode, `master_data.csv` must additionally carry a `split` column (see
readme.md) -- this script reads it to know which ids are public vs. private.

Managed as a uv project in tools/ (pyproject.toml + uv.lock) -- `uv run` picks
up the pinned dependencies without a manual venv.

Usage (from the repo root):
  uv run --project tools integration_test.py --config integration_config.json

Or from inside tools/:
  uv run integration_test.py --config integration_config.json
"""

from __future__ import annotations

import argparse
import csv
import io
import json
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path

try:
    import requests
except ImportError:
    sys.exit("error: this script needs `requests` (pip install requests)")


# --------------------------------------------------------------------------
# Result accounting
# --------------------------------------------------------------------------


@dataclass
class Results:
    passed: int = 0
    failures: list[str] = field(default_factory=list)

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        if ok:
            self.passed += 1
            print(f"  PASS  {name}")
        else:
            self.failures.append(name)
            print(f"  FAIL  {name}")
            if detail:
                for line in detail.strip().splitlines():
                    print(f"        {line}")
        return ok

    def contains(self, name: str, haystack: str, needle: str) -> bool:
        return self.check(
            name, needle.lower() in haystack.lower(), f"expected {needle!r} in:\n{haystack}"
        )

    def excludes(self, name: str, haystack: str, needle: str) -> bool:
        return self.check(
            name,
            needle.lower() not in haystack.lower(),
            f"did NOT expect {needle!r} in:\n{haystack}",
        )


# --------------------------------------------------------------------------
# Zulip client (one per identity)
# --------------------------------------------------------------------------


class Identity:
    """A Zulip account this script can act as."""

    def __init__(self, site: str, email: str, api_key: str, display_name: str, label: str):
        self.site = site.rstrip("/")
        self.email = email
        # As Zulip renders it: what shows up in bot replies and in @**mentions**.
        self.display_name = display_name
        self.label = label
        self.auth = (email, api_key)
        self.session = requests.Session()

    def _url(self, path: str) -> str:
        return f"{self.site}/api/v1/{path}"

    def upload(self, filename: str, content: str) -> str:
        """Uploads a file and returns the URI to embed in a message.

        A human attaching a file in the Zulip client does exactly this, then
        sends a message containing a markdown link to the returned URI. The bot
        parses that link, so the test must reproduce both steps.
        """
        response = self.session.post(
            self._url("user_uploads"),
            auth=self.auth,
            files={"file": (filename, io.BytesIO(content.encode()), "text/csv")},
            timeout=30,
        )
        response.raise_for_status()
        body = response.json()
        # Field name differs across Zulip versions.
        uri = body.get("uri") or body.get("url")
        if not uri:
            raise RuntimeError(f"no uri in upload response: {body}")
        return uri

    def send_dm(self, to_email: str, content: str) -> int:
        response = self.session.post(
            self._url("messages"),
            auth=self.auth,
            data={"type": "private", "to": to_email, "content": content},
            timeout=30,
        )
        response.raise_for_status()
        body = response.json()
        if body.get("result") != "success":
            raise RuntimeError(f"send failed: {body}")
        return body["id"]

    def _fetch_dms(self, with_email: str, num_before: int = 10) -> list[dict]:
        # The narrow operator was renamed; try the modern one and fall back.
        for operator in ("dm", "pm-with"):
            params = {
                "anchor": "newest",
                "num_before": num_before,
                "num_after": 0,
                "narrow": json.dumps([{"operator": operator, "operand": with_email}]),
                "apply_markdown": "false",
            }
            response = self.session.get(
                self._url("messages"), auth=self.auth, params=params, timeout=30
            )
            if response.status_code == 400:
                continue
            response.raise_for_status()
            return response.json().get("messages", [])
        raise RuntimeError("could not narrow to the DM conversation")

    def await_reply(self, bot_email: str, after_id: int, timeout: float = 30.0) -> str:
        """Waits for the bot's next message in this DM and returns its content."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for message in self._fetch_dms(bot_email):
                if message["id"] > after_id and message["sender_email"] == bot_email:
                    return message["content"]
            time.sleep(1.0)
        raise TimeoutError(f"no reply from {bot_email} within {timeout}s")

    def ask(self, bot_email: str, content: str, timeout: float = 30.0) -> str:
        """Sends a DM and returns the bot's reply."""
        sent = self.send_dm(bot_email, content)
        return self.await_reply(bot_email, sent, timeout)

    def submit(
        self, bot_email: str, name: str, expected_gain: float, csv_body: str
    ) -> str:
        uri = self.upload(f"{name}.csv", csv_body)
        return self.ask(bot_email, f"submit {name} {expected_gain}\n\n[{name}.csv]({uri})")

    def submit_many(
        self, bot_email: str, name: str, candidates: dict[str, str]
    ) -> str:
        """Kaggle mode: one message, several attachments -- `candidates` maps
        a distinct filename to its CSV body. This is exactly what a human
        attaching multiple files to one Zulip message produces: several
        `[name](uri)` markdown links in one message body. No expected_gain --
        kaggle mode's `submit` doesn't take one, since the public gain is
        already shown in the very reply this call gets back.
        """
        links = [
            f"[{filename}]({self.upload(filename, body)})"
            for filename, body in candidates.items()
        ]
        content = f"submit {name}\n\n" + "\n".join(links)
        return self.ask(bot_email, content)


# --------------------------------------------------------------------------
# Test data derived from the bot's own master data
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class Fixtures:
    positives: list[int]
    negatives: list[int]
    unknown_id: int
    matrix: dict
    # Empty in blind mode, where master_data.csv has no `split` column.
    public_ids: list[int] = field(default_factory=list)
    private_ids: list[int] = field(default_factory=list)

    def prediction_ids(self, n_pos: int, n_neg: int) -> set[int]:
        return set(self.positives[:n_pos] + self.negatives[:n_neg])

    def prediction(self, n_pos: int, n_neg: int) -> str:
        return self.csv_from_ids(self.prediction_ids(n_pos, n_neg))

    @staticmethod
    def csv_from_ids(ids: set[int]) -> str:
        return "".join(f"{i}\n" for i in ids)

    def gain_over(self, predicted_ids: set[int], ids: list[int]) -> float:
        """Mirrors `calculate_gain_over` in src/submission.rs exactly: `ids`
        restricts which rows of the dataset count (the whole set in blind
        mode, one side of the split in kaggle mode), `positives` (never
        split-specific) is the ground truth."""
        positive = set(self.positives)
        tp = tn = fp = fn = 0
        for id_ in ids:
            is_pos = id_ in positive
            is_pred = id_ in predicted_ids
            if is_pos and is_pred:
                tp += 1
            elif is_pos:
                fn += 1
            elif is_pred:
                fp += 1
            else:
                tn += 1
        return (
            tp * self.matrix["tp"]
            + tn * self.matrix["tn"]
            + fp * self.matrix["fp"]
            + fn * self.matrix["fn_"]
        )

    def expected_gain(self, n_pos: int, n_neg: int) -> float:
        """Blind-mode gain: scored against the whole dataset."""
        return self.gain_over(self.prediction_ids(n_pos, n_neg), self.positives + self.negatives)

    def public_gain(self, predicted_ids: set[int]) -> float:
        return self.gain_over(predicted_ids, self.public_ids)

    def private_gain(self, predicted_ids: set[int]) -> float:
        return self.gain_over(predicted_ids, self.private_ids)


def load_fixtures(bot_config: dict, repo: Path) -> Fixtures:
    master_path = Path(bot_config["master_data"]["path"])
    if not master_path.is_absolute():
        master_path = repo / master_path

    positives: list[int] = []
    negatives: list[int] = []
    public_ids: list[int] = []
    private_ids: list[int] = []
    with master_path.open(newline="") as handle:
        reader = csv.reader(handle)
        header = next(reader, None)
        has_split = header is not None and len(header) >= 3
        for row in reader:
            if len(row) < 2:
                continue
            id_ = int(row[0])
            (positives if row[1].strip() == "1" else negatives).append(id_)
            if has_split and len(row) >= 3:
                split = row[2].strip().lower()
                if split == "public":
                    public_ids.append(id_)
                elif split == "private":
                    private_ids.append(id_)

    if not positives or not negatives:
        sys.exit(f"error: {master_path} needs both positive and negative rows")

    mode = bot_config.get("competition", {}).get("mode", "blind")
    if mode == "kaggle" and (not public_ids or not private_ids):
        sys.exit(
            f"error: competition.mode is 'kaggle' but {master_path} has no "
            "usable 'split' column (expected id,label,split)"
        )

    return Fixtures(
        positives=positives,
        negatives=negatives,
        unknown_id=max(positives + negatives) + 10_000,
        matrix=bot_config["gain_matrix"],
        public_ids=public_ids,
        private_ids=private_ids,
    )


def parse_gate(value: str, offset_minutes: int = 0) -> datetime:
    """Resolves a competition gate to an instant exactly as `BotConfig` does.

    A naive string is read at `offset_minutes` (minutes to ADD to UTC to get
    local time), NOT as UTC: the bot applies the offset, so reading these as
    UTC drifts every gate by it -- with the default -180, the script would
    think a deadline had already passed three hours before the bot does.
    A string carrying its own offset always wins, same as the bot.
    """
    for fmt in ("%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M:%S"):
        try:
            parsed = datetime.strptime(value.replace("Z", "+0000"), fmt)
        except ValueError:
            continue
        if parsed.tzinfo:
            return parsed
        return parsed.replace(tzinfo=timezone(timedelta(minutes=offset_minutes)))
    sys.exit(f"error: cannot parse competition date {value!r}")


def sleep_until(when: datetime, label: str) -> None:
    remaining = (when - datetime.now(timezone.utc)).total_seconds()
    if remaining <= 0:
        print(f"  (already past {label})")
        return
    print(f"  waiting {remaining:.0f}s for {label} ...")
    time.sleep(remaining + 3)


# --------------------------------------------------------------------------
# Phases
# --------------------------------------------------------------------------


def phase_before_deadline(
    r: Results, bot: str, a: Identity, b: Identity, teacher: Identity, fx: Fixtures
) -> None:
    print("\n== Before the deadline ==")

    reply = a.ask(bot, "help")
    r.contains("help reaches a student", reply, "Students")

    reply = a.ask(bot, "this is not a command")
    r.contains("unknown command is called out", reply, "don't recognize")

    # The scoring assertion: 5 true positives and 3 false positives.
    n_pos, n_neg = 5, 3
    reply = a.submit(bot, "model-a", 1234.0, fx.prediction(n_pos, n_neg))
    r.contains("submission accepted", reply, "Submission ID")
    r.excludes("student reply hides the real gain", reply, "Actual gain")

    reply = a.ask(bot, "list submits")
    r.contains("submission is listed", reply, "model-a")
    r.excludes("list submits hides the real gain before reveal", reply, "✨")
    r.contains("reveal date is announced", reply, "will be revealed")

    # Rejections.
    reply = a.submit(bot, "bad", 1.0, f"{fx.unknown_id}\n")
    r.contains("unknown ids rejected", reply, "Invalid IDs")

    reply = a.ask(bot, "submit no-file 100")
    r.contains("missing attachment rejected", reply, "attach")

    reply = a.ask(bot, "submit incomplete")
    r.contains("malformed command rejected", reply, "Incorrect format")

    # `no submits` must see B (who has not submitted) and not A (who has).
    reply = teacher.ask(bot, "no submits")
    r.contains("no submits lists the silent student", reply, b.display_name)
    r.excludes("no submits omits the student who submitted", reply, a.display_name)

    # Same file from a second account -> the only real duplicate case.
    b.submit(bot, "copia", 999.0, fx.prediction(n_pos, n_neg))
    reply = teacher.ask(bot, "duplicates")
    r.contains("duplicate detected across accounts", reply, "Checksum")
    r.contains("duplicate names the first account", reply, a.display_name)
    r.contains("duplicate names the second account", reply, b.display_name)

    # Teacher-visible gain is where end-to-end scoring gets verified: the
    # expected value comes from the closed form, not from the bot.
    expected = fx.expected_gain(n_pos, n_neg)
    reply = teacher.ask(bot, f"user submits @**{a.display_name}**")
    r.contains("teacher sees the submission", reply, "model-a")
    r.contains(f"scored gain is {expected:g} as computed here", reply, f"{expected:.2f}")

    reply = teacher.ask(bot, "leaderboard")
    r.contains("leaderboard lists both students", reply, a.display_name)
    r.contains("leaderboard includes the second student", reply, b.display_name)

    reply = teacher.ask(bot, "submit something 1")
    r.contains("teachers cannot submit", reply, "cannot submit")


def phase_after_deadline(
    r: Results, bot: str, a: Identity, teacher: Identity, fx: Fixtures
) -> None:
    print("\n== After the deadline ==")

    reply = a.submit(bot, "late", 1.0, fx.prediction(9, 1))
    r.contains("late submission is flagged", reply, "LATE SUBMISSION")

    reply = teacher.ask(bot, "leaderboard")
    r.excludes(
        "late submission does not set the ranking gain",
        reply,
        f"{fx.expected_gain(9, 1):.2f}",
    )


def phase_after_reveal(r: Results, bot: str, a: Identity) -> None:
    print("\n== After the reveal date ==")

    reply = a.ask(bot, "list submits")
    r.contains("student now sees the real gain column", reply, "✨")


# --------------------------------------------------------------------------
# Kaggle-mode phases
# --------------------------------------------------------------------------


def phase_kaggle_before_deadline(
    r: Results,
    bot: str,
    a: Identity,
    b: Identity,
    teacher: Identity,
    fx: Fixtures,
    a_max_files: int,
) -> None:
    print("\n== Kaggle mode: before the deadline ==")

    reply = a.ask(bot, "help")
    r.excludes("help does not offer reveal in kaggle mode", reply, "reveal")

    # Two candidates in one submission: deliberately different quality, so the
    # "best on public wins" rule has something to actually pick between.
    weak_ids = fx.prediction_ids(1, 0)
    strong_ids = fx.prediction_ids(6, 1)
    reply = a.submit_many(
        bot,
        "model-a",
        {"weak.csv": fx.csv_from_ids(weak_ids), "strong.csv": fx.csv_from_ids(strong_ids)},
    )
    r.contains("submission accepted", reply, "submission received")
    r.contains("reply reports 2 candidates", reply, "2 candidate model")
    r.excludes("reply never names an individual candidate's gain", reply, "actual gain")

    # Matched at the same precision the bot formats with ({:.4} in
    # process_kaggle_submit) -- a looser precision here could spuriously
    # match or miss depending on rounding at the truncation boundary.
    expected_public_mean = (fx.public_gain(weak_ids) + fx.public_gain(strong_ids)) / 2
    r.contains(
        "public mean matches the closed-form computation here",
        reply,
        f"{expected_public_mean:.4f}",
    )

    # A bad CSV anywhere in the batch rejects the whole submission -- nothing gets
    # stored, so this must not spend any quota either (implicitly exercised:
    # the daily limit set in integration_config covers a handful of submissions,
    # and a rejected one must not eat into that budget).
    reply = a.submit_many(
        bot,
        "bad",
        {"ok.csv": fx.csv_from_ids(weak_ids), "bad.csv": f"{fx.unknown_id}\n"},
    )
    r.contains("unknown id in one candidate rejects the whole submission", reply, "Invalid IDs")

    # Too many candidate files in one submission, per the roster's own configured
    # limit for this student -- read from roster.csv, not hardcoded, so this
    # stays correct regardless of how the fixture roster is set up.
    too_many = {f"c{i}.csv": fx.csv_from_ids(weak_ids) for i in range(a_max_files + 1)}
    reply = a.submit_many(bot, "many", too_many)
    r.contains("too many candidate files in one submission is rejected", reply, "Too many files")

    # B never submits in this phase -- exercises "no submits".
    reply = teacher.ask(bot, "no submits")
    r.contains("no submits lists the silent student", reply, b.display_name)
    r.excludes("no submits omits the student who submitted", reply, a.display_name)

    # The scoring assertion this whole mode exists for: with no separate pick
    # step, a plain submit is already this competitor's final entry, and the
    # leaderboard must show the PRIVATE gain of the best-on-PUBLIC candidate
    # in it -- not the weak candidate's, and not either candidate's public
    # score.
    reply = teacher.ask(bot, "leaderboard")
    r.contains("leaderboard lists the competitor after a plain submit", reply, a.display_name)
    expected_private = fx.private_gain(strong_ids)
    r.contains(
        "leaderboard shows the PRIVATE gain of the best-on-public candidate",
        reply,
        f"{expected_private:.2f}",
    )

    # A student's own `list submits` shows the batch's AGGREGATE public gain
    # (already known to them, same mean the submit reply itself showed) --
    # never an individual candidate's own score, and never the private gain
    # the leaderboard just displayed.
    reply = a.ask(bot, "list submits")
    r.contains(
        "list submits shows the batch's aggregate public mean",
        reply,
        f"{expected_public_mean:.2f}",
    )
    # Structural check, not a value comparison: on the real master dataset
    # weak_ids and strong_ids can coincidentally land on the same public
    # gain (e.g. if every id beyond the first happens to fall in the
    # PRIVATE split), which would make an "excludes this number" check
    # meaningless. Counting rows is coincidence-proof: aggregated by batch,
    # a 2-candidate submission is exactly one table row, never two.
    r.check(
        "list submits shows ONE row for the batch, not one per candidate",
        reply.count("|model-a|") == 1,
        f"expected exactly one '|model-a|' row in:\n{reply}",
    )
    r.excludes(
        "list submits never shows the private gain",
        reply,
        f"{expected_private:.2f}",
    )

    reply = a.ask(bot, "reveal model-b 1")
    r.contains("reveal is unavailable in kaggle mode", reply, "not available")

    reply = teacher.ask(bot, "submit something 1")
    r.contains("teachers cannot submit", reply, "cannot submit")


def phase_kaggle_after_deadline(r: Results, bot: str, a: Identity, fx: Fixtures) -> None:
    print("\n== Kaggle mode: after the deadline ==")

    late_ids = fx.prediction_ids(2, 0)
    reply = a.submit_many(bot, "late", {"late.csv": fx.csv_from_ids(late_ids)})
    r.contains("late submission is flagged", reply, "LATE SUBMISSION")


# --------------------------------------------------------------------------


def load_roster(bot_config: dict, repo: Path) -> dict[str, dict]:
    """Roster rows keyed by lowercased email, so the script can read
    per-competitor limits (e.g. max_files_per_submission) instead of hardcoding
    numbers that would silently drift from whatever the fixture roster says.
    """
    roster_path = Path(bot_config["roster"]["path"])
    if not roster_path.is_absolute():
        roster_path = repo / roster_path
    with roster_path.open(newline="") as handle:
        return {row["email"].strip().lower(): row for row in csv.DictReader(handle)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", default="tools/integration_config.json")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    config_path = Path(args.config)
    if not config_path.is_absolute():
        config_path = repo / config_path
    if not config_path.exists():
        sys.exit(f"error: {config_path} not found (see integration_config.example.json)")

    config = json.loads(config_path.read_text())

    bot_config_path = Path(config["bot_config"])
    if not bot_config_path.is_absolute():
        bot_config_path = repo / bot_config_path
    bot_config = json.loads(bot_config_path.read_text())

    site = config["site"]
    bot_email = bot_config["zulip"]["email"]
    mode = bot_config.get("competition", {}).get("mode", "blind")

    def identity(spec: dict, label: str) -> Identity:
        return Identity(site, spec["email"], spec["api_key"], spec["display_name"], label)

    student_a = identity(config["students"][0], "student-a")
    student_b = identity(config["students"][1], "student-b")
    teacher = identity(config["teacher"], "teacher")

    if teacher.email not in bot_config["teachers"]:
        sys.exit(f"error: {teacher.email} is not in the bot's `teachers` list")
    for student in (student_a, student_b):
        if student.email in bot_config["teachers"]:
            sys.exit(f"error: {student.email} is in `teachers`, so it cannot submit")

    fixtures = load_fixtures(bot_config, repo)
    offset = int(bot_config["competition"].get("timezone_offset_minutes", 0))
    deadline = parse_gate(bot_config["competition"]["deadline"], offset)
    reveal = parse_gate(bot_config["competition"]["results_reveal_date"], offset)

    now = datetime.now(timezone.utc)
    if deadline <= now:
        sys.exit(
            "error: the bot's deadline must be in the future (a few minutes "
            "out) so this run can cross it"
        )
    # Kaggle mode has no student-facing behaviour gated by the reveal date
    # (private gain never surfaces to a student, before or after it), so
    # only blind mode's run needs to actually wait for it.
    if mode == "blind" and reveal <= now:
        sys.exit(
            "error: the bot's results_reveal_date must be in the future (a "
            "few minutes out) so this run can cross it"
        )

    print(f"bot under test: {bot_email} at {site} (mode: {mode})")
    print(f"deadline {deadline.isoformat()} / reveal {reveal.isoformat()}")

    results = Results()

    if mode == "kaggle":
        roster = load_roster(bot_config, repo)
        a_row = roster.get(student_a.email.lower())
        if a_row is None:
            sys.exit(f"error: {student_a.email} is not in the bot's roster.csv")
        a_max_files = int(a_row["max_files_per_submission"])

        phase_kaggle_before_deadline(
            results, bot_email, student_a, student_b, teacher, fixtures, a_max_files
        )

        sleep_until(deadline, "the deadline")
        phase_kaggle_after_deadline(results, bot_email, student_a, fixtures)
    else:
        phase_before_deadline(results, bot_email, student_a, student_b, teacher, fixtures)

        sleep_until(deadline, "the deadline")
        phase_after_deadline(results, bot_email, student_a, teacher, fixtures)

        sleep_until(reveal, "the reveal date")
        phase_after_reveal(results, bot_email, student_a)

    print(f"\n{results.passed} passed, {len(results.failures)} failed")
    for name in results.failures:
        print(f"  - {name}")

    return 1 if results.failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
