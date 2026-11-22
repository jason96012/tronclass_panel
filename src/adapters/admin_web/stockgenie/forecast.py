"""條件路徑：用這檔過去同樣狀態之後的實際漲跌當樣本。不是預言。

STOCKGENIE_WALK_CONE_V1：往後推時每一天只用當天以前的日K；圖上對照後來實際收盤。
"""

from __future__ import annotations

import math
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


def _wilson_lower(k: float, n: int, z: float = 1.96) -> float:
    """Wilson 95% 信賴區間下緣。點估計 54.8%、n=31 時下緣遠低於 50%。"""
    if n <= 0:
        return 0.0
    p = max(0.0, min(1.0, float(k) / float(n)))
    z2 = z * z
    denom = 1.0 + z2 / n
    centre = p + z2 / (2.0 * n)
    rad = (p * (1.0 - p) + z2 / (4.0 * n)) / n
    adj = z * math.sqrt(max(rad, 0.0))
    return max(0.0, (centre - adj) / denom)


def vol_regime_shift(df: pd.DataFrame | None) -> dict[str, Any]:
    """STOCKGENIE_REGIME_SHIFT_V1：近 20 日 ATR% 相對更早中位差 ≥1.8× 或 ≤0.5× → 舊樣本失效。"""
    empty = {"regime_shift": False, "atr_ratio": None}
    if df is None or "atr" not in df.columns or "close" not in df.columns or len(df) < 100:
        return empty
    try:
        atr_pct = (df["atr"] / df["close"]).replace([np.inf, -np.inf], np.nan).dropna()
        if len(atr_pct) < 80:
            return empty
        recent = float(atr_pct.iloc[-20:].median())
        past = float(atr_pct.iloc[20:-20].median()) if len(atr_pct) > 60 else None
        if not past or past <= 0 or recent != recent:
            return empty
        ratio = recent / past
        return {"regime_shift": bool(ratio >= 1.8 or ratio <= 0.5), "atr_ratio": round(ratio, 2)}
    except Exception:
        return empty


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
        "small_sample": True,
        "no_edge": True,
        "edge_note": "日K不足，無法評估優勢",
        "wilson_lo": None,
        "regime_shift": False,
        "atr_ratio": None,
        "note": "日K太短，沒有足夠的歷史同類走勢。",
    }
    # STOCKGENIE_WALK_CONE_V1：只看傳入序列末端當天（含）以前的日K。
    # 同類樣本的「後來走勢」也必須落在末端當天之前，不能偷看未來。
    if df is None or len(df) < MIN_BARS + horizon + 1:
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
        # i + horizon <= n-1：結局最遠只用到 as-of 當天收盤
        end = n - horizon
        for i in range(MIN_BARS, end):
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
    # STOCKGENIE_FORECAST_EDGE_V1：N<50 一律無優勢；頻率<65% 或 Wilson 下緣<55% 或中位 < 1.5×ATR。
    small_sample = nn < 50
    atr_frac = (float(atr) / last_px) if atr and last_px else None
    med_frac = None if med20 is None else float(med20) / 100.0
    k_up = 0.0 if p_up is None else float(p_up) / 100.0 * nn
    wilson_lo = _wilson_lower(k_up, nn)
    no_edge = False
    if small_sample:
        no_edge = True
    if p_up is not None and p_up < 65:
        no_edge = True
    if wilson_lo < 0.55:
        no_edge = True
    if atr_frac is not None and med_frac is not None and med_frac < 1.5 * atr_frac:
        no_edge = True
    shift = vol_regime_shift(work)
    if shift.get("regime_shift"):
        no_edge = True
    edge_bits = []
    if small_sample:
        edge_bits.append("小樣本，參考價值低")
        confidence = "低"
    elif nn >= 40:
        confidence = "中高"
    elif nn >= 20:
        confidence = "中"
    else:
        confidence = "中低"
    if wilson_lo < 0.55:
        edge_bits.append(f"20 日只是歷史頻率，Wilson 下緣 {wilson_lo*100:.0f}%＜55%，不是下次會漲的機率")
    if no_edge:
        edge_bits.append("歷史共振邊際優勢不足 (No Edge)")
        bias_label = "歷史共振邊際優勢不足 (No Edge)"
        confidence = "低"
    if shift.get("regime_shift"):
        edge_bits.append(f"波動狀態已切換（近20日 ATR 為舊樣本 {shift.get('atr_ratio')} 倍），舊路徑不適用")
        confidence = "低"
    edge_note = "；".join(edge_bits) if edge_bits else None

    return {
        "ok": True,
        "n": nn,
        "state": cur,
        "state_label": state_label(cur),
        "matched": matched,
        "bias": bias,
        "bias_label": bias_label,
        "confidence": confidence,
        "small_sample": small_sample,
        "no_edge": no_edge,
        "edge_note": edge_note,
        "wilson_lo": round(wilson_lo * 100, 1),
        "regime_shift": bool(shift.get("regime_shift")),
        "atr_ratio": shift.get("atr_ratio"),
        "h5": h5,
        "h10": h10,
        "h20": h20,
        "cone": cone,
        "price": round(last_px, 4),
        "atr": None if atr is None else round(float(atr), 4),
        "next_lo": None if atr is None else round(last_px - float(atr), 4),
        "next_hi": None if atr is None else round(last_px + float(atr), 4),
        "note": "用這檔過去同樣均線／RSI 狀態之後的實際收盤當樣本。數字是歷史頻率，不是下次上漲的機率。樣本的後來走勢不含 as-of 之後的資料。",
    }


