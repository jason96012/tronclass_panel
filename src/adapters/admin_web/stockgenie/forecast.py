"""條件路徑：用這檔過去同樣狀態之後的實際漲跌當樣本。不是預言。"""

from __future__ import annotations

from typing import Any

import numpy as np
import pandas as pd

from engine import add_indicators

HORIZON = 20
MIN_BARS = 80


def _f(x) -> float | None:
    try:
        v = float(x)
    except (TypeError, ValueError):
        return None
    if v != v:  # NaN
        return None
    return v


def classify_state(df: pd.DataFrame, i: int) -> str | None:
    row = df.iloc[i]
    close = _f(row["close"])
    if close is None or close <= 0:
        return None
    ma20 = _f(row["ma20"]) if "ma20" in df.columns else None
    ma60 = _f(row["ma60"]) if "ma60" in df.columns else None
    rsi = _f(row["rsi"]) if "rsi" in df.columns else 50.0
    ma20 = close if ma20 is None else ma20
    ma60 = close if ma60 is None else ma60
    rsi = 50.0 if rsi is None else rsi
    if close > ma20 > ma60:
        trend = "bull"
    elif close < ma20 < ma60:
        trend = "bear"
    else:
        trend = "range"
    if rsi >= 70:
        mom = "hot"
    elif rsi >= 50:
        mom = "ok"
    else:
        mom = "cold"
    return f"{trend}:{mom}"


_TREND = {
    "bull": "多頭排列（價>MA20>MA60）",
    "bear": "空頭排列（價<MA20<MA60）",
    "range": "均線糾結／盤整",
}
_MOM = {
    "hot": "RSI 偏熱 ≥70",
    "ok": "RSI 50–70",
    "cold": "RSI <50",
}


def state_label(state: str | None) -> str:
    if not state or ":" not in state:
        return "狀態不明"
    trend, mom = state.split(":", 1)
    return f"{_TREND.get(trend, trend)} · {_MOM.get(mom, mom)}"


def _stats(arr: list[float]) -> dict[str, Any] | None:
    if not arr:
        return None
    a = np.asarray(arr, dtype=float)
    a = a[np.isfinite(a)]
    if len(a) == 0:
        return None
    return {
        "n": int(len(a)),
        "p_up": round(float((a > 0).mean()) * 100, 1),
        "median": round(float(np.median(a)) * 100, 2),
        "mean": round(float(a.mean()) * 100, 2),
        "p25": round(float(np.percentile(a, 25)) * 100, 2),
        "p75": round(float(np.percentile(a, 75)) * 100, 2),
    }


def analog_pack(df: pd.DataFrame | None, horizon: int = HORIZON) -> dict[str, Any]:
    """回傳樣本統計＋從現價展開的 25/50/75 分位路徑。"""
    empty = {
        "ok": False,
        "n": 0,
        "state": None,
        "state_label": "日K不足",
        "matched": None,
        "bias": "unknown",
        "bias_label": "日K不足，無法估走向",
        "confidence": "無",
        "h5": None,
        "h10": None,
        "h20": None,
        "cone": [],
        "price": None,
        "atr": None,
        "next_lo": None,
        "next_hi": None,
        "note": "日K太短，沒有足夠的歷史同類走勢。",
    }
    if df is None or len(df) < MIN_BARS + horizon:
        return empty
    work = df if "ma20" in df.columns and "rsi" in df.columns else add_indicators(df)
    n = len(work)
    cur = classify_state(work, -1)
    if cur is None:
        return empty
    trend = cur.split(":", 1)[0]

    def collect(pred) -> tuple[dict[int, list], list[list[float]]]:
        rets: dict[int, list] = {5: [], 10: [], 20: []}
        paths: list[list[float]] = []
        last = n - horizon - 1
        for i in range(MIN_BARS, last):
            if not pred(i):
                continue
            c0 = _f(work["close"].iloc[i])
            if c0 is None or c0 <= 0:
                continue
            ok = True
            row_path = [1.0]
            for k in range(1, horizon + 1):
                ck = _f(work["close"].iloc[i + k])
                if ck is None or ck <= 0:
                    ok = False
                    break
                row_path.append(ck / c0)
            if not ok:
                continue
            for h in (5, 10, 20):
                if h <= horizon:
                    rets[h].append(row_path[h] - 1.0)
            paths.append(row_path)
        return rets, paths

    rets, paths = collect(lambda i: classify_state(work, i) == cur)
    matched = "state"
    if len(paths) < 12:
        rets, paths = collect(lambda i: (classify_state(work, i) or "").split(":")[0] == trend)
        matched = "trend"
    if not paths:
        empty["state"] = cur
        empty["state_label"] = state_label(cur)
        empty["note"] = "找不到足夠的同類歷史狀態。"
        return empty

    last_px = _f(work["close"].iloc[-1]) or 0.0
    atr = _f(work["atr"].iloc[-1]) if "atr" in work.columns else None
    if atr is None or atr <= 0:
        atr = last_px * 0.02 if last_px else None
    h5, h10, h20 = _stats(rets[5]), _stats(rets[10]), _stats(rets[20])
    mat = np.asarray(paths, dtype=float)
    med = np.median(mat, axis=0)
    lo = np.percentile(mat, 25, axis=0)
    hi = np.percentile(mat, 75, axis=0)
    cone = []
    for k in range(horizon + 1):
        cone.append(
            {
                "k": k,
                "med": round(last_px * float(med[k]), 4),
                "lo": round(last_px * float(lo[k]), 4),
                "hi": round(last_px * float(hi[k]), 4),
            }
        )

    med20 = None if h20 is None else h20["median"]
    p_up = None if h20 is None else h20["p_up"]
    if med20 is None:
        bias, bias_label = "unknown", "樣本不足"
    elif med20 >= 3 and (p_up or 0) >= 55:
        bias, bias_label = "bull", "樣本偏多（20 日中位為正且上漲居多）"
    elif med20 <= -3 and (p_up or 0) <= 45:
        bias, bias_label = "bear", "樣本偏空（20 日中位為負且下跌居多）"
    else:
        bias, bias_label = "range", "樣本震盪／分歧（中位接近零或漲跌參半）"

    nn = len(paths)
    if nn >= 40:
        confidence = "中高"
    elif nn >= 20:
        confidence = "中"
    elif nn >= 12:
        confidence = "中低"
    else:
        confidence = "低"

    return {
        "ok": True,
        "n": nn,
        "state": cur,
        "state_label": state_label(cur),
        "matched": matched,
        "bias": bias,
        "bias_label": bias_label,
        "confidence": confidence,
        "h5": h5,
        "h10": h10,
        "h20": h20,
        "cone": cone,
        "price": round(last_px, 4),
        "atr": None if atr is None else round(float(atr), 4),
        "next_lo": None if atr is None else round(last_px - float(atr), 4),
        "next_hi": None if atr is None else round(last_px + float(atr), 4),
        "note": "用這檔過去同樣均線／RSI 狀態之後的實際收盤當樣本。條件路徑，不是預言。",
    }


