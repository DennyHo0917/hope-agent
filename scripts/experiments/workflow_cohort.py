#!/usr/bin/env python3
"""FTR-004: synthetic-only local cohort PoC, deliberately outside default CI.

There is no file/database input option: this program never opens the product DB.
A template ID proves reuse of a saved definition, not equal input or user value.
Weeks start Monday 00:00 Asia/Shanghai. Last-week return is right-censored.
"""

import datetime as dt
import json
import sqlite3
from collections import Counter

ZONE = dt.timezone(dt.timedelta(hours=8))
STATES = ("completed", "failed", "blocked", "cancelled")


def ratio(numerator, denominator):
    return numerator / denominator if denominator else None


def cohorts(db, project, start, weeks):
    """Aggregate metadata only, with explicit template AND session scope checks."""
    if start.weekday() != 0 or weeks < 1:
        raise ValueError("A complete Monday-based observation window is required")
    active = [set() for _ in range(weeks)]
    states = [Counter() for _ in range(weeks)]
    linked = Counter()
    accepted = Counter()
    # Historical activity remains visible after disabling a saved template.
    # No join to the template's original source run: it can have been deleted.
    rows = db.execute("""
        SELECT t.id, w.created_at, w.status, g.id, g.accepted
        FROM workflow_runs w
        JOIN sessions s ON s.id = w.session_id
        JOIN workflow_templates t ON w.origin = 'template:' || t.id
        LEFT JOIN goals g ON g.id = w.goal_id AND g.session_id = s.id
        WHERE s.project_id = ? AND t.project_id = s.project_id
          AND s.incognito = 0 AND s.is_cron = 0 AND s.parent_session_id IS NULL
    """, (project,))
    for template, created, status, goal, goal_accepted in rows:
        day = dt.datetime.fromisoformat(created).astimezone(ZONE).date()
        week = (day - start).days // 7
        if not 0 <= week < weeks:
            continue
        active[week].add(template)
        if status in STATES:
            states[week][status] += 1
        if status == "completed" and goal is not None:
            linked[week] += 1
            accepted[week] += int(goal_accepted == 1)
    output = []
    for week in range(weeks):
        completed = states[week]["completed"]
        return_count = len(active[week] & active[week + 1]) if week + 1 < weeks else None
        output.append({
            "week": str(start + dt.timedelta(weeks=week)),
            "activeTemplates": len(active[week]),
            "nextWeekReturnNumerator": return_count,
            "nextWeekReturnDenominator": len(active[week]) if return_count is not None else None,
            "nextWeekReturn": ratio(return_count, len(active[week])) if return_count is not None else None,
            **{state: states[week][state] for state in STATES},
            "goalLinked": linked[week],
            "goalAccepted": accepted[week],
            "goalLinkedCoverage": ratio(linked[week], completed),
            "goalAcceptedCoverage": ratio(accepted[week], completed),
        })
    return output


def fixture():
    db = sqlite3.connect(":memory:")
    db.executescript("""
        CREATE TABLE sessions(id TEXT PRIMARY KEY, project_id TEXT, incognito INTEGER,
            is_cron INTEGER, parent_session_id TEXT);
        CREATE TABLE workflow_templates(id TEXT PRIMARY KEY, project_id TEXT,
            enabled INTEGER, source_run_id TEXT);
        CREATE TABLE goals(id TEXT PRIMARY KEY, session_id TEXT, accepted INTEGER);
        CREATE TABLE workflow_runs(id TEXT PRIMARY KEY, session_id TEXT, origin TEXT,
            created_at TEXT, status TEXT, goal_id TEXT);
        INSERT INTO sessions VALUES ('visible','project-a',0,0,NULL),
            ('private','project-a',1,0,NULL), ('other','project-b',0,0,NULL),
            ('cron','project-a',0,1,NULL), ('child','project-a',0,0,'visible');
        INSERT INTO workflow_templates VALUES ('useful','project-a',1,'deleted-source'),
            ('once','project-a',0,NULL), ('failure','project-a',1,NULL),
            ('foreign','project-b',1,NULL);
        INSERT INTO goals VALUES ('accepted','visible',1), ('rejected','visible',0),
            ('foreign-goal','other',1);
    """)
    start = dt.date(2026, 7, 20)
    serial = 0

    def add(template, week, status, goal=None, session="visible", stamp=None):
        nonlocal serial
        serial += 1
        stamp = stamp or dt.datetime.combine(start + dt.timedelta(weeks=week), dt.time(), ZONE).isoformat()
        db.execute("INSERT INTO workflow_runs VALUES (?,?,?,?,?,?)",
                   (str(serial), session, "template:" + template, stamp, status, goal))

    for week in range(8):
        add("useful", week, "completed", "accepted" if week % 2 == 0 else None)
        add("failure", week, "failed" if week % 2 == 0 else "blocked")
    add("once", 0, "cancelled")
    db.commit()
    return db, start, add


def self_test():
    db, start, add = fixture()
    baseline = cohorts(db, "project-a", start, 8)
    assert [r["activeTemplates"] for r in baseline] == [3] + [2] * 7
    assert [r["nextWeekReturn"] for r in baseline] == [2 / 3] + [1.0] * 6 + [None]
    assert baseline[7]["nextWeekReturnDenominator"] is None
    assert sum(r["completed"] for r in baseline) == 8
    assert sum(r["failed"] + r["blocked"] for r in baseline) == 8
    assert sum(r["cancelled"] for r in baseline) == 1
    assert [r["goalAcceptedCoverage"] for r in baseline] == [1.0, 0.0] * 4
    for session in ("private", "other", "cron", "child", "missing"):
        add("useful", 0, "completed", "accepted", session=session)
    add("foreign", 0, "completed", "accepted")  # Foreign template in an otherwise visible session.
    assert cohorts(db, "project-a", start, 8) == baseline
    assert all(r["nextWeekReturn"] is None and r["goalAcceptedCoverage"] is None
               for r in cohorts(db, "empty-project", start, 8))
    add("useful", 1, "completed", "foreign-goal")
    assert cohorts(db, "project-a", start, 8)[1]["goalLinked"] == 0
    # Sunday 23:59:59 UTC+8 still belongs to week zero; Monday 00:00 to week one.
    add("once", 0, "cancelled", stamp="2026-07-26T15:59:59+00:00")
    assert cohorts(db, "project-a", start, 8)[0]["cancelled"] == 2
    add("once", 1, "cancelled", stamp="2026-07-26T16:00:00+00:00")
    assert cohorts(db, "project-a", start, 8)[0]["nextWeekReturn"] == 1.0
    # A known, linked Goal that was not accepted never increases accepted coverage.
    add("useful", 2, "completed", "rejected")
    row = cohorts(db, "project-a", start, 8)[2]
    assert row["goalLinkedCoverage"] == 1.0 and row["goalAcceptedCoverage"] == 0.5
    db.close()
    return {"evidence": "synthetic-only", "weeks": baseline,
            "checks": ["exact-return-cohorts", "right-censoring", "terminal-states",
                       "goal-coverage", "incognito-and-scope", "cron-and-child",
                       "disabled-template", "deleted-source-run", "zero-denominator",
                       "timezone-boundary", "foreign-goal", "rejected-goal"],
            "productionDataRead": False, "productValueValidated": False}


if __name__ == "__main__":
    print(json.dumps(self_test(), ensure_ascii=False, indent=2))