def analog_guard(df, regime: dict | None = None, weekly: dict | None = None) -> dict[str, Any]:
    """掃描也能用的輕量否決：無優勢／衝突／波動切換。不含 walk 錐。"""
    pack = analog_pack(df)
    regime = regime or {}
    weekly = weekly or {}
    conflict = False
    why = []
    if pack.get("bias") == "bull" and ((weekly.get("key") in ("bear", "bear_flat")) or regime.get("key") == "bear"):
        conflict = True
        why.append("週線或大盤偏空，與個股樣本偏多衝突")
    if pack.get("bias") == "bear" and ((weekly.get("key") in ("bull", "bull_flat")) or regime.get("key") == "bull"):
        conflict = True
        why.append("週線或大盤偏多，與個股樣本偏空衝突")
    if pack.get("regime_shift"):
        conflict = True
        why.append("波動狀態已切換")
    return {
        "n": pack.get("n") or 0,
        "no_edge": bool(pack.get("no_edge")),
        "small_sample": bool(pack.get("small_sample")),
        "wilson_lo": pack.get("wilson_lo"),
        "conflict": conflict,
        "conflict_why": "；".join(why) if why else None,
        "regime_shift": bool(pack.get("regime_shift")),
        "atr_ratio": pack.get("atr_ratio"),
        "bias": pack.get("bias"),
    }


def walk_forward_cones(
    df: pd.DataFrame | None,
    step: int = HORIZON,
    limit: int = 4,
    horizon: int = HORIZON,
) -> list[dict[str, Any]]:
    """每隔 step 根、只用當天以前的資料重算一次錐，再對照後來實際收盤。"""
    if df is None or len(df) < MIN_BARS + horizon + step:
        return []
    work = df if "ma20" in df.columns and "rsi" in df.columns else add_indicators(df)
    n = len(work)
    origins: list[int] = []
    t = n - 1 - step
    while t >= MIN_BARS + horizon and len(origins) < limit:
        origins.append(int(t))
        t -= step
    origins.reverse()
    out: list[dict[str, Any]] = []
    for origin in origins:
        pack = analog_pack(work.iloc[: origin + 1], horizon=horizon)
        if not pack.get("ok"):
            continue
        cone = pack.get("cone") or []
        actual: list[dict[str, Any]] = []
        for k in range(0, horizon + 1):
            j = origin + k
            if j >= n:
                break
            ck = _f(work["close"].iloc[j])
            if ck is None or ck <= 0:
                break
            dt = work.index[j]
            actual.append(
                {
                    "k": k,
                    "px": round(float(ck), 4),
                    "t": dt.strftime("%Y-%m-%d") if hasattr(dt, "strftime") else str(dt)[:10],
                }
            )
        err = None
        inside = None
        k_end = min(len(actual), len(cone)) - 1
        if k_end >= 1 and actual and cone:
            a = actual[k_end]["px"]
            m = cone[k_end]["med"]
            lo = cone[k_end]["lo"]
            hi = cone[k_end]["hi"]
            c0 = actual[0]["px"]
            if c0:
                err = round((a - m) / c0 * 100, 2)
            inside = bool(lo <= a <= hi)
        asof = work.index[origin]
        out.append(
            {
                "asof": asof.strftime("%Y-%m-%d") if hasattr(asof, "strftime") else str(asof)[:10],
                "cone": cone,
                "actual": actual,
                "n": pack.get("n"),
                "err_pct": err,
                "inside": inside,
                "horizon": k_end,
                "state": pack.get("state_label"),
            }
        )
    return out


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
    if out.get("regime_shift"):
        conflict = True
        reasons.append("波動狀態已切換，舊樣本不適用")
    if conflict:
        out["confidence"] = "低"
        out["bias_label"] = (out.get("bias_label") or "") + " · 與大盤／週線／波動衝突"
        out["no_edge"] = True
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
                "label": "歷史上四分位（不是目標價）",
                "px": last["hi"],
                "ret": h20.get("p75"),
            },
            {
                "key": "base",
                "label": "歷史中位（不是目標價）",
                "px": last["med"],
                "ret": h20.get("median"),
            },
            {
                "key": "bear",
                "label": "歷史下四分位（不是目標價）",
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
    hn = hist.get("n") or 0
    if hn:
        bits.append(f"同一規則在這段日K模擬 {hn} 筆（會過擬合，不能當下次會贏的證據）。")
    out["note"] = " ".join(b for b in bits if b)
    try:
        walk = walk_forward_cones(df)
    except Exception:
        walk = []
    out["walk"] = walk
    errs = [w.get("err_pct") for w in walk if w.get("err_pct") is not None]
    insides = [w.get("inside") for w in walk if w.get("inside") is not None]
    out["walk_n"] = len(walk)
    out["walk_err_med"] = round(float(np.median(np.asarray(errs, dtype=float))), 2) if errs else None
    out["walk_inside_pct"] = round(100.0 * sum(1 for x in insides if x) / len(insides), 0) if insides else None
    overfit = False
    if out.get("matched") == "trend":
        overfit = True
    if (out.get("walk_n") or 0) >= 2 and (out.get("walk_inside_pct") is not None) and out["walk_inside_pct"] < 50:
        overfit = True
    hn = out.get("hist_n") or 0
    if hn and hn < 20:
        overfit = True
    out["overfit"] = overfit
    if overfit:
        out["confidence"] = "低"
        note = out.get("note") or ""
        if "過擬合風險" not in note:
            out["note"] = (note + " 回測／同類路徑有過擬合風險，禁止當實盤依據。").strip()
    return out
