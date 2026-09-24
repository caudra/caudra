#!/usr/bin/env python3
"""Compare Anthropic and OpenAI flagship models on token, cache, and wall-time efficiency.

Reads the Caudra session database read-only and prints a report as text, markdown or JSON.

Tables it reads
---------------
`usage_ledger`     Hourly aggregates keyed by (bucket_start, provider, model, cwd, purpose,
                   ephemeral, subscription). The only source of cost. Has NO main/subagent
                   dimension, which is why section D has to reconstruct roles elsewhere.
                   `priced_turns + unpriced_turns` is the turn count; `unpriced_turns` are
                   turns whose model had no price entry, so `cost` understates by an
                   unknown amount rather than by zero.
`model_usage`      Token totals per (session, model). Rows survive only as long as the
                   session does, so it can hold more than the ledger for old sessions and
                   less for swept ones. Section H2 reports the drift.
`sessions`         One row per session. `model` is "provider/model"; every other table
                   stores the bare model id, hence MAIN_MODEL below. `metadata` is JSON
                   holding the thinking level and a user-turn counter.
`main_history_items`      Main-agent transcript, one JSON payload per item, ordered by
                          `ordinal`. Reasoning items carry `duration_ms` and `source.model`.
`subagent_history_items`  The same for subagent transcripts, keyed additionally by
                          `subagent_id`.
`subagents`        One row per subagent run, with the tool call that spawned it.

Units, which are the easiest thing to get wrong here
----------------------------------------------------
Five different things could be called a "turn", and mixing them silently produces
plausible nonsense. This script keeps them apart and labels every column:

  block         One thinking block. Providers fragment reasoning differently, so this
                width rewards whoever splits into the most pieces.
  response      One `group_id`, i.e. one assistant reply / API round-trip.
  turn          One unit of requested work: a user turn for the main agent, a whole
                delegated run for a subagent. The only fair width for comparing models.
  ledger turn   One API round-trip as counted by `usage_ledger`. Close to `response`
                but sourced independently, so the two need not match exactly.
  user turn     `sessions.metadata.turns`, counting user messages only. Roughly 1/13th
                of a ledger turn in agentic work. NEVER divide tokens by this.

Known asymmetries
-----------------
Anthropic bills explicit cache writes and reports only uncached input in `input_tokens`.
OpenAI reports `cache_creation = 0` always and puts all non-cached input in `input_tokens`.
A hit rate computes for both, but its denominator does not mean the same thing, so the
report says so rather than presenting one number as comparable.

Wall-time is thinking-block streaming duration only. It excludes tool execution, network
and output streaming, and history items carry no timestamps, so true end-to-end turn
latency is not recoverable from this database at all.

All percentiles are nearest-rank (ceiling), not interpolated.
"""

import argparse
import json
import os
import sqlite3
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

if os.name == "nt":
    STATE_HOME = Path(os.environ.get("APPDATA", Path.home() / "AppData/Roaming"))
else:
    STATE_HOME = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
DEFAULT_DB = STATE_HOME / "caudra" / "caudra.sqlite"

DEFAULT_MODELS = ["claude-opus-5", "claude-fable-5-1", "gpt-5.6-sol"]
# Excludes title/compaction/goal traffic: short utility work, often on a cheaper model,
# which would otherwise drag every per-turn average toward zero.
DEFAULT_PURPOSE = "chat"

NO_CACHE_WRITE_PROVIDERS = ["openai"]
ROLE_ORDER = ["main", "subagent", "mixed"]
ROLE_VIEWS = ["main", "subagent", "combined"]

# Values found inside the JSON payloads of the history tables.
REASONING = "reasoning"
USER_TYPE = "user"
TOOL_CALL = "tool_call"
# A user item also exists for observations and synthetic injections; only origin "turn" is
# a real user message, and so only it starts a new turn.
TURN_ORIGIN = "turn"

BLOCK = "block"
RESPONSE = "response"
TURN = "turn"

# Subagents launched from one parent tool call run concurrently, so their durations would
# overlap rather than add. Section G sums them, which is only valid while fanout stays 1;
# an integrity check enforces that instead of leaving the assumption implicit.
SEQUENTIAL_FANOUT = 1

P50 = 50
P90 = 90
P95 = 95

# `sessions.model` is "provider/model" but every other table stores the bare id, so joins
# between them have to strip the prefix. Exact substr rather than LIKE, which would treat
# a model id containing % or _ as a pattern.
MAIN_MODEL = "substr(s.model, instr(s.model, '/') + 1)"

SESSION_HAS_SELF_SUBAGENT = "EXISTS (SELECT 1 FROM subagents sa WHERE sa.session_id = s.id AND sa.model = s.model)"

# Recovers the main/subagent split the ledger lacks, from `model_usage` instead.
#   subagent  the row's model is not the session's main model, so only subagents ran it
#   mixed     it is the main model AND the session delegated to that same model, so the
#             row fuses both roles and cannot be split at all
#   main      it is the main model and nothing delegated to it
# `mixed` is not a rounding error: it holds the majority of some models' tokens, so it is
# reported rather than folded into `main`.
ROLE_CASE = f"""CASE
    WHEN {MAIN_MODEL} <> mu.model THEN 'subagent'
    WHEN {SESSION_HAS_SELF_SUBAGENT} THEN 'mixed'
    ELSE 'main'
END"""


def placeholders(items):
    return ",".join("?" * len(items))


