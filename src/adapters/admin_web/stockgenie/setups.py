"""事件型進場：海龜／RVOL／VCP／高窄旗／均線放量／週線／簡化結構三買。

STOCKGENIE_EDGE_ACCURACY_V1：收緊回檔確認根與 A 級突破，避免普通陽線／1 tick 刺穿當成可買。
"""

from __future__ import annotations

from typing import Any

import pandas as pd

from patterns import upper_wick


def _rvol(df: pd.DataFrame) -> float:
    row = df.iloc[-1]
    vol20 = float(row["vol20"]) if pd.notna(row.get("vol20")) else 0.0
    vol = float(row["volume"]) if pd.notna(row["volume"]) else 0.0
    if vol20 <= 0:
        return 1.0
    return vol / vol20


def weekly_bias(df: pd.DataFrame) -> dict[str, Any]:
    """週線收盤 vs 週 MA20（約 20 週）。1 年日線足夠。"""
    if df is None or len(df) < 40:
        return {"key": "unknown", "label": "週線不足"}
    w = df.resample("W-FRI").agg({"open": "first", "high": "max", "low": "min", "close": "last", "volume": "sum"})
    w = w.dropna(subset=["close"])
    if len(w) < 8:
        return {"key": "unknown", "label": "週線不足"}
    span = min(20, max(5, len(w) - 1))
    w["ma"] = w["close"].rolling(span).mean()
    last = w.iloc[-1]
    prev = w.iloc[-4] if len(w) >= 4 else last
    px = float(last["close"])
    ma = float(last["ma"]) if pd.notna(last["ma"]) else px
    ma_prev = float(prev["ma"]) if pd.notna(prev.get("ma", ma)) else ma
    if px > ma and ma >= ma_prev:
        return {"key": "bull", "label": f"週線多頭（收 {px:.2f} > 週MA{span} {ma:.2f} 上彎）", "ma": round(ma, 4), "close": round(px, 4)}
    if px > ma:
        return {"key": "bull_flat", "label": f"週線偏多（收 > 週MA{span}，均線走平）", "ma": round(ma, 4), "close": round(px, 4)}
    if px < ma and ma < ma_prev:
        return {"key": "bear", "label": f"週線空頭（收 {px:.2f} < 週MA{span} {ma:.2f} 下彎）", "ma": round(ma, 4), "close": round(px, 4)}
    return {"key": "bear_flat", "label": f"週線偏空（收 < 週MA{span}）", "ma": round(ma, 4), "close": round(px, 4)}


def _ranges(df: pd.DataFrame, n: int) -> float:
    sl = df.iloc[-n:]
    c = float(sl["close"].iloc[-1])
    if c <= 0:
        return 0.0
    return float(sl["high"].max() - sl["low"].min()) / c


def contraction(df: pd.DataFrame) -> dict[str, Any]:
    """VCP 收縮 + 高窄旗形（Sequoia HighTightFlag 門檻）。"""
    out = {"vcp": False, "htf": False, "r40": None, "r20": None, "r10": None, "dry": False}
    if len(df) < 50:
        return out
    r40, r20, r10 = _ranges(df, 40), _ranges(df, 20), _ranges(df, 10)
    vol10 = float(df["volume"].iloc[-10:].mean())
    vol20 = float(df["vol20"].iloc[-1]) if pd.notna(df["vol20"].iloc[-1]) else 0.0
    dry = vol20 > 0 and vol10 < 0.7 * vol20
    dry_htf = vol20 > 0 and vol10 < 0.6 * vol20
    out.update({"r40": round(r40, 4), "r20": round(r20, 4), "r10": round(r10, 4), "dry": dry})
    # 歐奈爾／Minervini：波動一波比一波小、量縮；放寬 40 日至少 18%
    out["vcp"] = bool(r40 > r20 > r10 and r10 <= 0.15 and r40 >= 0.18 and dry)
    # 高窄旗：40 日 >60%、近 10 日 <15%、量縮到 0.6×
    out["htf"] = bool(r40 >= 0.60 and r10 <= 0.15 and dry_htf)
    return out


