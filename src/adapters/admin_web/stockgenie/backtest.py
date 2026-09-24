"""Walk-forward 回測：與看板同一套裁決（無基本面／籌碼，避免前視）。"""

from __future__ import annotations

from typing import Any

import numpy as np
import pandas as pd

from engine import add_indicators, levels_and_plan, tech_breakdown, detect_signals
from integrity import check_prices
from patterns import detect as detect_candles
from regime import detect_regime
from scorer import composite, rps_ranks, verdict
from setups import detect as detect_setups
from universe import meta as uni_meta
from vp import recent_profile


COST = 0.0015


def summarize_trades(arr: list[float]) -> dict[str, Any]:
    """勝率／平均報酬／獲利因子／盈虧比（均賺÷均賠）。"""
    if not arr:
        return {
            "n": 0,
            "win_rate": None,
            "avg": None,
            "profit_factor": None,
            "payoff": None,
            "sum_pct": None,
        }
    w = [t for t in arr if t > 0]
    l = [t for t in arr if t <= 0]
    pf = round(sum(w) / abs(sum(l)), 2) if l and sum(l) != 0 else None
    payoff = None
    if w and l:
        avg_l = abs(float(np.mean(l)))
        if avg_l > 0:
            payoff = round(float(np.mean(w)) / avg_l, 2)
    return {
        "n": len(arr),
        "win_rate": round(len(w) / len(arr) * 100, 1),
        "avg": round(float(np.mean(arr)) * 100, 2),
        "profit_factor": pf,
        "payoff": payoff,
        "sum_pct": round(float(np.sum(arr)) * 100, 2),
    }


def _asof_regime(index_df: pd.DataFrame | None, vix_df: pd.DataFrame | None, ts, breadth: float | None, market: str) -> dict:
    if index_df is None or len(index_df) < 80:
        return {"key": "unknown", "allow_long": True, "label": "無指數"}
    sl = index_df.loc[:ts]
    vx = vix_df.loc[:ts] if vix_df is not None else None
    if len(sl) < 80:
        return {"key": "unknown", "allow_long": True, "label": "指數不足"}
    return detect_regime(market, sl, vx, breadth=breadth)


def _hist_signal(df: pd.DataFrame, i: int, market: str, rps: int | None, sector_rs: int | None, regime: dict) -> dict | None:
    sl = df.iloc[: i + 1]
    if len(sl) < 80 or pd.isna(sl["atr"].iloc[-1]) or pd.isna(sl["ma60"].iloc[-1]):
        return None
    tech = tech_breakdown(sl)
    extra = []
    vp40 = recent_profile(sl, 40)
    if vp40:
        extra.append((vp40["poc"], "近40日POC"))
        extra.append((vp40["val"], "近40日VAL"))
        extra.append((vp40["vah"], "近40日VAH"))
    plan = levels_and_plan(sl, extra_levels=extra or None)
    pats = detect_candles(sl)
    setup = detect_setups(sl, plan, tech, rps=rps, patterns=pats)
    setup["integrity"] = check_prices(sl)
    signals = detect_signals(sl, plan, tech)
    for p in pats:
        signals.append(p)
    for s in setup.get("signals") or []:
        signals.append(s)
    comp = composite(market, tech["score"], None, None)
    fund = {"earnings_days": None, "available": False}
    dec = verdict(
        market, comp, tech, fund, None, plan, False, signals,
        setups=setup, regime=regime, sector_rs=sector_rs, rps=rps,
    )
    if dec["key"] not in ("buy", "plan", "buy_breakout"):
        return None
    path = dec.get("path")
    if path == "breakout" and plan.get("breakout_plan"):
        bp = plan["breakout_plan"]
        return {
            "path": "breakout",
            "entry_ref": bp["entry"],
            "stop": bp["stop"],
            "t1": bp["t1"],
            "t2": bp["t2"],
            "score": tech["score"],
        }
    buy = plan.get("buy") or {}
    stop = plan.get("stop")
    tgt = (plan.get("target") or {}).get("mid")
    if not buy or not stop:
        return None
    risk = buy["mid"] - stop
    return {
        "path": "pullback",
        "entry_ref": buy["mid"],
        "stop": stop,
        "t1": buy["mid"] + risk if risk > 0 else None,
        "t2": tgt,
        "score": tech["score"],
    }


def _exit_day(nxt_low, nxt_high, nxt_close, stop, tgt, hold, trail_stop) -> tuple[float | None, str | None]:
    use_stop = trail_stop if trail_stop is not None else stop
    hit_stop = nxt_low <= use_stop
    hit_tgt = tgt is not None and nxt_high >= tgt
    if hit_stop and hit_tgt:
        return use_stop, "同日止損止盈（記停損）"
    if hit_stop:
        return use_stop, "停損"
    if hit_tgt:
        return tgt, "停利"
    if hold >= 20:
        return nxt_close, "到期"
    return None, None


