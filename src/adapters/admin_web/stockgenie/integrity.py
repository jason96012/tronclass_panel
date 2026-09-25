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


# STOCKGENIE_QUOTE_HARDSTOP_V1：0.8% 就會讓買區偏 1–2 跳，門檻改 0.3%。
TWSE_DRIFT_HARD = 0.003


def _index_yyyymmdd(ts) -> str | None:
    try:
        if hasattr(ts, "strftime"):
            return ts.strftime("%Y%m%d")
        s = str(ts)[:10].replace("-", "")
        return s if len(s) >= 8 and s[:8].isdigit() else None
    except Exception:
        return None


def _close_on_session(df: pd.DataFrame, yyyymmdd: str | None) -> float | None:
    """只對同一交易日對帳，避免 T vs T-1 假警報。"""
    if not yyyymmdd or df is None or len(df) == 0:
        return None
    for i in range(len(df) - 1, -1, -1):
        ds = _index_yyyymmdd(df.index[i])
        if ds == yyyymmdd:
            try:
                px = float(df["close"].iloc[i])
            except Exception:
                return None
            return px if px > 0 else None
        if ds and ds < yyyymmdd:
            break
    return None


def check_prices(
    df: pd.DataFrame,
    exdiv: dict | None = None,
    official_close: float | None = None,
    official_asof: str | None = None,
) -> dict[str, Any]:
    """
    還原檢查（同一套閘門，看板與回測都走這裡）：
    1. 近 10 根 OHLC 不合法（高<開收、低>開收）
    2. 近 5 根「大跳空但量不增」→ 比較像未還原／除權，不像成交
    3. 證交所 TWT49U 除權息日落在最近 5 個交易日內
    4. 台股：同一交易日日K收盤 vs 證交所 MI_INDEX 差 ≥0.3% → DATA_MISMATCH 硬阻斷
    官方價以證交所為準（MI_INDEX／twse_bar），不接第三方付費 API。
    """
    empty = {
        "ok": True,
        "issues": [],
        "last_gap": None,
        "suspect_exdiv": False,
        "twse_drift": None,
        "hard_stop": False,
        "status": "OK",
        "yahoo_close": None,
        "official_close": None,
        "official_asof": official_asof,
    }
    if df is None or len(df) < 3:
        return empty
    o, h, l, c = df["open"], df["high"], df["low"], df["close"]
    vol = df["volume"]
    issues: list[str] = []
    tail = min(10, len(df))
    bad_ohlc = int(((h.iloc[-tail:] < o.iloc[-tail:]) | (h.iloc[-tail:] < c.iloc[-tail:]) | (l.iloc[-tail:] > o.iloc[-tail:]) | (l.iloc[-tail:] > c.iloc[-tail:])).sum())
    hard_stop = False
    if bad_ohlc:
        issues.append(f"近{tail}根 OHLC 不合法 {bad_ohlc} 根")
        hard_stop = True

    last_gap = float(c.iloc[-1] / c.iloc[-2] - 1) if float(c.iloc[-2]) else 0.0
    suspect = False
    look = min(5, len(df) - 1)
    v20 = float(vol.iloc[-20:].mean()) if len(vol) >= 20 else float(vol.mean())
    last_rvol = float(vol.iloc[-1] / v20) if v20 > 0 else 1.0
    try:
        last_vol = float(vol.iloc[-1])
    except Exception:
        last_vol = 0.0
    # STOCKGENIE_GIGO_V1：垃圾進、垃圾出。未還原跳空／空量／非法 OHLC 直接硬停，不算技術。
    if last_vol <= 0 and official_close is not None:
        issues.append("最近一根成交量為 0（停牌或資料缺漏）")
        hard_stop = True
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
            hard_stop = True
            break
        if gap >= 0.12 and rvol < 1.2:
            issues.append(f"異常高開跳空 {gap:.1%} 量平（第 {i} 根），不當突破")
            suspect = True
            hard_stop = True
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
    yahoo_px = float(c.iloc[-1]) if len(c) else None
    off_px = None
    if official_close is not None:
        try:
            off_px = float(official_close)
        except Exception:
            off_px = None
        if off_px is not None and off_px <= 0:
            off_px = None
        # STOCKGENIE_QUOTE_HARDSTOP_V1：只比同一交易日；對不上就跳過，不拿 T-1 硬比。
        cmp_px = _close_on_session(df, official_asof) if official_asof else None
        last_ds = _index_yyyymmdd(df.index[-1]) if len(df) else None
        if cmp_px is None and official_asof and last_ds == official_asof:
            cmp_px = yahoo_px
        if cmp_px is None and not official_asof:
            cmp_px = yahoo_px
        if off_px and cmp_px and cmp_px > 0:
            yahoo_px = cmp_px
            twse_drift = cmp_px / off_px - 1.0
            if abs(twse_drift) >= TWSE_DRIFT_HARD:
                issues.append(
                    f"日K收盤 {cmp_px:.2f} 與證交所 {off_px:.2f} 差 {twse_drift:.1%}（≥{TWSE_DRIFT_HARD:.1%}，買區會偏 1–2 跳）"
                )
                suspect = True
                hard_stop = True

    ok = (not suspect) and bad_ohlc == 0 and not hard_stop
    status = "DATA_MISMATCH" if hard_stop else ("ERROR" if not ok else "OK")
    return {
        "ok": ok,
        "issues": issues,
        "last_gap": round(last_gap, 4),
        "suspect_exdiv": suspect,
        "rvol": round(last_rvol, 2),
        "twse_drift": None if twse_drift is None else round(float(twse_drift), 4),
        "hard_stop": hard_stop,
        "status": status,
        "yahoo_close": None if yahoo_px is None else round(float(yahoo_px), 4),
        "official_close": None if off_px is None else round(float(off_px), 4),
        "official_asof": official_asof,
    }

def align_last_to_official(df: pd.DataFrame, official_close: float | None, official_asof: str | None):
    """同一交易日、差 < 硬停門檻時，最後一根收盤對齊證交所，減少買區偏跳。"""
    if df is None or len(df) == 0 or official_close is None:
        return df
    try:
        px = float(official_close)
    except Exception:
        return df
    if px <= 0:
        return df
    i = None
    if official_asof:
        for j in range(len(df) - 1, -1, -1):
            ds = _index_yyyymmdd(df.index[j])
            if ds == official_asof:
                i = j
                break
            if ds and ds < official_asof:
                break
    if i is None:
        last_ds = _index_yyyymmdd(df.index[-1])
        if official_asof and last_ds != official_asof:
            return df
        i = len(df) - 1
    out = df.copy()
    try:
        out.iloc[i, out.columns.get_loc("close")] = px
        if "high" in out.columns:
            out.iloc[i, out.columns.get_loc("high")] = max(float(out["high"].iloc[i]), px)
        if "low" in out.columns:
            out.iloc[i, out.columns.get_loc("low")] = min(float(out["low"].iloc[i]), px)
    except Exception:
        return df
    return out
