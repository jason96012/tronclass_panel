"""每日潛力股：約 30 檔，不下載全市場 Yahoo 歷史。隔日（新交易日）換一批。

STOCKGENIE_PICKS_RS_PULLBACK_V1：舊分只看當日成交金額／漲幅／收在高檔，偏向追高、漏掉安靜回檔。
改用證交所 MI_INDEX 日K 快取（twse_bar）近 20 日：相對強度、量縮、距 20 日高點的回檔、族群領先。
新交易日最多補抓數日快取，不掃 Yahoo 全市場。裁決層（engine／scorer）不變。
"""

from __future__ import annotations

import json
import time
from collections import defaultdict
from datetime import datetime, timedelta, timezone

from paths import cache_dir, ensure_dir

TAIPEI = timezone(timedelta(hours=8))
DAILY_N = 30
MAX_PER_SECTOR = 4
PICKS_VER = "rs_pb_v1"
PICKS_FILE = "daily_picks.json"
HIST_WANT = 25
HIST_MAX_NEW = 6


def _num(x) -> float:
    try:
        return float(x)
    except (TypeError, ValueError):
        return 0.0


def _rank(rows: list[dict], key: str, out_key: str | None = None) -> None:
    ordered = sorted(rows, key=lambda r: r[key])
    n = max(len(ordered) - 1, 1)
    dest = out_key or f"{key}_rk"
    for i, r in enumerate(ordered):
        r[dest] = i / n


def _recent_sessions(want: int = HIST_WANT, max_new: int = HIST_MAX_NEW) -> list[tuple[str, dict]]:
    """由近而遠收集 MI_INDEX 日K。已快取的照讀；未快取每天最多新抓 max_new 日。"""
    from data import load_twse_session_bars

    bar_dir = cache_dir() / "twse_bar"
    today = datetime.now(TAIPEI).date()
    d = today
    sessions: list[tuple[str, dict]] = []
    tries = 0
    new_dl = 0
    while len(sessions) < want and tries < want + 50:
        tries += 1
        if d.weekday() >= 5:
            d -= timedelta(days=1)
            continue
        ds = d.strftime("%Y%m%d")
        cached = (bar_dir / f"{ds}.json").exists()
        if not cached and new_dl >= max_new:
            d -= timedelta(days=1)
            continue
        got = load_twse_session_bars(ds)
        if got is None:
            time.sleep(0.12)
            d -= timedelta(days=1)
            continue
        if not cached:
            new_dl += 1
            time.sleep(0.12)
        if got:
            sessions.append((ds, got))
        d -= timedelta(days=1)
    sessions.reverse()
    return sessions


def _pullback_score(ret20: float, dist_high: float) -> float:
    """趨勢向上且距離 20 日高 2–15% 為甜區；貼高追價給低分。"""
    if ret20 <= 0:
        return max(0.0, 0.15 * (1.0 + min(ret20, 0.0)))
    if 0.02 <= dist_high <= 0.15:
        return 1.0
    if dist_high < 0.02:
        return 0.35
    if dist_high <= 0.25:
        return 0.55
    return 0.2


def _contraction_score(vr: float) -> float:
    """量比 0.35–0.95 視為量縮回檔；放量追價接近 0。"""
    if vr <= 0:
        return 0.0
    if 0.35 <= vr <= 0.95:
        return 1.0
    if vr < 0.35:
        return 0.5
    if vr <= 1.3:
        return 0.55
    if vr <= 2.0:
        return 0.2
    return 0.05


def _score_hist(sessions: list[tuple[str, dict]], industry: dict) -> list[dict]:
    last_ds, last_bars = sessions[-1]
    look = min(20, len(sessions))
    closes: dict[str, list[float]] = {}
    vols: dict[str, list[float]] = {}
    highs: dict[str, list[float]] = {}
    lows: dict[str, list[float]] = {}
    names: dict[str, str] = {}
    for _ds, bars in sessions[-look:]:
        for code, rec in bars.items():
            c = _num(rec.get("close"))
            if c <= 0:
                continue
            closes.setdefault(code, []).append(c)
            vols.setdefault(code, []).append(_num(rec.get("volume")))
            highs.setdefault(code, []).append(_num(rec.get("high")) or c)
            lows.setdefault(code, []).append(_num(rec.get("low")) or c)
            if rec.get("name"):
                names[code] = rec["name"]

    rows: list[dict] = []
    for code, rec in last_bars.items():
        series = closes.get(code) or []
        if len(series) < 8:
            continue
        close = _num(rec.get("close"))
        vol = _num(rec.get("volume"))
        open_px = _num(rec.get("open")) or close
        high = _num(rec.get("high")) or close
        low = _num(rec.get("low")) or close
        if close <= 0 or vol < 200_000:
            continue
        amount = close * vol
        if amount < 30_000_000:
            continue
        n = len(series)
        ret20 = series[-1] / series[0] - 1.0 if series[0] else 0.0
        high_n = max(highs.get(code) or [close])
        low_n = min(lows.get(code) or [close])
        vol_n = vols.get(code) or [vol]
        vol20 = sum(vol_n) / max(len(vol_n), 1)
        vr = vol / vol20 if vol20 > 0 else 1.0
        dist_high = (high_n - close) / close if close else 0.0
        loc20 = (close - low_n) / (high_n - low_n) if high_n > low_n else 0.5
        day_pct = (close - open_px) / open_px if open_px else 0.0
        day_span = high - low
        day_loc = (close - low) / day_span if day_span > 0 else 0.5
        chase = 1.0 if (day_pct > 0.04 and day_loc > 0.85 and vr > 1.4) else 0.0
        rows.append(
            {
                "code": code,
                "symbol": f"{code}.TW",
                "name": rec.get("name") or names.get(code) or code,
                "sector": industry.get(code) or "",
                "market": "TW",
                "close": close,
                "volume": vol,
                "amount": amount,
                "ret20": ret20,
                "vr": vr,
                "dist_high": dist_high,
                "loc20": loc20,
                "pb": _pullback_score(ret20, dist_high),
                "ctr": _contraction_score(vr),
                "chase": chase,
                "n_bars": n,
                "session": last_ds,
            }
        )
    if not rows:
        return []
    _rank(rows, "ret20", "rps_rk")
    _rank(rows, "amount", "amount_rk")

    sec_rets: dict[str, list[float]] = defaultdict(list)
    for r in rows:
        if r["sector"]:
            sec_rets[r["sector"]].append(r["ret20"])
    sec_mean = {k: sum(v) / len(v) for k, v in sec_rets.items() if len(v) >= 3}
    sec_ordered = sorted(sec_mean.items(), key=lambda kv: kv[1])
    nsec = max(len(sec_ordered) - 1, 1)
    sec_rk = {k: i / nsec for i, (k, _) in enumerate(sec_ordered)}
    for r in rows:
        r["sector_rk"] = sec_rk.get(r["sector"], 0.5)

    for r in rows:
        r["score"] = (
            0.30 * r["rps_rk"]
            + 0.20 * r["sector_rk"]
            + 0.25 * r["pb"]
            + 0.15 * r["ctr"]
            + 0.10 * r["amount_rk"]
            - 0.35 * r["chase"]
        )
    rows.sort(key=lambda r: r["score"], reverse=True)
    return rows