def turtle(df: pd.DataFrame) -> dict[str, Any]:
    """海龜：收盤 > 前 20 日最高 + 陽線且收盤>昨收 + RVOL≥1.0。"""
    if len(df) < 22:
        return {"ok": False}
    prior = float(df["high"].iloc[-21:-1].max())
    row = df.iloc[-1]
    prev = df.iloc[-2]
    close = float(row["close"])
    yang = close > float(row["open"]) and close > float(prev["close"])
    rvol = _rvol(df)
    ok = close > prior and yang and rvol >= 1.0
    return {
        "ok": ok,
        "level": round(prior, 4),
        "yang": yang,
        "rvol": round(rvol, 2),
        "detail": f"前20日高 {prior:.2f}，RVOL {rvol:.2f}，{'陽線收漲' if yang else '非陽線收漲'}",
    }


def ma_volume(df: pd.DataFrame) -> dict[str, Any]:
    """5 上穿 20 且當日量 > 1.5×20 日均量（Sequoia MaVolume）。"""
    if "ma5" not in df.columns or len(df) < 8:
        return {"ok": False}
    ma5, ma20 = df["ma5"], df["ma20"]
    rvol = _rvol(df)
    crossed = False
    if len(df) >= 6:
        diff = (ma5 - ma20).iloc[-5:]
        if pd.notna(diff.iloc[0]) and pd.notna(diff.iloc[-1]) and diff.iloc[0] <= 0 < diff.iloc[-1]:
            crossed = True
    ok = crossed and rvol >= 1.5
    return {"ok": ok, "crossed": crossed, "rvol": round(rvol, 2)}


def structure_third_buy(df: pd.DataFrame) -> dict[str, Any]:
    """
    簡化結構三買（不是完整纏論中樞）：
    近 40 日內、排除最近 8 日，曾收盤創新 20 日高；
    之後 8 日低點不破該段盤整上沿，收盤仍在上沿之上。
    """
    if len(df) < 60:
        return {"ok": False}
    close, high, low = df["close"], df["high"], df["low"]
    brk_i = None
    start = max(21, len(df) - 40)
    end = len(df) - 8
    for i in range(start, end):
        if float(close.iloc[i]) > float(high.iloc[i - 20 : i].max()):
            brk_i = i
    if brk_i is None:
        return {"ok": False}
    z_hi = float(high.iloc[max(0, brk_i - 15) : brk_i].max())
    if float(low.iloc[-8:].min()) < z_hi * 0.985:
        return {"ok": False}
    if float(close.iloc[-1]) < z_hi:
        return {"ok": False}
    return {"ok": True, "pivot": round(z_hi, 4), "detail": f"回測不破盤整上沿 {z_hi:.2f}"}


def breakout_grade(df: pd.DataFrame, rps: int | None, turt: dict, weekly: dict) -> str | None:
    """
    STOCKGENIE_EDGE_ACCURACY_V1
    A：收盤高出前 20 日高至少 0.3%（或貼 52 週高）、陽線、收在振幅上半、
       實體≥0.25×ATR、RVOL≥1.5、RPS≥87、週線非空、站上季線、跳空<5%。
    B：海龜成立但未滿配。1 tick 刺穿／十字線／中性收盤不再當 A 級。
    """
    if len(df) < 22:
        return None
    row = df.iloc[-1]
    close = float(row["close"])
    open_ = float(row["open"])
    prev_c = float(df["close"].iloc[-2])
    yang = close > open_ and close > prev_c
    rvol = _rvol(df)
    high52 = float(df["high"].max())
    prior20 = float(df["high"].iloc[-21:-1].max())
    near52 = close >= high52 * 0.995
    broke20 = close > prior20 * 1.003
    weekly_ok = weekly.get("key") not in ("bear", "bear_flat")
    wick = upper_wick(df)
    close_loc = float(wick.get("close_loc") or 0.5)
    atr = float(row["atr"]) if "atr" in df.columns and pd.notna(row.get("atr")) else abs(close) * 0.02
    body = abs(close - open_)
    ma60 = float(row["ma60"]) if "ma60" in df.columns and pd.notna(row.get("ma60")) else None
    trend_ok = ma60 is None or close > ma60
    gap = (open_ - prev_c) / prev_c if prev_c else 0.0
    if wick.get("long_upper") or close_loc < 0.55:
        if turt.get("ok"):
            return "B"
        return None
    if not yang or not weekly_ok or not trend_ok:
        if turt.get("ok"):
            return "B"
        return None
    if gap > 0.05 or body < 0.25 * max(atr, 1e-9):
        if turt.get("ok"):
            return "B"
        return None
    if (broke20 or near52) and rvol >= 1.5 and rps is not None and rps >= 87:
        return "A"
    if turt.get("ok"):
        return "B"
    return None