def run_symbol(
    df_raw: pd.DataFrame,
    market: str,
    index_df: pd.DataFrame | None,
    vix_df: pd.DataFrame | None,
    rps_on_date: dict | None = None,
    sector_on_date: dict | None = None,
) -> dict[str, Any] | None:
    df = add_indicators(df_raw)
    n = len(df)
    start = 250 if n > 700 else 200 if n > 260 else 80
    end = n - 21
    if end <= start:
        return None
    buckets: dict[str, list[float]] = {k: [] for k in ("強多", "溫和多", "中性", "溫和空", "強空")}
    trades: list[float] = []
    paths: list[str] = []
    fills: list[dict] = []
    in_pos = False
    entry_px = stop = t1 = t2 = 0.0
    hold = 0
    path = "pullback"
    trail = None
    entry_ts = None

    def bucket(score: float) -> str:
        if score >= 55:
            return "強多"
        if score >= 30:
            return "溫和多"
        if score > -30:
            return "中性"
        if score > -55:
            return "溫和空"
        return "強空"

    for i in range(start, n - 1):
        ts = df.index[i]
        if i <= end and not pd.isna(df["close"].iloc[i + 20]) and not pd.isna(df["ma60"].iloc[i]):
            try:
                tech = tech_breakdown(df.iloc[: i + 1])
                future = float(df["close"].iloc[i + 20] / df["close"].iloc[i] - 1)
                buckets[bucket(tech["score"])].append(future)
            except Exception:
                pass

        nxt_open = float(df["open"].iloc[i + 1])
        nxt_low = float(df["low"].iloc[i + 1])
        nxt_high = float(df["high"].iloc[i + 1])
        nxt_close = float(df["close"].iloc[i + 1])
        ma20 = float(df["ma20"].iloc[i]) if pd.notna(df["ma20"].iloc[i]) else nxt_close
        atr = float(df["atr"].iloc[i]) if pd.notna(df["atr"].iloc[i]) else nxt_close * 0.02

        if in_pos:
            hold += 1
            if t1 and nxt_high >= t1:
                trail = max(stop, ma20 - 0.5 * atr)
            exit_px, why = _exit_day(nxt_low, nxt_high, nxt_close, stop, t2, hold, trail)
            if exit_px is not None:
                ret = (exit_px / entry_px - 1) - 2 * COST
                trades.append(ret)
                paths.append(path)
                fills.append(
                    {
                        "entry": str(entry_ts)[:10] if entry_ts is not None else "",
                        "exit": str(df.index[i + 1])[:10],
                        "path": "突破" if path == "breakout" else "回檔",
                        "ret": round(ret * 100, 2),
                        "why": why or "",
                    }
                )
                in_pos = False
                trail = None
                entry_ts = None
            continue

        close = float(df["close"].iloc[i])
        ma20_i = float(df["ma20"].iloc[i]) if pd.notna(df["ma20"].iloc[i]) else close
        don_hi = float(df["don_hi"].iloc[i]) if "don_hi" in df.columns and pd.notna(df["don_hi"].iloc[i]) else close
        near_ma = abs(close - ma20_i) / close <= 0.04
        near_hi = close >= don_hi * 0.985
        if not near_ma and not near_hi:
            continue
        rps = None if rps_on_date is None else rps_on_date.get(ts)
        sector_rs = None if sector_on_date is None else sector_on_date.get(ts)
        regime = _asof_regime(index_df, vix_df, ts, None, market)
        try:
            sig = _hist_signal(df, i, market, rps, sector_rs, regime)
        except Exception:
            sig = None
        if not sig:
            continue
        in_pos = True
        path = sig["path"]
        entry_ts = df.index[i + 1] if (i + 1) < n else ts
        entry_px = nxt_open
        stop = sig["stop"]
        if stop >= entry_px:
            stop = entry_px * 0.97
        t1 = sig.get("t1")
        t2 = sig.get("t2")
        hold = 0
        trail = None

    def stats(arr):
        if not arr:
            return {"n": 0, "avg": None}
        a = np.array(arr)
        return {"n": int(len(a)), "avg": round(float(a.mean()) * 100, 2)}

    return {
        "buckets": {k: stats(v) for k, v in buckets.items()},
        "trades": trades,
        "paths": paths,
        "fills": fills,
    }


