"""籌碼成本分布 CYQ：近 250 日價量三角攤價、換手率衰減。"""

from __future__ import annotations

from typing import Any

import numpy as np
import pandas as pd


def compute(df: pd.DataFrame, shares: float | None, bins: int = 72) -> dict[str, Any] | None:
    if df is None or len(df) < 40:
        return None
    tail = df.iloc[-250:].copy()
    px_lo = float(tail["low"].min())
    px_hi = float(tail["high"].max())
    if px_hi <= px_lo:
        return None
    edges = np.linspace(px_lo * 0.985, px_hi * 1.015, bins + 1)
    centers = (edges[:-1] + edges[1:]) / 2.0
    dist = np.zeros(bins, dtype=float)
    use_shares = float(shares) if shares and shares > 0 else None

    for _, row in tail.iterrows():
        vol = float(row["volume"] or 0)
        if vol <= 0:
            continue
        if use_shares:
            turnover = min(max(vol / use_shares, 0.002), 0.35)
        else:
            v20 = float(row.get("vol20") or vol)
            turnover = min(max(vol / max(v20 * 20, 1.0), 0.01), 0.25)
        dist *= 1.0 - turnover
        lo, hi, cl = float(row["low"]), float(row["high"]), float(row["close"])
        typical = (lo + hi + cl) / 3.0
        if hi <= lo:
            hi = lo * 1.001
        half = max((hi - lo) / 2.0, 1e-9)
        w = np.clip(1.0 - np.abs(centers - typical) / (hi - lo + 1e-9), 0, None)
        # 只攤在當日高低內
        mask = (centers >= lo) & (centers <= hi)
        w = np.where(mask, np.maximum(w, 0.05), 0.0)
        s = w.sum()
        if s <= 0:
            idx = int(np.clip(np.searchsorted(edges, cl) - 1, 0, bins - 1))
            dist[idx] += vol
        else:
            dist += vol * (w / s)

    total = dist.sum()
    if total <= 0:
        return None
    price = float(df["close"].iloc[-1])
    profit = float(dist[centers < price].sum() / total)
    avg = float((centers * dist).sum() / total)
    cdf = np.cumsum(dist) / total
    lo90 = float(centers[np.searchsorted(cdf, 0.05)])
    hi90 = float(centers[min(np.searchsorted(cdf, 0.95), bins - 1)])
    peak = float(centers[int(dist.argmax())])
    hist = [
        {
            "px": round(float(centers[i]), 4),
            "mass": round(float(dist[i] / total), 6),
            "profit": bool(centers[i] < price),
        }
        for i in range(bins)
        if dist[i] > 0
    ]
    return {
        "profit_pct": round(profit * 100, 1),
        "avg_cost": round(avg, 4),
        "range90": [round(lo90, 4), round(hi90, 4)],
        "peak": round(peak, 4),
        "hist": hist,
    }
