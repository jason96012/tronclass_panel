"""價格還原／K 線合法性檢查。除權未還原會讓買賣點整段錯位。"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Any

import pandas as pd

TAIPEI = timezone(timedelta(hours=8))


def _parse_exdiv_date(exdiv: dict | None):
    if not exdiv:
        return None
    raw = str(exdiv.get("date") or "").strip()
    for fmt in ("%Y%m%d", "%Y/%m/%d", "%Y-%m-%d"):
        try:
            return datetime.strptime(raw.replace("年", "").replace("月", "").replace("日", ""), fmt).date()
        except Exception:
            continue
    digits = "".join(ch for ch in raw if ch.isdigit())
    if len(digits) >= 8:
        try:
            return datetime.strptime(digits[:8], "%Y%m%d").date()
        except Exception:
            return None
    return None


def check_prices(
    df: pd.DataFrame,
    exdiv: dict | None = None,
    official_close: float | None = None,
) -> dict[str, Any]:
    """
    還原檢查（同一套閘門，看板與回測都走這裡）：
    1. 近 10 根 OHLC 不合法（高<開收、低>開收）
    2. 近 5 根「大跳空但量不增」→ 比較像未還原／除權，不像成交
    3. 證交所 TWT49U 除權息日落在最近 5 個交易日內
    4. 台股：Yahoo 最後收盤 vs 證交所 MI_INDEX 收盤差 ≥2%
    """
    if df is None or len(df) < 3:
        return {"ok": True, "issues": [], "last_gap": None, "suspect_exdiv": False, "twse_drift": None}
    o, h, l, c = df["open"], df["high"], df["low"], df["close"]
    vol = df["volume"]
    issues: list[str] = []
    tail = min(10, len(df))
    bad_ohlc = int(((h.iloc[-tail:] < o.iloc[-tail:]) | (h.iloc[-tail:] < c.iloc[-tail:]) | (l.iloc[-tail:] > o.iloc[-tail:]) | (l.iloc[-tail:] > c.iloc[-tail:])).sum())
    if bad_ohlc:
        issues.append(f"近{tail}根 OHLC 不合法 {bad_ohlc} 根")

    last_gap = float(c.iloc[-1] / c.iloc[-2] - 1) if float(c.iloc[-2]) else 0.0
    suspect = False
    look = min(5, len(df) - 1)
    v20 = float(vol.iloc[-20:].mean()) if len(vol) >= 20 else float(vol.mean())
    last_rvol = float(vol.iloc[-1] / v20) if v20 > 0 else 1.0
    for i in range(-look, 0):
        prev = float(c.iloc[i - 1])
        cur = float(c.iloc[i])
        if prev <= 0:
            continue
        gap = cur / prev - 1
        v_i = float(vol.iloc[i])
        rvol = v_i / v20 if v20 > 0 else 1.0
        if gap <= -0.07 and rvol < 1.3:
            issues.append(f"疑似未還原跳空 {gap:.1%}（第 {i} 根，量未放大 RVOL {rvol:.2f}）")
            suspect = True
            break
        if gap >= 0.12 and rvol < 1.2:
            issues.append(f"異常高開跳空 {gap:.1%} 量平（第 {i} 根），不當突破")
            suspect = True
            break

    xd = _parse_exdiv_date(exdiv)
    if xd is not None:
        asof = df.index[-1]
        try:
            asof_d = asof.date() if hasattr(asof, "date") else datetime.now(TAIPEI).date()
        except Exception:
            asof_d = datetime.now(TAIPEI).date()
        delta = (xd - asof_d).days
        if -5 <= delta <= 2:
            issues.append(f"除權息日 {xd.isoformat()}（距收盤日 {delta} 日），價位幾何可能未還原")
            suspect = True

    twse_drift = None
    if official_close is not None:
        try:
            off = float(official_close)
        except Exception:
            off = 0.0
        last = float(c.iloc[-1])
        if off > 0 and last > 0:
            twse_drift = last / off - 1.0
            if abs(twse_drift) >= 0.02:
                issues.append(f"Yahoo 收盤 {last:.2f} 與證交所 {off:.2f} 差 {twse_drift:.1%}（可能未還原或資料源不一致）")
                suspect = True

    return {
        "ok": not suspect and bad_ohlc == 0,
        "issues": issues,
        "last_gap": round(last_gap, 4),
        "suspect_exdiv": suspect,
        "rvol": round(last_rvol, 2),
        "twse_drift": None if twse_drift is None else round(float(twse_drift), 4),
    }