def percentile(values, p):
    """Nearest-rank: the smallest observed value at or above rank p, never interpolated.

    Interpolating would invent durations no turn actually took, and several of these
    distributions are small enough (n=9 for one slice) that the invented value would sit
    nowhere near a real observation.
    """
    if not values:
        return 0
    ordered = sorted(values)
    rank = max(1, -(-len(ordered) * p // 100))
    return ordered[rank - 1]


def mean(values):
    return sum(values) / len(values) if values else 0


def ratio(numerator, denominator):
    return numerator / denominator if denominator else 0.0


def billable_input(row):
    """Every input token the provider counted, cached or not.

    The three components are populated differently per provider (see the module docstring),
    so this total is comparable across providers even though its parts are not.
    """
    return row["input"] + row["cache_creation"] + row["cache_read"]


def total_tokens(row):
    return billable_input(row) + row["output"]


def cache_hit(row):
    return ratio(row["cache_read"], billable_input(row))


def zero_usage():
    return {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}


def add_usage(target, source):
    for key in zero_usage():
        target[key] += source[key]


def fmt_int(n):
    return f"{int(n):,}"


def fmt_pct(x):
    return f"{x:.1%}"


def fmt_usd(x):
    return f"${x:,.4f}" if 0 < abs(x) < 0.01 else f"${x:,.2f}"


def fmt_dur(ms):
    return f"{int(ms)}ms" if ms < 1000 else f"{ms / 1000:.1f}s"


def fmt_signed(n):
    return f"{int(n):+,}" if n else "0"


def connect(path):
    """Read-only, in autocommit mode so the caller can hold one explicit read snapshot."""
    if not path.exists():
        sys.exit(f"database not found: {path}")
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True, isolation_level=None)


def ledger_totals(conn, models, purpose, cwd):
    """Per-model token, cost and turn totals from the hourly usage ledger."""
    where = [f"model IN ({placeholders(models)})"]
    params = list(models)
    if purpose:
        where.append("purpose = ?")
        params.append(purpose)
    if cwd:
        where.append("cwd = ?")
        params.append(cwd)
    rows = conn.execute(
        f"""SELECT provider, model, SUM(input_tokens), SUM(output_tokens),
                   SUM(cache_creation), SUM(cache_read), SUM(cost),
                   SUM(priced_turns), SUM(unpriced_turns),
                   SUM(CASE WHEN subscription = 1 THEN priced_turns + unpriced_turns ELSE 0 END)
            FROM usage_ledger WHERE {" AND ".join(where)}
            GROUP BY provider, model""",
        params,
    ).fetchall()
    return [
        {
            "provider": provider,
            "model": model,
            "input": inp,
            "output": out,
            "cache_creation": cw,
            "cache_read": cr,
            "cost": cost,
            "priced_turns": priced,
            "unpriced_turns": unpriced,
            "turns": priced + unpriced,
            "subscription_turns": subscription,
        }
        for provider, model, inp, out, cw, cr, cost, priced, unpriced, subscription in rows
    ]


def with_derived(row):
    turns = row["turns"]
    return {
        **row,
        "total": total_tokens(row),
        "tokens_per_turn": ratio(total_tokens(row), turns),
        "output_per_turn": ratio(row["output"], turns),
        "cache_read_per_turn": ratio(row["cache_read"], turns),
        "cache_hit": cache_hit(row),
        "cache_miss": 1.0 - cache_hit(row),
        "cache_write": ratio(row["cache_creation"], billable_input(row)),
        "cost_per_turn": ratio(row["cost"], turns),
        "unpriced_share": ratio(row["unpriced_turns"], turns),
        "subscription_share": ratio(row["subscription_turns"], turns),
    }


def by_provider(ledger):
    grouped = {}
    for row in ledger:
        bucket = grouped.setdefault(
            row["provider"],
            {
                "provider": row["provider"],
                "model": ", ".join(
                    sorted(
                        r["model"] for r in ledger if r["provider"] == row["provider"]
                    )
                ),
                "cost": 0.0,
                "priced_turns": 0,
                "unpriced_turns": 0,
                "turns": 0,
                "subscription_turns": 0,
                **zero_usage(),
            },
        )
        add_usage(bucket, row)
        for key in (
            "cost",
            "priced_turns",
            "unpriced_turns",
            "turns",
            "subscription_turns",
        ):
            bucket[key] += row[key]
    return [with_derived(row) for row in grouped.values()]


def reasoning_events(conn, models, cwd):
    """One row per reasoning block, tagged with the role, response and turn it belongs to.

    `turn` is the unit of requested work: for the main agent a user turn, running until
    the next user message; for a subagent the whole delegated run. Both coalesce every
    round-trip the model needed, which is what makes the two providers comparable.
    """
    cwd_filter = "AND s.cwd = ?" if cwd else ""
    main_params = [USER_TYPE, TURN_ORIGIN, REASONING, *models]
    subagent_params = [REASONING, *models]
    if cwd:
        main_params.insert(0, cwd)
        subagent_params.append(cwd)
    # The running SUM over a 0/1 flag is a turn counter: it increments on each real user
    # message and holds steady across everything that follows, so every item carries the
    # number of the turn it belongs to. Filtering to reasoning items has to happen after
    # the window runs, otherwise the user messages that define the boundaries are gone.
    main = conn.execute(
        f"""WITH items AS (
                SELECT h.session_id, h.ordinal,
                       json_extract(h.payload, '$.type') AS kind,
                       json_extract(h.payload, '$.origin') AS origin,
                       json_extract(h.payload, '$.group_id') AS group_id,
                       json_extract(h.payload, '$.source.model') AS model,
                       json_extract(h.payload, '$.duration_ms') AS ms
                FROM main_history_items h JOIN sessions s ON s.id = h.session_id
                WHERE 1 = 1 {cwd_filter}
            ),
            marked AS (
                SELECT *, SUM(CASE WHEN kind = ? AND origin = ? THEN 1 ELSE 0 END)
                            OVER (PARTITION BY session_id ORDER BY ordinal) AS turn_no
                FROM items
            )
            SELECT session_id, model, group_id, turn_no, ms FROM marked
            WHERE kind = ? AND ms IS NOT NULL AND model IN ({placeholders(models)})""",
        main_params,
    ).fetchall()
    subagent = conn.execute(
        f"""SELECT h.session_id, json_extract(h.payload, '$.source.model'),
                   json_extract(h.payload, '$.group_id'), h.subagent_id,
                   json_extract(h.payload, '$.duration_ms')
            FROM subagent_history_items h JOIN sessions s ON s.id = h.session_id
            WHERE json_extract(h.payload, '$.type') = ?
              AND json_extract(h.payload, '$.source.model') IN ({placeholders(models)})
              AND json_extract(h.payload, '$.duration_ms') IS NOT NULL
              {cwd_filter}""",
        subagent_params,
    ).fetchall()
    return [
        {
            "role": role,
            "session_id": session,
            "model": model,
            "group": group,
            "turn": turn,
            "ms": ms,
        }
        for role, rows in (("main", main), ("subagent", subagent))
        for session, model, group, turn, ms in rows
    ]


def wall_time(events, models):
    """Thinking duration coalesced at three widths, per model and role.

    A block is one thinking block, a response is one `group_id`, and a turn is one unit
    of requested work. Models differ in how many blocks they emit per response and how
    many responses they spend per turn, so only the turn compares like with like.
    """
    blocks = defaultdict(list)
    responses = defaultdict(lambda: defaultdict(int))
    turns = defaultdict(lambda: defaultdict(int))
    for event in events:
        # Each event lands in its own role and again in "combined", so the combined view is
        # a real pooled distribution rather than an average of two summaries.
        for role in (event["role"], "combined"):
            key = (event["model"], role)
            blocks[key].append(event["ms"])
            responses[key][(event["session_id"], event["group"])] += event["ms"]
            # Role is part of the turn key because a main-agent turn number and a subagent
            # id can collide within one session, which would fuse two unrelated units.
            turns[key][(event["session_id"], event["role"], event["turn"])] += event[
                "ms"
            ]
    out = []
    for model in models:
        for role in ROLE_VIEWS:
            key = (model, role)
            if key not in blocks:
                continue
            for unit, durations in (
                (BLOCK, blocks[key]),
                (RESPONSE, list(responses[key].values())),
                (TURN, list(turns[key].values())),
            ):
                out.append(
                    {
                        "model": model,
                        "role": role,
                        "unit": unit,
                        "n": len(durations),
                        "p50": percentile(durations, P50),
                        "p95": percentile(durations, P95),
                        "mean": mean(durations),
                        "blocks_per_unit": ratio(len(blocks[key]), len(durations)),
                    }
                )
    return out


def role_tokens(conn, models, cwd):
    """Token totals split by whether the model ran as main agent or as a subagent.

    The ledger has no role dimension, so this comes from `model_usage`, where a row is
    attributable only when the session's main model differs from it. Sessions that ran
    a subagent on their own main model are reported as `mixed` and cannot be split.
    """
    cwd_filter = "AND s.cwd = ?" if cwd else ""
    params = [*models, cwd] if cwd else list(models)
    rows = conn.execute(
        f"""SELECT mu.model, {ROLE_CASE}, COUNT(*), SUM(mu.input_tokens), SUM(mu.output_tokens),
                   SUM(mu.cache_creation), SUM(mu.cache_read)
            FROM sessions s JOIN model_usage mu ON mu.session_id = s.id
            WHERE mu.model IN ({placeholders(models)}) {cwd_filter}
            GROUP BY 1, 2""",
        params,
    ).fetchall()
    per_model = defaultdict(int)
    parsed = []
    for model, role, sessions, inp, out, cw, cr in rows:
        row = {
            "model": model,
            "role": role,
            "sessions": sessions,
            "input": inp,
            "output": out,
            "cache_creation": cw,
            "cache_read": cr,
        }
        row["total"] = total_tokens(row)
        row["cache_hit"] = cache_hit(row)
        per_model[model] += row["total"]
        parsed.append(row)
    for row in parsed:
        row["share"] = ratio(row["total"], per_model[row["model"]])
    parsed.sort(key=lambda r: (models.index(r["model"]), ROLE_ORDER.index(r["role"])))
    return parsed


def session_distribution(conn, models, cwd):
    """Total tokens per session, the only genuinely per-session token distribution."""
    cwd_filter = "AND s.cwd = ?" if cwd else ""
    params = [*models, cwd] if cwd else list(models)
    rows = conn.execute(
        f"""SELECT mu.model,
                   mu.input_tokens + mu.output_tokens + mu.cache_creation + mu.cache_read
            FROM sessions s JOIN model_usage mu ON mu.session_id = s.id
            WHERE mu.model IN ({placeholders(models)}) {cwd_filter}""",
        params,
    ).fetchall()
    grouped = defaultdict(list)
    for model, total in rows:
        grouped[model].append(total)
    return [
        {
            "model": model,
            "sessions": len(grouped[model]),
            "p50": percentile(grouped[model], P50),
            "p95": percentile(grouped[model], P95),
            "mean": mean(grouped[model]),
        }
        for model in models
        if model in grouped
    ]


def bucket_distribution(conn, models, purpose, cwd):
    """Ledger buckets are hourly and average many turns, so these are smoothed averages."""
    where = [f"model IN ({placeholders(models)})", "priced_turns + unpriced_turns > 0"]
    params = list(models)
    if purpose:
        where.append("purpose = ?")
        params.append(purpose)
    if cwd:
        where.append("cwd = ?")
        params.append(cwd)
    rows = conn.execute(
        f"""SELECT model, input_tokens, output_tokens, cache_creation, cache_read,
                   priced_turns + unpriced_turns
            FROM usage_ledger WHERE {" AND ".join(where)}""",
        params,
    ).fetchall()
    per_turn = defaultdict(list)
    hits = defaultdict(list)
    turns_per_bucket = defaultdict(list)
    for model, inp, out, cw, cr, turns in rows:
        row = {"input": inp, "output": out, "cache_creation": cw, "cache_read": cr}
        per_turn[model].append(ratio(total_tokens(row), turns))
        turns_per_bucket[model].append(turns)
        if billable_input(row):
            hits[model].append(cache_hit(row))
    return [
        {
            "model": model,
            "buckets": len(per_turn[model]),
            "mean_turns_per_bucket": mean(turns_per_bucket[model]),
            "tokens_per_turn_p50": percentile(per_turn[model], P50),
            "tokens_per_turn_p95": percentile(per_turn[model], P95),
            "tokens_per_turn_mean": mean(per_turn[model]),
            "cache_hit_p50": percentile(hits[model], P50),
            "cache_hit_p95": percentile(hits[model], P95),
            "cache_hit_mean": mean(hits[model]),
        }
        for model in models
        if model in per_turn
    ]


def thinking_levels(conn, models, cwd, events):
    """Thinking level is a per-session last-value, never per turn, so this is coarse."""
    cwd_filter = "AND s.cwd = ?" if cwd else ""
    params = [*models, cwd] if cwd else list(models)
    rows = conn.execute(
        f"""SELECT s.id, {MAIN_MODEL}, json_extract(s.metadata, '$.thinking.kind'),
                   json_extract(s.metadata, '$.thinking.level'), json_extract(s.metadata, '$.turns')
            FROM sessions s WHERE {MAIN_MODEL} IN ({placeholders(models)}) {cwd_filter}""",
        params,
    ).fetchall()
    groups = {}
    session_key = {}
    for session_id, model, kind, level, turns in rows:
        label = "unset" if kind is None else (f"{kind}:{level}" if level else kind)
        key = (model, label)
        session_key[session_id] = key
        group = groups.setdefault(
            key,
            {
                "model": model,
                "thinking": label,
                "sessions": 0,
                "user_turns": 0,
                "ms": [],
                **zero_usage(),
            },
        )
        group["sessions"] += 1
        group["user_turns"] += turns or 0
    usage = conn.execute(
        f"""SELECT mu.session_id, mu.input_tokens, mu.output_tokens, mu.cache_creation,
                   mu.cache_read
            FROM sessions s JOIN model_usage mu ON mu.session_id = s.id
            WHERE mu.model = {MAIN_MODEL} AND mu.model IN ({placeholders(models)}) {cwd_filter}""",
        params,
    ).fetchall()
    for session_id, inp, out, cw, cr in usage:
        key = session_key.get(session_id)
        if key:
            add_usage(
                groups[key],
                {"input": inp, "output": out, "cache_creation": cw, "cache_read": cr},
            )
    for event in events:
        if event["role"] == "main":
            key = session_key.get(event["session_id"])
            if key:
                groups[key]["ms"].append(event["ms"])
    out = []
    for group in groups.values():
        out.append(
            {
                **{k: v for k, v in group.items() if k != "ms"},
                "total": total_tokens(group),
                "blocks": len(group["ms"]),
                "block_p50": percentile(group["ms"], P50),
                "block_p95": percentile(group["ms"], P95),
            }
        )
    out.sort(key=lambda r: (models.index(r["model"]), -r["sessions"]))
    return out


def session_workload(conn, models, cwd):
    """Thinking time per user turn, normalised per session and grouped by the session's model.

    Unlike every other section this counts whatever the session spent, including subagents
    on other models, because the question it answers is what a session led by this model
    costs in wall time. Session-level medians and the pooled per-turn rates disagree when
    the distribution is skewed, so both are reported.
    """
    cwd_filter = "AND s.cwd = ?" if cwd else ""
    # Parameter order follows the order the placeholders appear in the SQL text below:
    # the marked CTE, then main_think, tools, sub_think, then the model list and cwd.
    params = [USER_TYPE, TURN_ORIGIN, REASONING, TOOL_CALL, REASONING, *models]
    if cwd:
        params.append(cwd)
    # Note what is NOT filtered by model: the four CTEs measure whatever the session spent,
    # including subagents running a different model. Only the final WHERE restricts by
    # model, and it restricts the session's leader. Delegated time is a cost of choosing
    # that leader, so it belongs to it.
    rows = conn.execute(
        f"""WITH marked AS (
                SELECT h.session_id, h.ordinal,
                       json_extract(h.payload, '$.type') AS kind,
                       json_extract(h.payload, '$.group_id') AS group_id,
                       json_extract(h.payload, '$.duration_ms') AS ms,
                       SUM(CASE WHEN json_extract(h.payload, '$.type') = ?
                                 AND json_extract(h.payload, '$.origin') = ? THEN 1 ELSE 0 END)
                         OVER (PARTITION BY h.session_id ORDER BY h.ordinal) AS turn_no
                FROM main_history_items h
            ),
            turns AS (
                SELECT session_id, COUNT(DISTINCT turn_no) AS n FROM marked
                WHERE turn_no > 0 GROUP BY session_id
            ),
            main_think AS (
                SELECT session_id, SUM(ms) AS ms, COUNT(DISTINCT group_id) AS responses
                FROM marked WHERE kind = ? AND ms IS NOT NULL GROUP BY session_id
            ),
            tools AS (
                SELECT session_id, COUNT(*) AS calls FROM main_history_items
                WHERE json_extract(payload, '$.type') = ? GROUP BY session_id
            ),
            sub_think AS (
                SELECT session_id, SUM(json_extract(payload, '$.duration_ms')) AS ms
                FROM subagent_history_items
                WHERE json_extract(payload, '$.type') = ?
                  AND json_extract(payload, '$.duration_ms') IS NOT NULL
                GROUP BY session_id
            ),
            sub_runs AS (SELECT session_id, COUNT(*) AS runs FROM subagents GROUP BY session_id)
            SELECT {MAIN_MODEL}, t.n, COALESCE(mt.ms, 0), COALESCE(mt.responses, 0),
                   COALESCE(tc.calls, 0), COALESCE(st.ms, 0), COALESCE(sr.runs, 0)
            FROM sessions s
            JOIN turns t ON t.session_id = s.id
            LEFT JOIN main_think mt ON mt.session_id = s.id
            LEFT JOIN tools tc ON tc.session_id = s.id
            LEFT JOIN sub_think st ON st.session_id = s.id
            LEFT JOIN sub_runs sr ON sr.session_id = s.id
            WHERE {MAIN_MODEL} IN ({placeholders(models)}) {cwd_filter}""",
        params,
    ).fetchall()
    grouped = defaultdict(list)
    for model, turns, main_ms, responses, calls, sub_ms, runs in rows:
        grouped[model].append(
            {
                "turns": turns,
                "main_ms": main_ms,
                "sub_ms": sub_ms,
                "responses": responses,
                "calls": calls,
                "runs": runs,
            }
        )
    out = []
    for model in models:
        sessions = grouped.get(model)
        if not sessions:
            continue
        per_turn = [ratio(s["main_ms"] + s["sub_ms"], s["turns"]) for s in sessions]
        shares = [
            ratio(s["sub_ms"], s["main_ms"] + s["sub_ms"])
            for s in sessions
            if s["main_ms"] + s["sub_ms"]
        ]
        turns = sum(s["turns"] for s in sessions)
        out.append(
            {
                "model": model,
                "sessions": len(sessions),
                "user_turns": turns,
                "median_ms_per_turn": percentile(per_turn, P50),
                # Mean over sessions, not the pooled total: a handful of slow sessions pull
                # it far above the median, and that gap is the skew worth seeing.
                "mean_ms_per_turn": mean(per_turn),
                "p90_ms_per_turn": percentile(per_turn, P90),
                "main_ms_per_turn": ratio(sum(s["main_ms"] for s in sessions), turns),
                "sub_ms_per_turn": ratio(sum(s["sub_ms"] for s in sessions), turns),
                "mean_sub_share": mean(shares),
                "responses_per_turn": ratio(
                    sum(s["responses"] for s in sessions), turns
                ),
                "tools_per_turn": ratio(sum(s["calls"] for s in sessions), turns),
                "runs_per_turn": ratio(sum(s["runs"] for s in sessions), turns),
            }
        )
    return out


def cost_attribution(conn, models, cwd):
    """Notional cost per user turn, split by which model actually incurred it.

    A leader's blended cost hides delegation: one expensive subagent session can add a
    fifth to it while the leader itself never got slower or dearer. Splitting own from
    delegated is what makes two leaders comparable.

    Restricted to sessions that carry any cost at all, because the unpriced era would
    otherwise contribute turns with no dollars and halve every figure. That makes the
    denominator here narrower than section G1's, so the two do not divide.
    """
    cwd_filter = "AND s.cwd = ?" if cwd else ""
    params = [USER_TYPE, TURN_ORIGIN, *models]
    if cwd:
        params.append(cwd)
    rows = conn.execute(
        f"""WITH marked AS (
                SELECT h.session_id,
                       SUM(CASE WHEN json_extract(h.payload, '$.type') = ?
                                 AND json_extract(h.payload, '$.origin') = ? THEN 1 ELSE 0 END)
                         OVER (PARTITION BY h.session_id ORDER BY h.ordinal) AS turn_no
                FROM main_history_items h
            ),
            turns AS (
                SELECT session_id, COUNT(DISTINCT turn_no) AS n FROM marked
                WHERE turn_no > 0 GROUP BY session_id
            ),
            priced AS (
                SELECT session_id FROM model_usage GROUP BY session_id
                HAVING SUM(COALESCE(subscription_cost, 0) + COALESCE(cost, 0)) > 0
            )
            SELECT {MAIN_MODEL} AS leader, mu.model AS incurred_by,
                   COUNT(DISTINCT s.id) AS sessions,
                   SUM(COALESCE(mu.subscription_cost, 0) + COALESCE(mu.cost, 0)) AS notional,
                   (SELECT SUM(t2.n) FROM turns t2
                      JOIN sessions s2 ON s2.id = t2.session_id
                      JOIN priced p2 ON p2.session_id = s2.id
                     WHERE {MAIN_MODEL.replace("s.model", "s2.model")} = {MAIN_MODEL}) AS leader_turns
            FROM sessions s
            JOIN turns t ON t.session_id = s.id
            JOIN priced p ON p.session_id = s.id
            JOIN model_usage mu ON mu.session_id = s.id
            WHERE {MAIN_MODEL} IN ({placeholders(models)}) {cwd_filter}
              AND COALESCE(mu.subscription_cost, 0) + COALESCE(mu.cost, 0) > 0
            GROUP BY leader, incurred_by""",
        params,
    ).fetchall()
    totals = defaultdict(float)
    for leader, _, _, notional, _ in rows:
        totals[leader] += notional
    out = [
        {
            "leader": leader,
            "incurred_by": incurred_by,
            "kind": "own" if leader == incurred_by else "delegated",
            "sessions": sessions,
            "user_turns": leader_turns,
            "notional": notional,
            "per_user_turn": ratio(notional, leader_turns),
            "share": ratio(notional, totals[leader]),
        }
        for leader, incurred_by, sessions, notional, leader_turns in rows
    ]
    out.sort(key=lambda r: (models.index(r["leader"]), -float(r["notional"])))
    return out


def integrity(conn, models, ledger_scoped, roles):
    """Cross-checks that must hold, and reconciliations that expose silent undercounts."""
    lifetime = dict(
        conn.execute(
            f"""SELECT model, SUM(input_tokens + output_tokens + cache_creation + cache_read)
                FROM usage_ledger WHERE model IN ({placeholders(models)}) GROUP BY model""",
            models,
        ).fetchall()
    )
    tracked = dict(
        conn.execute(
            f"""SELECT model, SUM(input_tokens + output_tokens + cache_creation + cache_read)
                FROM model_usage WHERE model IN ({placeholders(models)}) GROUP BY model""",
            models,
        ).fetchall()
    )
    reconciliation = [
        {
            "model": model,
            "ledger_lifetime": lifetime.get(model, 0),
            "model_usage": tracked.get(model, 0),
            "delta": lifetime.get(model, 0) - tracked.get(model, 0),
            "delta_share": ratio(
                lifetime.get(model, 0) - tracked.get(model, 0), tracked.get(model, 0)
            ),
        }
        for model in models
        if model in lifetime or model in tracked
    ]

    checks = []
    stray_writes = conn.execute(
        f"""SELECT COUNT(*) FROM usage_ledger
            WHERE provider IN ({placeholders(NO_CACHE_WRITE_PROVIDERS)}) AND cache_creation > 0""",
        NO_CACHE_WRITE_PROVIDERS,
    ).fetchone()[0]
    checks.append(
        {
            "check": "openai rows report no cache writes",
            "detail": f"{stray_writes} row(s) with cache_creation > 0",
            "ok": stray_writes == 0,
        }
    )

    zero_turn = [row["model"] for row in ledger_scoped if row["turns"] == 0]
    checks.append(
        {
            "check": "every per-turn divisor is non-zero",
            "detail": ", ".join(zero_turn) or "all models have turns in scope",
            "ok": not zero_turn,
        }
    )

    # COALESCE to the subagent's own id so top-level runs, which have no parent, each form
    # their own group instead of collapsing into one giant NULL group and faking a huge
    # fanout. Section G's summing of subagent durations is only sound while this is 1.
    fanout = conn.execute(
        """SELECT COALESCE(MAX(fanout), 0) FROM (
               SELECT COUNT(*) AS fanout FROM subagents
               GROUP BY session_id, COALESCE(parent_tool_use_id, tool_use_id))"""
    ).fetchone()[0]
    checks.append(
        {
            "check": "subagents ran sequentially, so their durations sum",
            "detail": f"max {fanout} subagent(s) per parent tool call",
            "ok": fanout <= SEQUENTIAL_FANOUT,
        }
    )

    role_sum = defaultdict(int)
    for row in roles:
        role_sum[row["model"]] += row["total"]
    mismatched = [m for m, total in role_sum.items() if total != tracked.get(m, 0)]
    checks.append(
        {
            "check": "role buckets sum to model_usage totals",
            "detail": ", ".join(mismatched) or f"{len(role_sum)} model(s) reconcile",
            "ok": not mismatched,
        }
    )
    return {"checks": checks, "reconciliation": reconciliation}


def table(headers, aligns, rows):
    return {"headers": headers, "aligns": aligns, "rows": rows}


def combined_table(ledger):
    return table(
        [
            "Model",
            "Turns",
            "Total tokens",
            "Tok/turn",
            "Out/turn",
            "Cache rd/turn",
            "Hit",
            "Miss",
            "Write",
            "$/turn",
            "Unpriced",
            "Sub",
        ],
        ["<", ">", ">", ">", ">", ">", ">", ">", ">", ">", ">", ">"],
        [
            [
                row["model"],
                fmt_int(row["turns"]),
                fmt_int(row["total"]),
                fmt_int(row["tokens_per_turn"]),
                fmt_int(row["output_per_turn"]),
                fmt_int(row["cache_read_per_turn"]),
                fmt_pct(row["cache_hit"]),
                fmt_pct(row["cache_miss"]),
                fmt_pct(row["cache_write"]),
                fmt_usd(row["cost_per_turn"]),
                fmt_pct(row["unpriced_share"]),
                fmt_pct(row["subscription_share"]),
            ]
            for row in ledger
        ],
    )


def wall_table(rows):
    return table(
        ["Model", "Role", "Unit", "N", "p50", "p95", "Mean", "Blocks/unit"],
        ["<", "<", "<", ">", ">", ">", ">", ">"],
        [
            [
                row["model"],
                row["role"],
                row["unit"],
                fmt_int(row["n"]),
                fmt_dur(row["p50"]),
                fmt_dur(row["p95"]),
                fmt_dur(row["mean"]),
                f"{row['blocks_per_unit']:.2f}",
            ]
            for row in rows
        ],
    )


def role_table(rows):
    return table(
        [
            "Model",
            "Role",
            "Sessions",
            "Total tokens",
            "Share",
            "Input",
            "Output",
            "Cache write",
            "Cache read",
            "Hit",
        ],
        ["<", "<", ">", ">", ">", ">", ">", ">", ">", ">"],
        [
            [
                row["model"],
                row["role"],
                fmt_int(row["sessions"]),
                fmt_int(row["total"]),
                fmt_pct(row["share"]),
                fmt_int(row["input"]),
                fmt_int(row["output"]),
                fmt_int(row["cache_creation"]),
                fmt_int(row["cache_read"]),
                fmt_pct(row["cache_hit"]),
            ]
            for row in rows
        ],
    )


def session_table(rows):
    return table(
        ["Model", "Sessions", "p50", "p95", "mean"],
        ["<", ">", ">", ">", ">"],
        [
            [
                row["model"],
                fmt_int(row["sessions"]),
                fmt_int(row["p50"]),
                fmt_int(row["p95"]),
                fmt_int(row["mean"]),
            ]
            for row in rows
        ],
    )


def bucket_table(rows):
    return table(
        [
            "Model",
            "Buckets",
            "Turns/bucket",
            "Tok/turn p50",
            "Tok/turn p95",
            "Tok/turn mean",
            "Hit p50",
            "Hit p95",
            "Hit mean",
        ],
        ["<", ">", ">", ">", ">", ">", ">", ">", ">"],
        [
            [
                row["model"],
                fmt_int(row["buckets"]),
                fmt_int(row["mean_turns_per_bucket"]),
                fmt_int(row["tokens_per_turn_p50"]),
                fmt_int(row["tokens_per_turn_p95"]),
                fmt_int(row["tokens_per_turn_mean"]),
                fmt_pct(row["cache_hit_p50"]),
                fmt_pct(row["cache_hit_p95"]),
                fmt_pct(row["cache_hit_mean"]),
            ]
            for row in rows
        ],
    )


def thinking_table(rows):
    return table(
        [
            "Model",
            "Thinking",
            "Sessions",
            "User turns",
            "Total tokens",
            "Blocks",
            "Block p50",
            "Block p95",
        ],
        ["<", "<", ">", ">", ">", ">", ">", ">"],
        [
            [
                row["model"],
                row["thinking"],
                fmt_int(row["sessions"]),
                fmt_int(row["user_turns"]),
                fmt_int(row["total"]),
                fmt_int(row["blocks"]),
                fmt_dur(row["block_p50"]),
                fmt_dur(row["block_p95"]),
            ]
            for row in rows
        ],
    )


def workload_table(rows):
    return table(
        [
            "Model",
            "Sessions",
            "User turns",
            "Median s/turn",
            "Mean s/turn",
            "p90 s/turn",
            "Skew",
            "Main s/turn",
            "Sub s/turn",
            "Sub share",
            "Resp/turn",
            "Tools/turn",
            "Subruns/turn",
        ],
        ["<", ">", ">", ">", ">", ">", ">", ">", ">", ">", ">", ">", ">"],
        [
            [
                row["model"],
                fmt_int(row["sessions"]),
                fmt_int(row["user_turns"]),
                fmt_dur(row["median_ms_per_turn"]),
                fmt_dur(row["mean_ms_per_turn"]),
                fmt_dur(row["p90_ms_per_turn"]),
                f"{ratio(row['mean_ms_per_turn'], row['median_ms_per_turn']):.1f}x",
                fmt_dur(row["main_ms_per_turn"]),
                fmt_dur(row["sub_ms_per_turn"]),
                fmt_pct(row["mean_sub_share"]),
                f"{row['responses_per_turn']:.1f}",
                f"{row['tools_per_turn']:.1f}",
                f"{row['runs_per_turn']:.2f}",
            ]
            for row in rows
        ],
    )


def attribution_table(rows):
    return table(
        [
            "Leader",
            "Incurred by",
            "Kind",
            "Sessions",
            "User turns",
            "Notional",
            "$/user turn",
            "Share",
        ],
        ["<", "<", "<", ">", ">", ">", ">", ">"],
        [
            [
                row["leader"],
                row["incurred_by"],
                row["kind"],
                fmt_int(row["sessions"]),
                fmt_int(row["user_turns"]),
                fmt_usd(row["notional"]),
                fmt_usd(row["per_user_turn"]),
                fmt_pct(row["share"]),
            ]
            for row in rows
        ],
    )


def check_table(rows):
    return table(
        ["Check", "Result", "Detail"],
        ["<", "<", "<"],
        [
            [row["check"], "pass" if row["ok"] else "FAIL", row["detail"]]
            for row in rows
        ],
    )


def reconciliation_table(rows):
    return table(
        ["Model", "Ledger (lifetime)", "model_usage", "Delta", "Delta share"],
        ["<", ">", ">", ">", ">"],
        [
            [
                row["model"],
                fmt_int(row["ledger_lifetime"]),
                fmt_int(row["model_usage"]),
                fmt_signed(row["delta"]),
                fmt_pct(row["delta_share"]),
            ]
            for row in rows
        ],
    )


CAVEATS = [
    (
        "No per-turn token rows exist in this database. Token percentiles are per hourly "
        "ledger bucket or per session, as labelled, never per turn."
    ),
    (
        "Wall-time is thinking-block streaming duration, and excludes tool execution and "
        "network time. History items carry no timestamps, so end-to-end turn latency is not "
        "recoverable."
    ),
    (
        "Read wall-time at the `turn` width. Providers fragment thinking differently, so the "
        "`block` width rewards whoever splits its reasoning into the most pieces and says "
        "nothing about how long a unit of work took. The turn width has the smallest n."
    ),
    (
        "Cache accounting is not symmetric. Anthropic bills explicit cache writes and reports "
        "only uncached input; OpenAI reports no cache writes and puts all non-cached input in "
        "input tokens. The hit rate computes, but its denominator means different things per "
        "provider."
    ),
    (
        "The usage ledger has no main/subagent dimension. Token role attribution comes from "
        "model_usage and only resolves when a session's main model differs from its "
        "subagent's; everything else is reported as mixed."
    ),
    (
        "Cost is notional. Every turn here was covered by a subscription, so no dollar was "
        "invoiced; the figure is what the usage would have cost at API rates, which is a "
        "efficiency proxy rather than spend. Read the `Sub` column to confirm."
    ),
    (
        "Unpriced turns are turns whose model had no price entry, so even the notional cost "
        "is a floor. One model here is around 60% unpriced, so its dollar figure understates "
        "by an unknown amount and is not comparable with a fully priced model's."
    ),
    (
        "Thinking level is stored per session as a last-value, never per turn and never in "
        "the ledger, so its breakdown is coarse and small-n."
    ),
    (
        "Roles are close to inverted between the models compared here, which is likely a "
        "larger effect than any model difference. Read the role split before the combined view."
    ),
    (
        "Section G is normalised per session and every other section is not, so its numbers "
        "are not comparable with theirs. Prefer its median to its pooled per-turn columns: a "
        "few slow sessions move the pooled figure enough to reverse the ranking."
    ),
]


def build_report(conn, models, purpose, cwd):
    ledger = [with_derived(row) for row in ledger_totals(conn, models, purpose, cwd)]
    ledger.sort(key=lambda r: models.index(r["model"]))
    events = reasoning_events(conn, models, cwd)
    walls = wall_time(events, models)
    roles = role_tokens(conn, models, cwd)
    sessions = session_distribution(conn, models, cwd)
    buckets = bucket_distribution(conn, models, purpose, cwd)
    thinking = thinking_levels(conn, models, cwd, events)
    workload = session_workload(conn, models, cwd)
    attribution = cost_attribution(conn, models, cwd)
    audit = integrity(conn, models, ledger, roles)
    return {
        "checks": audit["checks"],
        "scope": {
            "generated": datetime.now(timezone.utc).astimezone().date().isoformat(),
            "models": models,
            "purpose": purpose or "all",
            "cwd": cwd or "all",
            "turns_in_scope": {row["model"]: row["turns"] for row in ledger},
        },
        "caveats": CAVEATS,
        "sections": [
            {
                "title": "A. Combined efficiency",
                "note": "Every role pooled, from the hourly usage ledger. `Sub` is the share of "
                "turns covered by a subscription rather than invoiced.",
                "data": ledger,
                "table": combined_table(ledger),
            },
            {
                "title": "B. Provider rollup",
                "note": "The same columns aggregated to the provider.",
                "data": by_provider(ledger),
                "table": combined_table(by_provider(ledger)),
            },
            {
                "title": "C. Thinking wall-time by role and coalescing width",
                "note": "The only metric here with genuine event-level percentiles. `block` is "
                "one thinking block, `response` coalesces the blocks of one `group_id`, and "
                "`turn` coalesces every response in one unit of requested work: a user turn "
                "for the main agent, a whole delegated run for a subagent. Models differ in "
                "both ratios, so only `turn` compares like with like; `block` flatters whoever "
                "fragments its thinking most.",
                "data": walls,
                "table": wall_table(walls),
            },
            {
                "title": "D. Tokens and cache by role",
                "note": "From `model_usage`, not the ledger. `mixed` is usage that cannot be "
                "attributed because the session ran a subagent on its own main model. Compare "
                "only within a role.",
                "data": roles,
                "table": role_table(roles),
            },
            {
                "title": "E1. Total tokens per session",
                "note": "Unit is the session, not the turn. Session length varies, so this "
                "measures how the models were used as much as how they behave.",
                "data": sessions,
                "table": session_table(sessions),
            },
            {
                "title": "E2. Per hourly ledger bucket",
                "note": "Unit is the hourly bucket. Buckets average many turns, so these "
                "percentiles describe hour-to-hour variation in average turn size, not turn "
                "variance.",
                "data": buckets,
                "table": bucket_table(buckets),
            },
            {
                "title": "F. Thinking level",
                "note": "Grouped by the session's last thinking level. Tokens count only the "
                "session's own main model; wall-time counts only main-agent blocks. `User "
                "turns` counts user messages, which is a different unit from the API "
                "round-trips every other section calls a turn, so the two never divide.",
                "data": thinking,
                "table": thinking_table(thinking),
            },
            {
                "title": "G1. Session workload decomposition",
                "note": "Per session, then summarised across sessions, grouped by the model "
                "that led the session. Counts everything the session spent including "
                "subagents on other models, because this asks what a session led by this "
                "model costs in wall time. The median and the pooled per-turn columns "
                "disagree when the distribution is skewed, which is the point of showing "
                "both: read `p90 s/turn` for what a bad turn feels like.",
                "data": workload,
                "table": workload_table(workload),
            },
            {
                "title": "G2. Notional cost per user turn, by who incurred it",
                "note": "Splits each leader's cost into what the leader itself spent and what "
                "it delegated away. A blended figure flatters or penalises a leader for the "
                "company it kept: compare the `own` rows to compare the models. Priced "
                "sessions only, so the denominator is narrower than G1's and the two do not "
                "divide into each other.",
                "data": attribution,
                "table": attribution_table(attribution),
            },
            {
                "title": "H1. Integrity checks",
                "note": "Invariants this report depends on.",
                "data": audit["checks"],
                "table": check_table(audit["checks"]),
            },
            {
                "title": "H2. Ledger reconciliation",
                "note": "Lifetime ledger totals against `model_usage`. A negative delta means "
                "the ledger holds less than the sessions do, so section A undercounts that "
                "model relative to section D.",
                "data": audit["reconciliation"],
                "table": reconciliation_table(audit["reconciliation"]),
            },
        ],
    }


def render_text(report):
    out = [f"Model efficiency  ({report['scope']['generated']})"]
    scope = report["scope"]
    out.append(f"scope: purpose={scope['purpose']}  cwd={scope['cwd']}")
    out.append("")
    for section in report["sections"]:
        out.append(section["title"])
        out.append("-" * len(section["title"]))
        spec = section["table"]
        headers, aligns, rows = spec["headers"], spec["aligns"], spec["rows"]
        if not rows:
            out.extend(["(no rows)", ""])
            continue
        widths = [
            max(len(headers[i]), max(len(row[i]) for row in rows))
            for i in range(len(headers))
        ]
        out.append(
            "  ".join(
                f"{h:{a}{w}}" for h, a, w in zip(headers, aligns, widths)
            ).rstrip()
        )
        for row in rows:
            out.append(
                "  ".join(
                    f"{c:{a}{w}}" for c, a, w in zip(row, aligns, widths)
                ).rstrip()
            )
        out.append("")
    out.append("Caveats")
    out.append("-------")
    out.extend(f"- {c}" for c in report["caveats"])
    return "\n".join(out)


def render_markdown(report):
    scope = report["scope"]
    out = [
        "# Model efficiency: Anthropic flagships vs OpenAI Sol",
        "",
        (
            f"Snapshot: {scope['generated']}. Scope: `purpose={scope['purpose']}`, "
            f"`cwd={scope['cwd']}`, models {', '.join(f'`{m}`' for m in scope['models'])}."
        ),
        "",
        "Turns in scope: "
        + ", ".join(f"`{m}` {fmt_int(t)}" for m, t in scope["turns_in_scope"].items())
        + ".",
        "",
        "Regenerate with:",
        "",
        "```sh",
        "python3 scripts/model_efficiency.py --format markdown > docs/model-efficiency.md",
        "```",
        "",
        "## Read this first",
        "",
    ]
    out.extend(f"- {c}" for c in report["caveats"])
    for section in report["sections"]:
        out.extend(["", f"## {section['title']}", "", section["note"], ""])
        spec = section["table"]
        if not spec["rows"]:
            out.append("_No rows._")
            continue
        out.append("| " + " | ".join(spec["headers"]) + " |")
        out.append(
            "|" + "|".join("---:" if a == ">" else "---" for a in spec["aligns"]) + "|"
        )
        out.extend("| " + " | ".join(row) + " |" for row in spec["rows"])
    return "\n".join(out) + "\n"


def render_json(report):
    return json.dumps(
        {
            "scope": report["scope"],
            "caveats": report["caveats"],
            "checks": report["checks"],
            "sections": [
                {"title": s["title"], "note": s["note"], "data": s["data"]}
                for s in report["sections"]
            ],
        },
        indent=2,
    )


def parse_args():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument(
        "--db",
        type=Path,
        default=DEFAULT_DB,
        help=f"session database (default: {DEFAULT_DB})",
    )
    p.add_argument(
        "--models",
        default=",".join(DEFAULT_MODELS),
        help=f"comma-separated model ids (default: {','.join(DEFAULT_MODELS)})",
    )
    p.add_argument(
        "--purpose", default=DEFAULT_PURPOSE, help="ledger purpose, empty for all"
    )
    p.add_argument("--cwd", default="", help="restrict to one project directory")
    p.add_argument("--format", choices=["text", "markdown", "json"], default="text")
    return p.parse_args()


def main():
    args = parse_args()
    models = [m.strip() for m in args.models.split(",") if m.strip()]
    conn = connect(args.db)
    try:
        # One read snapshot for the whole report. The database is usually live, and the
        # integrity checks compare results from queries issued seconds apart; without this
        # they compare two different states of the world and fail at random.
        conn.execute("BEGIN")
        report = build_report(conn, models, args.purpose, args.cwd)
        conn.execute("ROLLBACK")
    finally:
        conn.close()
    renderers = {"text": render_text, "markdown": render_markdown, "json": render_json}
    print(renderers[args.format](report))
    return 1 if any(not check["ok"] for check in report["checks"]) else 0


if __name__ == "__main__":
    sys.exit(main())