def pack_symbol_hist(raw: dict[str, Any] | None) -> dict[str, Any]:
    """個股詳情用：若照系統買賣點進出的歷史模擬摘要。"""
    empty = summarize_trades([])
    empty["fills"] = []
    empty["pullback"] = summarize_trades([])
    empty["breakout"] = summarize_trades([])
    if not raw:
        return empty
    trades = raw.get("trades") or []
    paths = raw.get("paths") or []
    out = summarize_trades(trades)
    pb = [t for t, p in zip(trades, paths) if p != "breakout"]
    bo = [t for t, p in zip(trades, paths) if p == "breakout"]
    out["pullback"] = summarize_trades(pb)
    out["breakout"] = summarize_trades(bo)
    out["fills"] = list(raw.get("fills") or [])[-8:]
    return out


def _cross_section_ranks(prepared: dict[str, pd.DataFrame]) -> tuple[dict, dict]:
    """每個日期：RPS、族群 RS。"""
    rps_map: dict[str, dict] = {}  # symbol -> {ts: rank}
    sec_map: dict[str, dict] = {}
    all_dates = sorted({d for df in prepared.values() for d in df.index[200:]})
    sectors = {s: uni_meta(s)["sector"] for s in prepared}
    for ts in all_dates:
        rets = {}
        sec_rets: dict[str, list] = {}
        for s, df in prepared.items():
            if ts not in df.index:
                continue
            loc = df.index.get_loc(ts)
            if isinstance(loc, slice):
                continue
            if loc >= 120:
                rets[s] = float(df["close"].iloc[loc] / df["close"].iloc[loc - 120] - 1)
            elif loc >= 20:
                rets[s] = float(df["close"].iloc[loc] / df["close"].iloc[0] - 1)
            else:
                continue
            sec_rets.setdefault(sectors.get(s) or "", []).append((s, rets[s]))
        ranks = rps_ranks(rets)
        for s, r in ranks.items():
            rps_map.setdefault(s, {})[ts] = r
        for sec, items in sec_rets.items():
            items.sort(key=lambda kv: kv[1])
            n = len(items)
            for i, (s, _) in enumerate(items):
                sec_map.setdefault(s, {})[ts] = max(1, min(99, int(round((i / max(n - 1, 1)) * 98 + 1))))
    return rps_map, sec_map


def run_market(
    ohlcv: dict[str, pd.DataFrame],
    index_df: pd.DataFrame | None = None,
    market: str = "TW",
    vix_df: pd.DataFrame | None = None,
) -> dict[str, Any]:
    prepared = {s: add_indicators(df) for s, df in ohlcv.items() if df is not None and len(df) >= 100}
    rps_map, sec_map = _cross_section_ranks(prepared)
    buckets = {k: {"n": 0, "sum": 0.0} for k in ("強多", "溫和多", "中性", "溫和空", "強空")}
    all_trades: list[float] = []
    pb, bo = [], []
    used = 0
    for s, df in prepared.items():
        r = run_symbol(
            df, market, index_df, vix_df,
            rps_on_date=rps_map.get(s),
            sector_on_date=sec_map.get(s),
        )
        if not r:
            continue
        used += 1
        for k, v in r["buckets"].items():
            if v["n"]:
                buckets[k]["n"] += v["n"]
                buckets[k]["sum"] += (v["avg"] or 0) / 100 * v["n"]
        all_trades.extend(r["trades"])
        for ret, p in zip(r["trades"], r["paths"]):
            (bo if p == "breakout" else pb).append(ret)

    def summarize(arr):
        if not arr:
            return {"n": 0, "win_rate": None, "avg": None, "profit_factor": None}
        w = [t for t in arr if t > 0]
        l = [t for t in arr if t <= 0]
        pf = round(sum(w) / abs(sum(l)), 2) if l and sum(l) != 0 else None
        return {
            "n": len(arr),
            "win_rate": round(len(w) / len(arr) * 100, 1),
            "avg": round(float(np.mean(arr)) * 100, 2),
            "profit_factor": pf,
        }

    out_b = {}
    for k, v in buckets.items():
        out_b[k] = {"n": v["n"], "avg": None if v["n"] == 0 else round(v["sum"] / v["n"] * 100, 2)}
    return {
        "symbols": used,
        "buckets": out_b,
        "strategy": summarize(all_trades),
        "pullback": summarize(pb),
        "breakout": summarize(bo),
        "rules": (
            "與看板同一套裁決（無基本面／籌碼，避免前視）。"
            "回檔：買區品質≥40、在區內、確認根、RR≥1.5、大盤／週線允許多、族群RS≥40；"
            "突破：A 級放量陽線。次日開盤進場；走過 1R 後停損上移到 MA20−0.5×ATR；"
            "目標 2R 或壓力帶；最長 20 日；雙邊成本 0.15%。"
        ),
        "limits": "樣本約五年（不足則用已載入日線）、觀察窗重疊；不含法人與財報。長上影／價格還原／週線空頭／大盤空頭會擋單。不是實盤保證。",
    }
