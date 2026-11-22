"""近 40 日成交量分布（誰在守），不是 250 日 CYQ。"""

from __future__ import annotations

from typing import Any

import numpy as np
import pandas as pd


def recent_profile(df: pd.DataFrame, lookback: int = 40, bins: int = 48) -> dict[str, Any] | None:
    if df is None or len(df) < 15:
        return None
    tail = df.iloc[-lookback:].copy()
    lo = float(tail["low"].min())
    hi = float(tail["high"].max())
    if hi <= lo:
        return None
    edges = np.linspace(lo * 0.995, hi * 1.005, bins + 1)
    centers = (edges[:-1] + edges[1:]) / 2.0
    dist = np.zeros(bins, dtype=float)
    for _, row in tail.iterrows():
        vol = float(row["volume"] or 0)
        if vol <= 0:
            continue
        a, b, c = float(row["low"]), float(row["high"]), float(row["close"])
        if b <= a:
            b = a * 1.001
        w = np.clip(1.0 - np.abs(centers - (a + b + c) / 3.0) / (b - a + 1e-9), 0, None)
        mask = (centers >= a) & (centers <= b)
        w = np.where(mask, np.maximum(w, 0.05), 0.0)
        s = w.sum()
        if s <= 0:
            continue
        dist += vol * (w / s)
    total = dist.sum()
    if total <= 0:
        return None
    poc = float(centers[int(dist.argmax())])
    cdf = np.cumsum(dist) / total
    val = float(centers[np.searchsorted(cdf, 0.15)])
    vah = float(centers[min(np.searchsorted(cdf, 0.85), bins - 1)])
    price = float(df["close"].iloc[-1])
    defending = abs(price - poc) / price <= 0.012 or (val <= price <= vah)
    return {
        "poc": round(poc, 4),
        "val": round(val, 4),
        "vah": round(vah, 4),
        "defending": defending,
        "lookback": lookback,
    }