def build_forecast(
    df: pd.DataFrame | None,
    plan: dict | None = None,
    setup: dict | None = None,
    regime: dict | None = None,
    hist: dict | None = None,
    tech: dict | None = None,
) -> dict[str, Any]:
    out = analog_pack(df)
    plan = plan or {}
    setup = setup or {}
    regime = regime or {}
    hist = hist or {}
    stop = plan.get("live_stop") or plan.get("stop")
    buy = plan.get("buy") or {}
    hold = buy.get("hi") or buy.get("mid")
    ma20 = None
    if tech:
        ma20 = tech.get("ma20")
    weekly = setup.get("weekly") or {}
    wkey = weekly.get("key")
    rkey = regime.get("key")
    conflict = False
    reasons: list[str] = []
    if out.get("bias") == "bull" and (wkey in ("bear", "bear_flat") or rkey == "bear"):
        conflict = True
        reasons.append("週線或大盤偏空，與個股樣本偏多衝突")
    if out.get("bias") == "bear" and (wkey in ("bull", "bull_flat") or rkey == "bull"):
        conflict = True
        reasons.append("週線或大盤偏多，與個股樣本偏空衝突")
    if conflict:
        out["confidence"] = "低"
        out["bias_label"] = (out.get("bias_label") or "") + " · 與大盤／週線衝突"
    out["conflict"] = conflict
    out["conflict_why"] = "；".join(reasons) if reasons else None
    out["invalid_below"] = None if stop is None else round(float(stop), 4)
    out["hold_above"] = None if hold is None else round(float(hold), 4)
    out["ma20"] = None if ma20 is None else round(float(ma20), 4)
    out["weekly"] = weekly.get("label")
    out["regime"] = regime.get("label")
    wr = hist.get("win_rate")
    out["hist_n"] = hist.get("n")
    out["hist_win_rate"] = wr
    cone = out.get("cone") or []
    last = cone[-1] if cone else None
    scenarios = []
    if last:
        h20 = out.get("h20") or {}
        scenarios = [
            {
                "key": "bull",
                "label": "樂觀（上四分位）",
                "px": last["hi"],
                "ret": h20.get("p75"),
            },
            {
                "key": "base",
                "label": "基準（中位）",
                "px": last["med"],
                "ret": h20.get("median"),
            },
            {
                "key": "bear",
                "label": "悲觀（下四分位）",
                "px": last["lo"],
                "ret": h20.get("p25"),
            },
        ]
    out["scenarios"] = scenarios
    bits = [out.get("note") or ""]
    if hold:
        bits.append(f"守住買區上緣 {hold:.2f}，基準路徑才有效。")
    if stop:
        bits.append(f"跌破停損 {stop:.2f}，偏多路徑作廢。")
    if out.get("matched") == "trend":
        bits.append("同樣 RSI 區間樣本不足，改用同一均線結構。")
    if (hist.get("n") or 0) >= 5 and wr is not None:
        bits.append(f"若照系統進出，這檔歷史勝率 {wr}%。")
    out["note"] = "".join(bits) if bits else out.get("note")
    return out
