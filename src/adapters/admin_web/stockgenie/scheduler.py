"""收盤後寫日報到 cache/reports。Telegram／Webhook 可選。"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPORTS = ROOT / "cache" / "reports"
REPORTS.mkdir(parents=True, exist_ok=True)
TAIPEI = timezone(timedelta(hours=8))


def write_daily_report(market: str, rows: list[dict], regime: dict | None) -> Path:
    market = (market or "TW").upper()
    now = datetime.now(TAIPEI).strftime("%Y-%m-%d %H:%M")
    buys = [r for r in rows if r.get("verdict_key") in ("buy", "plan", "buy_breakout")]
    reso = [r for r in rows if r.get("resonance")]
    payload = {
        "market": market,
        "generated": now,
        "regime": regime,
        "n": len(rows),
        "actionable": buys,
        "resonance": reso,
    }
    path = REPORTS / f"{market}_{datetime.now(TAIPEI).strftime('%Y%m%d')}.json"
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, default=str))
    latest = REPORTS / f"{market}_latest.json"
    latest.write_text(path.read_text())
    return path
