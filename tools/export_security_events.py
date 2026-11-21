#!/usr/bin/env python3
"""Read-only, bounded local export of explicitly allowlisted events.

This is NOT a complete SIEM or a reliable delivery system. The source log is
short-lived and may drop events. Never emit raw message, details, identities,
credentials, URLs, IPs or other columns from the database.
"""
import argparse
import json
import sqlite3
import sys
from pathlib import Path

EVENTS = {
    "login_lock_backend_unavailable": "denied",
    "logout_refresh_revoke_failed": "unknown",
    "account_session_revoke_failed": "unknown",
    "admin_rate_backend_unavailable": "denied",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", help="path to the local accounts SQLite database")
    parser.add_argument("--after", type=int, default=0, help="only IDs greater than this value")
    parser.add_argument("--limit", type=int, default=100, help="maximum source rows, 1 to 1000")
    args = parser.parse_args()
    if args.after < 0 or not 1 <= args.limit <= 1000:
        parser.error("--after must be nonnegative and --limit must be between 1 and 1000")
    db_path = Path(args.database).expanduser().resolve(strict=True)
    connection = sqlite3.connect(db_path.as_uri() + "?mode=ro", uri=True, timeout=2)
    try:
        rows = connection.execute(
            "SELECT id, timestamp, details FROM web_logs "
            "WHERE source = ? AND id > ? ORDER BY id ASC LIMIT ?",
            ("security_event", args.after, args.limit),
        )
        scanned_through = args.after
        for row_id, timestamp, details in rows:
            scanned_through = row_id
            fields = set((details or "").split(", "))
            for event, outcome in EVENTS.items():
                if f"event={event}" in fields and f"outcome={outcome}" in fields:
                    print(json.dumps({"schema": 1, "id": row_id, "timestamp": timestamp,
                                      "event": event, "outcome": outcome}, separators=(",", ":")))
                    break
        print(f"scanned_through={scanned_through}", file=sys.stderr)
    finally:
        connection.close()


if __name__ == "__main__":
    try:
        main()
    except (OSError, sqlite3.Error) as error:
        # Avoid printing paths and arbitrary database error messages.
        print(f"security event export failed: {type(error).__name__}", file=sys.stderr)
        sys.exit(1)
