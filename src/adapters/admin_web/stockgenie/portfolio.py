"""組合層：族群擁擠、可買檔數上限、相對大盤 β。"""

from __future__ import annotations

from collections import defaultdict
from typing import Any

import numpy as np
import pandas as pd


MAX_BUYS = 8
MAX_PER_SECTOR = 2


def beta_vs(index_df: pd.DataFrame | None, stock_df: pd.DataFrame, n: int = 60) -> float | None:
    if index_df is None or stock_df is None or len(index_df) < n or len(stock_df) < n:
        return None
    a = stock_df["close"].pct_change().dropna().iloc[-n:]
    b = index_df["close"].pct_change().dropna().iloc[-n:]
    aligned = pd.concat([a, b], axis=1, join="inner")
    if len(aligned) < 30:
        return None
    x, y = aligned.iloc[:, 1].to_numpy(), aligned.iloc[:, 0].to_numpy()
    var = float(np.var(x))
    if var <= 0:
        return None
    return float(np.cov(y, x, ddof=0)[0, 1] / var)


def apply(packed: dict, positions: list[dict] | None = None) -> None:
    held_sec: dict[str, int] = defaultdict(int)
    for p in positions or []:
        # sector filled by caller via packed
        pass
    by_sec: dict[str, list] = defaultdict(list)
    buys = []
    for full in packed.values():
        v = full.get("verdict") or {}
        if v.get("key") in ("buy", "plan", "buy_breakout"):
            by_sec[full.get("sector") or ""].append(full)
            buys.append(full)
            if (full.get("beta") or 0) > 1.8:
                flags = list(v.get("flags") or [])
                if "高β" not in flags:
                    flags.append("高β")
                v["flags"] = flags
    held_symbols = {p.get("symbol") for p in (positions or []) if p.get("status") == "open"}
    for sec, items in by_sec.items():
        if not sec:
            continue
        extra = sum(1 for s in held_symbols if (packed.get(s) or {}).get("sector") == sec)
        cap = max(0, MAX_PER_SECTOR - extra)
        items.sort(
            key=lambda x: (x.get("factor_rank") or 0, (x.get("composite") or {}).get("score") or 0),
            reverse=True,
        )
        for loser in items[cap:]:
            _demote(loser, f"{sec} 可買已滿 {MAX_PER_SECTOR} 檔（含持倉）", "族群擁擠")
    still = [f for f in packed.values() if (f.get("verdict") or {}).get("key") in ("buy", "plan", "buy_breakout")]
    still.sort(key=lambda x: (x.get("factor_rank") or 0, (x.get("composite") or {}).get("score") or 0), reverse=True)
    for loser in still[MAX_BUYS:]:
        _demote(loser, f"組合可買上限 {MAX_BUYS} 檔", "組合上限")


def _demote(full: dict, reason: str, flag: str) -> None:
    v = full.get("verdict") or {}
    v["key"] = "wait"
    v["title"] = "觀望 · 組合風控"
    v["tone"] = "warn"
    v["reason"] = reason
    flags = list(v.get("flags") or [])
    if flag not in flags:
        flags.append(flag)
    v["flags"] = flags
    full["verdict"] = v
