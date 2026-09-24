"""每日潛力股：只抓最新一盤 MI_INDEX 挑約 30 檔，不下載全市場歷史。隔日（新交易日）換一批。"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

from paths import cache_dir, ensure_dir

TAIPEI = timezone(timedelta(hours=8))
DAILY_N = 30
ANCHORS = ("2330.TW", "2317.TW", "2454.TW", "2308.TW", "2382.TW", "2881.TW")
PICKS_FILE = "daily_picks.json"


def _num(x) -> float:
    try:
        return float(x)
    except (TypeError, ValueError):
        return 0.0


def _latest_session() -> tuple[str | None, dict]:
    from data import load_twse_session_bars

    today = datetime.now(TAIPEI).date()
    d = today
    for _ in range(14):
        if d.weekday() < 5:
            got = load_twse_session_bars(d.strftime("%Y%m%d"))
            if got:
                return d.strftime("%Y%m%d"), got
        d -= timedelta(days=1)
    return None, {}


def _rank(rows: list[dict], key: str) -> None:
    ordered = sorted(rows, key=lambda r: r[key])
    n = max(len(ordered) - 1, 1)
    for i, r in enumerate(ordered):
        r[f"{key}_rk"] = i / n


def _score_session(bars: dict) -> list[dict]:
    from data import load_twse_industry_map

    industry = {}
    try:
        industry = load_twse_industry_map() or {}
    except Exception:
        industry = {}
    rows: list[dict] = []
    for code, rec in bars.items():
        if not rec:
            continue
        close = _num(rec.get("close"))
        open_px = _num(rec.get("open")) or close
        high = _num(rec.get("high")) or close
        low = _num(rec.get("low")) or close
        vol = _num(rec.get("volume"))
        if close <= 0 or vol < 200_000:
            continue
        amount = close * vol
        if amount < 30_000_000:
            continue
        pct = (close - open_px) / open_px if open_px else 0.0
        span = high - low
        loc = (close - low) / span if span > 0 else 0.5
        rows.append(
            {
                "code": code,
                "symbol": f"{code}.TW",
                "name": rec.get("name") or code,
                "sector": industry.get(code) or "",
                "market": "TW",
                "close": close,
                "volume": vol,
                "amount": amount,
                "pct": pct,
                "loc": loc,
            }
        )
    if not rows:
        return []
    _rank(rows, "volume")
    _rank(rows, "amount")
    _rank(rows, "pct")
    _rank(rows, "loc")
    for r in rows:
        r["score"] = (
            0.35 * r["amount_rk"]
            + 0.25 * r["volume_rk"]
            + 0.25 * r["pct_rk"]
            + 0.15 * r["loc_rk"]
        )
    rows.sort(key=lambda r: r["score"], reverse=True)
    return rows


def _fallback_rows() -> list[dict]:
    from universe import TW

    out = []
    for sym, name, sector in TW[:DAILY_N]:
        out.append({"symbol": sym, "name": name, "sector": sector, "market": "TW"})
    return out


def _compact(row: dict) -> dict:
    return {
        "symbol": row["symbol"],
        "name": row.get("name") or row["symbol"],
        "sector": row.get("sector") or "",
        "market": "TW",
        "pick_score": round(float(row.get("score") or 0), 4),
    }


def daily_tw_rows(force: bool = False) -> list[dict]:
    """今日潛力股（約 30 檔）。同一交易日重用，新的 MI_INDEX 日才換。"""
    path = cache_dir() / PICKS_FILE
    sess, bars = _latest_session()
    if not force and path.exists() and sess:
        try:
            obj = json.loads(path.read_text())
            if obj.get("session") == sess and obj.get("rows"):
                return obj["rows"]
        except Exception:
            pass
    if not bars:
        return _fallback_rows()
    ranked = _score_session(bars)
    picked: list[dict] = []
    seen: set[str] = set()
    for a in ANCHORS:
        code = a.split(".")[0]
        rec = bars.get(code)
        if not rec:
            continue
        seen.add(a)
        hit = next((r for r in ranked if r["symbol"] == a), None)
        if hit:
            picked.append(_compact(hit))
        else:
            picked.append(
                {
                    "symbol": a,
                    "name": rec.get("name") or a,
                    "sector": "",
                    "market": "TW",
                    "pick_score": 0.0,
                }
            )
    for r in ranked:
        if r["symbol"] in seen:
            continue
        picked.append(_compact(r))
        seen.add(r["symbol"])
        if len(picked) >= DAILY_N:
            break
    if len(picked) < 8:
        picked = _fallback_rows()
    ensure_dir(path.parent)
    try:
        path.write_text(
            json.dumps(
                {
                    "session": sess,
                    "updated": datetime.now(TAIPEI).strftime("%Y-%m-%d %H:%M"),
                    "n": len(picked),
                    "rows": picked,
                },
                ensure_ascii=False,
                indent=2,
            )
        )
    except OSError:
        pass
    return picked