def _score_oneday(bars: dict, industry: dict) -> list[dict]:
    """快取不足 12 日時的後備：流動性為主，懲罰收在當日高檔的放量上漲（追高）。"""
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
        mid = 1.0 - min(1.0, abs(loc - 0.55) / 0.55)
        chase = 1.0 if (pct > 0.04 and loc > 0.85) else 0.0
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
                "mid": mid,
                "chase": chase,
            }
        )
    if not rows:
        return []
    _rank(rows, "amount")
    _rank(rows, "volume")
    for r in rows:
        r["score"] = (
            0.50 * r["amount_rk"]
            + 0.25 * r["volume_rk"]
            + 0.25 * r["mid"]
            - 0.35 * r["chase"]
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
    why = []
    if row.get("rps_rk") is not None and row["rps_rk"] >= 0.7:
        why.append("相對強度")
    if row.get("ctr") is not None and row["ctr"] >= 0.9:
        why.append("量縮")
    if 0.02 <= _num(row.get("dist_high")) <= 0.15 and _num(row.get("ret20")) > 0:
        why.append("回檔")
    if row.get("sector_rk") is not None and row["sector_rk"] >= 0.7:
        why.append("族群領先")
    return {
        "symbol": row["symbol"],
        "name": row.get("name") or row["symbol"],
        "sector": row.get("sector") or "",
        "market": "TW",
        "pick_score": round(float(row.get("score") or 0), 4),
        "pick_why": " · ".join(why),
    }


def _select(ranked: list[dict], n: int, max_per_sector: int) -> list[dict]:
    picked: list[dict] = []
    seen: set[str] = set()
    sec_n: dict[str, int] = defaultdict(int)
    for r in ranked:
        sym = r["symbol"]
        if sym in seen:
            continue
        sec = r.get("sector") or ""
        if sec and sec_n[sec] >= max_per_sector:
            continue
        picked.append(_compact(r))
        seen.add(sym)
        if sec:
            sec_n[sec] += 1
        if len(picked) >= n:
            break
    if len(picked) < n:
        for r in ranked:
            if r["symbol"] in seen:
                continue
            picked.append(_compact(r))
            seen.add(r["symbol"])
            if len(picked) >= n:
                break
    return picked


def daily_tw_rows(force: bool = False) -> list[dict]:
    """今日潛力股（約 30 檔）。同一交易日＋同一版本重用，新的 MI_INDEX 日才換。"""
    path = cache_dir() / PICKS_FILE
    sessions = _recent_sessions()
    sess = sessions[-1][0] if sessions else None
    if not force and path.exists() and sess:
        try:
            obj = json.loads(path.read_text())
            if obj.get("session") == sess and obj.get("ver") == PICKS_VER and obj.get("rows"):
                return obj["rows"]
        except Exception:
            pass
    if not sessions:
        return _fallback_rows()

    industry: dict = {}
    try:
        from data import load_twse_industry_map

        industry = load_twse_industry_map() or {}
    except Exception:
        industry = {}

    if len(sessions) >= 12:
        ranked = _score_hist(sessions, industry)
    else:
        ranked = _score_oneday(sessions[-1][1], industry)
    picked = _select(ranked, DAILY_N, MAX_PER_SECTOR)
    if len(picked) < 8:
        picked = _fallback_rows()
    ensure_dir(path.parent)
    try:
        path.write_text(
            json.dumps(
                {
                    "session": sess,
                    "ver": PICKS_VER,
                    "updated": datetime.now(TAIPEI).strftime("%Y-%m-%d %H:%M"),
                    "n": len(picked),
                    "hist": len(sessions),
                    "rows": picked,
                },
                ensure_ascii=False,
                indent=2,
            )
        )
    except OSError:
        pass
    return picked