def failed_breakout(df: pd.DataFrame) -> bool:
    """昨收站上前高、今收跌回之下：假突破。"""
    if df is None or len(df) < 25:
        return False
    prior = float(df["high"].iloc[-22:-2].max())
    yest = float(df["close"].iloc[-2])
    today = float(df["close"].iloc[-1])
    return yest > prior and today < prior * 0.995


def detect(df: pd.DataFrame, plan: dict, tech: dict, rps: int | None = None, patterns: list | None = None) -> dict[str, Any]:
    rvol = round(_rvol(df), 2)
    weekly = weekly_bias(df)
    turt = turtle(df)
    cont = contraction(df)
    mav = ma_volume(df)
    third = structure_third_buy(df)
    grade = breakout_grade(df, rps, turt, weekly)
    price = float(df["close"].iloc[-1])
    prior20 = float(df["high"].iloc[-21:-1].max()) if len(df) >= 22 else float(df["high"].max())
    # 貼近 20 日高是突破區，不當回檔買點。
    extended = bool(price >= prior20 * 0.985)
    buy = plan.get("buy") or {}
    near_buy = False
    if buy:
        hi = buy.get("hi") or buy.get("mid")
        lo = buy.get("lo") or buy.get("mid")
        if hi and lo:
            dist = (price - hi) / price if price else 1
            inside = lo <= price <= hi
            above = 0 <= dist <= 0.015
            near_buy = bool((inside or above) and not extended)

    confirm_codes = {"HAMMER", "ENGULF_BULL", "MORNING", "THREE_SOLDIERS"}
    candle_ok = bool(any((p.get("code") in confirm_codes) for p in (patterns or [])))
    row = df.iloc[-1]
    prev = df.iloc[-2] if len(df) >= 2 else row
    close = float(row["close"])
    open_ = float(row["open"])
    prev_c = float(prev["close"])
    yang = close > open_ and close > prev_c
    vol_dry = rvol <= 0.8
    gap = (open_ - prev_c) / prev_c if prev_c else 0.0
    wick = upper_wick(df)
    long_upper = bool(wick.get("long_upper"))
    close_loc = float(wick.get("close_loc") or 0.5)
    weak_close = close_loc < 0.40
    held_close = bool(close_loc >= 0.55 and yang)
    # STOCKGENIE_CONFIRM_STRICT_V1：量縮小陽太多弱反彈。確認根只認反轉K。
    pullback_confirm = bool(near_buy and not long_upper and candle_ok)
    confirm = bool(near_buy and candle_ok and not long_upper)
    distribution = bool((not yang) and close_loc < 0.40 and rvol >= 1.3)
    last10 = df.iloc[-10:]
    up_v = float(last10.loc[last10["close"] >= last10["open"], "volume"].sum())
    dn_v = float(last10.loc[last10["close"] < last10["open"], "volume"].sum())
    up_share = up_v / (up_v + dn_v) if (up_v + dn_v) > 0 else 0.5

    signals: list[dict] = []
    if turt.get("ok"):
        signals.append({"code": "TURTLE", "title": "海龜 20 日突破", "detail": turt["detail"], "tone": "long"})
    if grade == "A":
        signals.append({"code": "BO_A", "title": "突破 A 級（放量陽線）", "detail": f"RVOL {rvol}≥1.5，RPS {rps}，收在上半", "tone": "long"})
    elif grade == "B":
        signals.append({"code": "BO_B", "title": "突破未滿配", "detail": "海龜成立但量能或 RPS 未達 A 級", "tone": "warn"})
    if cont.get("vcp"):
        signals.append({"code": "VCP", "title": "波動收縮 VCP", "detail": f"40/20/10 日振幅 {cont['r40']:.0%}/{cont['r20']:.0%}/{cont['r10']:.0%} 且量縮", "tone": "long"})
    if cont.get("htf"):
        signals.append({"code": "HTF", "title": "高窄旗形", "detail": f"40 日漲幅區間 {cont['r40']:.0%} 後 10 日收斂 {cont['r10']:.0%}", "tone": "long"})
    if mav.get("ok"):
        signals.append({"code": "MA_VOL", "title": "均線金叉放量", "detail": f"MA5 上穿 MA20 且 RVOL {rvol}≥1.5", "tone": "long"})
    if third.get("ok"):
        signals.append({"code": "THIRD_BUY", "title": "結構三買（簡化）", "detail": third["detail"], "tone": "long"})
    if rvol >= 1.4:
        signals.append({"code": "RVOL", "title": f"相對量 RVOL {rvol:.2f}", "detail": "當日量 ≥1.4×20 日均量", "tone": "long"})
    elif rvol >= 1.15 and float(tech.get("ret20") or 0) < 0:
        signals.append({"code": "RVOL_DUMP", "title": "價跌量增", "detail": f"RVOL {rvol:.2f} 且 20 日報酬為負", "tone": "short"})
    prev_c_safe = prev_c if prev_c else close
    last_ret = (close / prev_c_safe - 1.0) if prev_c_safe else 0.0
    day_rng = 0.0
    try:
        day_rng = (float(row["high"]) - float(row["low"])) / prev_c_safe if prev_c_safe else 0.0
    except Exception:
        day_rng = 0.0
    limit_lock = bool(abs(last_ret) >= 0.095 or (abs(last_ret) >= 0.07 and day_rng <= 0.012))
    fail_bo = failed_breakout(df)
    if limit_lock:
        signals.append({"code": "LIMIT_LOCK", "title": "漲跌停／鎖死", "detail": f"當日漲跌 {last_ret:.1%}，收盤不是可成交價", "tone": "warn"})
    if fail_bo:
        signals.append({"code": "FAIL_BO", "title": "假突破", "detail": "昨收站上前高、今收跌回，結構未守住", "tone": "short"})
    if distribution:
        signals.append({"code": "DISTRIBUTE", "title": "出貨日", "detail": f"陰線收在振幅下段且 RVOL {rvol}≥1.3", "tone": "short"})
    if long_upper:
        signals.append({"code": "LONG_UPPER", "title": "長上影線", "detail": "上影過長，當根買點剔除", "tone": "short"})
    if weak_close and grade == "A":
        signals.append({"code": "WEAK_CLOSE", "title": "突破收在低檔", "detail": f"收盤僅在當日振幅 {close_loc:.0%} 處，不當突破", "tone": "short"})
    if weekly.get("key") in ("bear", "bear_flat"):
        signals.append({"code": "HTF_BEAR", "title": "週線空頭", "detail": weekly["label"], "tone": "short"})
    elif weekly.get("key") in ("bull", "bull_flat"):
        signals.append({"code": "HTF_BULL", "title": "週線多頭", "detail": weekly["label"], "tone": "long"})

    if grade == "A":
        kind = "breakout"
    elif near_buy:
        kind = "pullback"
    else:
        kind = "watch"

    return {
        "kind": kind,
        "rvol": rvol,
        "weekly": weekly,
        "turtle": turt,
        "contraction": cont,
        "ma_vol": mav,
        "third_buy": third,
        "breakout_grade": grade,
        "near_buy": near_buy,
        "confirm": confirm,
        "pullback_confirm": pullback_confirm,
        "yang": yang,
        "vol_dry": vol_dry,
        "gap": round(float(gap), 4),
        "long_upper": long_upper,
        "weak_close": weak_close,
        "close_loc": round(float(close_loc), 3),
        "held_close": held_close,
        "extended": extended,
        "limit_lock": limit_lock,
        "last_ret": round(float(last_ret), 4),
        "distribution": distribution,
        "up_vol_share": round(float(up_share), 3),
        "failed_breakout": fail_bo,
        "signals": signals,
    }
