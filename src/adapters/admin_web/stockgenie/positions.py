"""持倉狀態機：進場後只管理停損／1R 上移／目標／20 日，不每天重貼可買。"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from paths import cache_dir

PATH = cache_dir() / "positions.json"
TAIPEI = timezone(timedelta(hours=8))


def _load() -> list[dict]:
    if not PATH.exists():
        return []
    try:
        obj = json.loads(PATH.read_text())
        return obj.get("positions") or []
    except Exception:
        return []


def _save(rows: list[dict]) -> None:
    PATH.parent.mkdir(parents=True, exist_ok=True)
    PATH.write_text(json.dumps({"positions": rows}, ensure_ascii=False, indent=2, default=str))


def list_open() -> list[dict]:
    return [p for p in _load() if p.get("status") == "open"]


def by_symbol(symbol: str) -> dict | None:
    symbol = symbol.upper()
    for p in list_open():
        if p.get("symbol") == symbol:
            return p
    return None


def open_position(symbol: str, payload: dict) -> dict:
    symbol = symbol.upper()
    rows = _load()
    for p in rows:
        if p.get("symbol") == symbol and p.get("status") == "open":
            p.update(payload)
            p["symbol"] = symbol
            p["status"] = "open"
            _save(rows)
            return p
    rec = {
        "symbol": symbol,
        "status": "open",
        "opened": datetime.now(TAIPEI).strftime("%Y-%m-%d"),
        "path": payload.get("path") or "pullback",
        "entry": payload.get("entry"),
        "stop": payload.get("stop"),
        "target": payload.get("target"),
        "qty": payload.get("qty") or 0,
        "hold_days": 0,
        "trail_active": False,
    }
    rows.append(rec)
    _save(rows)
    return rec


def close_position(symbol: str, reason: str, px: float | None = None) -> dict | None:
    symbol = symbol.upper()
    rows = _load()
    found = None
    for p in rows:
        if p.get("symbol") == symbol and p.get("status") == "open":
            p["status"] = "closed"
            p["closed"] = datetime.now(TAIPEI).strftime("%Y-%m-%d")
            p["exit_reason"] = reason
            p["exit_px"] = px
            found = p
            break
    _save(rows)
    return found


def mark(full: dict) -> dict | None:
    """用最新日線更新持倉：移動停損、到期、停利、停損。回傳覆蓋用的裁決。"""
    pos = by_symbol(full.get("symbol") or "")
    if not pos:
        return None
    price = full.get("price")
    plan = full.get("plan") or {}
    atr = (full.get("tech") or {}).get("atr") or plan.get("atr")
    ma20 = (full.get("tech") or {}).get("ma20")
    entry = float(pos.get("entry") or 0)
    stop = float(pos.get("stop") or 0)
    target = pos.get("target")
    if not entry or not price:
        return None
    asof = str(full.get("asof") or datetime.now(TAIPEI).strftime("%Y-%m-%d"))[:10]
    opened = str(pos.get("opened") or asof)[:10]
    try:
        hold = max(0, (datetime.strptime(asof, "%Y-%m-%d") - datetime.strptime(opened, "%Y-%m-%d")).days)
    except Exception:
        hold = int(pos.get("hold_days") or 0)
    pos["hold_days"] = hold
    risk = entry - stop
    t1 = entry + risk if risk > 0 else None
    trail = stop
    if t1 and price >= t1 and ma20 and atr:
        trail = max(stop, float(ma20) - 0.5 * float(atr))
        pos["trail_active"] = True
        pos["stop"] = round(trail, 4)
        stop = trail
    reason = None
    key, title, tone = "hold", "持有 · 管理停損", "good"
    if price <= stop:
        reason = "停損"
        key, title, tone = "exit_stop", "出場 · 停損", "bad"
    elif target and price >= float(target):
        reason = "停利"
        key, title, tone = "exit_tgt", "出場 · 停利", "good"
    elif hold >= 20:
        reason = "到期"
        key, title, tone = "exit_time", "出場 · 滿 20 日", "warn"
    if reason:
        close_position(full["symbol"], reason, price)
    else:
        rows = _load()
        for p in rows:
            if p.get("symbol") == full["symbol"] and p.get("status") == "open":
                p.update(pos)
        _save(rows)
    return {
        "key": key,
        "title": title,
        "tone": tone,
        "reason": f"持倉自 {pos.get('opened')} 進場 {entry}。停損 {stop}。"
        + (f"已觸發{reason}。" if reason else "不再每天重判可買。"),
        "flags": ["持倉"],
        "rr": None,
        "entry": entry,
        "stop": stop,
        "target": target,
        "path": pos.get("path"),
        "hold": True,
        "hold_days": hold,
    }
