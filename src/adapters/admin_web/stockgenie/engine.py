"""技術面評分、支撐／壓力聚合、買點賣點停損。每個數字對應說明書公式。"""

from __future__ import annotations

import math
from typing import Any

import numpy as np
import pandas as pd


def _clip(x: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, x))


def tick_size(price: float, market: str = "TW") -> float:
    """台股證交所價位表；美股用 0.01。停損／限價都要對齊跳動，不能出現 137.37 這種下不了的價。"""
    p = abs(float(price) or 0.0)
    if (market or "TW").upper() != "TW":
        return 0.01
    if p < 10:
        return 0.01
    if p < 50:
        return 0.05
    if p < 100:
        return 0.10
    if p < 500:
        return 0.50
    if p < 1000:
        return 1.0
    return 5.0


def round_tick(px: float | None, market: str = "TW", mode: str = "nearest") -> float | None:
    if px is None:
        return None
    tick = tick_size(px, market)
    if tick <= 0:
        return round(float(px), 4)
    n = float(px) / tick
    if mode == "down":
        n = math.floor(n + 1e-12)
    elif mode == "up":
        n = math.ceil(n - 1e-12)
    else:
        n = round(n)
    return round(n * tick, 6)


def _friction(market: str) -> float:
    """來回摩擦（相對進場價）。台股：證交稅 0.3%＋手續費＋一點滑價 ≈ 0.45%；美股約 0.2%。"""
    return 0.0045 if (market or "TW").upper() == "TW" else 0.002


def _rr_net(entry: float | None, tgt: float | None, stp: float | None, friction: float) -> float | None:
    """含稅費滑價的 RR。賣不到中點、停損也會滑，所以比紙上 (目標-進場)/(進場-停損) 更接近實盤。"""
    if entry is None or tgt is None or stp is None or entry <= stp:
        return None
    gain = tgt - entry - friction * entry
    risk = entry - stp + 0.25 * friction * entry
    if risk <= 0:
        return None
    if gain <= 0:
        return 0.0
    return round(gain / risk, 2)


def ema(series: pd.Series, span: int) -> pd.Series:
    return series.ewm(span=span, adjust=False).mean()


def wilder(series: pd.Series, period: int) -> pd.Series:
    return series.ewm(alpha=1 / period, adjust=False).mean()


def rsi_wilder(close: pd.Series, period: int = 14) -> pd.Series:
    delta = close.diff()
    gain = delta.clip(lower=0.0)
    loss = (-delta).clip(lower=0.0)
    avg_gain = wilder(gain, period)
    avg_loss = wilder(loss, period)
    rs = avg_gain / avg_loss.replace(0, np.nan)
    return 100 - (100 / (1 + rs))


def macd_hist(close: pd.Series) -> pd.Series:
    line = ema(close, 12) - ema(close, 26)
    signal = ema(line, 9)
    return line - signal


def atr14(df: pd.DataFrame) -> pd.Series:
    high, low, close = df["high"], df["low"], df["close"]
    prev = close.shift(1)
    tr = pd.concat([(high - low), (high - prev).abs(), (low - prev).abs()], axis=1).max(axis=1)
    return wilder(tr, 14)


def add_indicators(df: pd.DataFrame) -> pd.DataFrame:
    out = df.copy()
    out["ma5"] = out["close"].rolling(5).mean()
    out["ma20"] = out["close"].rolling(20).mean()
    out["ma60"] = out["close"].rolling(60).mean()
    out["ma120"] = out["close"].rolling(120).mean()
    out["ma200"] = out["close"].rolling(200).mean()
    out["vol5"] = out["volume"].rolling(5).mean()
    out["vol20"] = out["volume"].rolling(20).mean()
    out["vol60"] = out["volume"].rolling(60).mean()
    out["macd_hist"] = macd_hist(out["close"])
    out["rsi"] = rsi_wilder(out["close"], 14)
    out["atr"] = atr14(out)
    out["ret20"] = out["close"].pct_change(20)
    tp = (out["high"] + out["low"] + out["close"]) / 3.0
    out["vwap20"] = (tp * out["volume"]).rolling(20).sum() / out["volume"].rolling(20).sum()
    out["don_hi"] = out["high"].rolling(20).max()
    out["don_lo"] = out["low"].rolling(20).min()
    out["rvol"] = out["volume"] / out["vol20"].replace(0, np.nan)
    return out


def tech_breakdown(df: pd.DataFrame) -> dict[str, Any]:
    """六個維度，合計 clip 到 ±100。"""
    row = df.iloc[-1]
    close = float(row["close"])
    ma20 = float(row["ma20"]) if pd.notna(row["ma20"]) else close
    ma60 = float(row["ma60"]) if pd.notna(row["ma60"]) else close
    ma20_slope = float(df["ma20"].iloc[-1] - df["ma20"].iloc[-11]) if len(df) > 11 and pd.notna(df["ma20"].iloc[-11]) else 0.0
    ma60_slope = float(df["ma60"].iloc[-1] - df["ma60"].iloc[-21]) if len(df) > 21 and pd.notna(df["ma60"].iloc[-21]) else 0.0

    # ① 均線架構 ±25
    if close > ma20 > ma60 and ma20_slope > 0 and ma60_slope > 0:
        ma_score, ma_label = 25, "強勢多頭（價>MA20>MA60，兩線上彎）"
    elif close > ma20 > ma60:
        ma_score, ma_label = 20, "多頭排列（價>MA20>MA60）"
    elif close > ma60:
        ma_score, ma_label = 10, "站上季線"
    elif close < ma20 < ma60 and ma20_slope < 0 and ma60_slope < 0:
        ma_score, ma_label = -25, "強勢空頭（價<MA20<MA60，兩線下彎）"
    elif close < ma20 < ma60:
        ma_score, ma_label = -20, "空頭排列"
    elif close < ma60:
        ma_score, ma_label = -10, "跌破季線"
    else:
        ma_score, ma_label = 0, "盤整"

    # ② MACD ±20
    hist = df["macd_hist"].dropna()
    h1 = float(hist.iloc[-1]) if len(hist) else 0.0
    h0 = float(hist.iloc[-2]) if len(hist) > 1 else h1
    if h1 > 0 and h1 > h0:
        macd_score, macd_label = 15, "柱狀圖>0 且放大"
    elif h1 > 0:
        macd_score, macd_label = 8, "柱狀圖>0 但縮小"
    elif h1 < 0 and abs(h1) < abs(h0):
        macd_score, macd_label = -8, "柱狀圖<0 收斂"
    elif h1 < 0:
        macd_score, macd_label = -15, "柱狀圖<0 擴大"
    else:
        macd_score, macd_label = 0, "柱狀圖接近 0"

    # ③ RSI ±15
    rsi = float(row["rsi"]) if pd.notna(row["rsi"]) else 50.0
    if 50 <= rsi <= 70:
        rsi_score, rsi_label = 12, "多方動能區 50–70"
    elif 70 < rsi <= 80:
        rsi_score, rsi_label = 4, "偏熱 70–80"
    elif rsi > 80:
        rsi_score, rsi_label = -8, "超買 >80"
    elif 40 <= rsi < 50:
        rsi_score, rsi_label = 0, "中性 40–50"
    elif 30 <= rsi < 40:
        rsi_score, rsi_label = -6, "偏弱 30–40"
    else:
        rsi_score, rsi_label = -10, "超賣 <30"

    # ④ 量能 ±15：20/60 結構 + 近5日 vs 20日 + 近10日陽線量佔比
    vol5 = float(row["vol5"]) if pd.notna(row.get("vol5", np.nan)) else 0.0
    vol20 = float(row["vol20"]) if pd.notna(row["vol20"]) else 0.0
    vol60 = float(row["vol60"]) if pd.notna(row["vol60"]) else 0.0
    ret20 = float(row["ret20"]) if pd.notna(row["ret20"]) else 0.0
    ratio = (vol20 / vol60) if vol60 > 0 else 1.0
    v5r = (vol5 / vol20) if vol20 > 0 else 1.0
    last10 = df.iloc[-10:]
    up_v = float(last10.loc[last10["close"] >= last10["open"], "volume"].sum())
    dn_v = float(last10.loc[last10["close"] < last10["open"], "volume"].sum())
    up_share = up_v / (up_v + dn_v) if (up_v + dn_v) > 0 else 0.5
    dry = 0
    for i in range(1, min(6, len(df))):
        if float(df["volume"].iloc[-i]) < vol20 * 0.85:
            dry += 1
        else:
            break
    if ret20 > 0 and ratio >= 1.15:
        vol_score, vol_label = 13, f"價漲量增（20/60={ratio:.2f}）"
    elif ret20 > 0:
        vol_score, vol_label = 6, "上漲但量能未擴張"
    elif ret20 < 0 and ratio >= 1.15:
        vol_score, vol_label = -12, f"價跌量增（賣壓，20/60={ratio:.2f}）"
    elif ret20 < 0:
        vol_score, vol_label = -4, "下跌量縮"
    else:
        vol_score, vol_label = 0, "量價持平"
    extra_n = []
    if v5r >= 1.3 and ret20 > 0:
        vol_score += 2
        extra_n.append(f"近5日量/20日={v5r:.2f}")
    elif v5r <= 0.7 and ret20 < 0:
        vol_score -= 1
        extra_n.append(f"近5日量萎縮 {v5r:.2f}")
    if up_share >= 0.65 and ret20 > 0:
        vol_score += 2
        extra_n.append(f"陽線量佔比 {up_share:.0%}")
    elif up_share <= 0.35:
        vol_score -= 2
        extra_n.append(f"陰線量佔比 {1-up_share:.0%}")
    if dry >= 3:
        extra_n.append(f"量縮{dry}日")
    vol_score = int(_clip(vol_score, -15, 15))
    if extra_n:
        vol_label = vol_label + "；" + "、".join(extra_n)

    # ⑤ 52週位置（最近 252 根，避免 2 年資料把前高當 52 週）
    win52 = df.iloc[-252:] if len(df) > 252 else df
    low52 = float(win52["low"].min())
    high52 = float(win52["high"].max())
    span = high52 - low52
    pos = (close - low52) / span if span > 0 else 0.5
    if pos > 0.90:
        pos_score, pos_label = 8, f"強勢但靠近高點（52週分位 {pos:.0%}，不宜再當回檔加分）"
    elif pos > 0.70:
        pos_score, pos_label = 10, f"偏強（52週分位 {pos:.0%}）"
    elif pos > 0.40:
        pos_score, pos_label = 2, f"中段（52週分位 {pos:.0%}）"
    elif pos > 0.15:
        pos_score, pos_label = -8, f"偏弱（52週分位 {pos:.0%}）"
    else:
        pos_score, pos_label = -12, f"貼近低點勿摸底（52週分位 {pos:.0%}）"

    # ⑥ 20日動量 ±10
    mom_raw = (ret20 * 100) / 2.0
    mom = _clip(mom_raw, -10, 10)
    mom_label = f"20日 {ret20*100:+.1f}% → clip({mom_raw:.1f}, ±10) = {mom:.1f}"

    total = ma_score + macd_score + rsi_score + vol_score + pos_score + mom
    total = _clip(total, -100, 100)

    rvol = float(row["rvol"]) if pd.notna(row.get("rvol", np.nan)) else None
    return {
        "score": round(total, 1),
        "ma5": float(row["ma5"]) if pd.notna(row.get("ma5", np.nan)) else None,
        "ma20": ma20,
        "ma60": ma60,
        "ma120": float(row["ma120"]) if pd.notna(row.get("ma120", np.nan)) else None,
        "ma200": float(row["ma200"]) if pd.notna(row.get("ma200", np.nan)) else None,
        "rsi": round(rsi, 1),
        "macd_hist": round(h1, 4),
        "atr": float(row["atr"]) if pd.notna(row["atr"]) else None,
        "high52": high52,
        "low52": low52,
        "pos52": round(pos, 4),
        "ret20": round(ret20, 4),
        "vol_ratio": round(ratio, 3),
        "vol5_ratio": round(v5r, 3),
        "up_vol_share": round(up_share, 3),
        "vol_dry_days": dry,
        "rvol": None if rvol is None else round(rvol, 2),
        "vwap20": float(row["vwap20"]) if pd.notna(row.get("vwap20", np.nan)) else None,
        "don_hi": float(row["don_hi"]) if pd.notna(row.get("don_hi", np.nan)) else None,
        "don_lo": float(row["don_lo"]) if pd.notna(row.get("don_lo", np.nan)) else None,
        "ma_score": ma_score,
        "items": [
            {"key": "均線架構", "max": 25, "score": ma_score, "note": ma_label},
            {"key": "MACD動能", "max": 20, "score": macd_score, "note": macd_label},
            {"key": "RSI(14)", "max": 15, "score": rsi_score, "note": rsi_label},
            {"key": "量能結構", "max": 15, "score": vol_score, "note": vol_label},
            {"key": "52週位置", "max": 15, "score": pos_score, "note": pos_label},
            {"key": "20日動量", "max": 10, "score": round(mom, 1), "note": mom_label},
        ],
    }


def _fractals(df: pd.DataFrame, lookback: int = 120, wing: int = 3) -> tuple[list, list]:
    window = df.iloc[-lookback:] if len(df) > lookback else df
    lows, highs = window["low"].to_numpy(), window["high"].to_numpy()
    idx = window.index
    swing_low, swing_high = [], []
    n = len(window)
    for i in range(wing, n - wing):
        sl = lows[i - wing : i + wing + 1]
        sh = highs[i - wing : i + wing + 1]
        if lows[i] == sl.min():
            swing_low.append((float(lows[i]), f"擺動低 {idx[i].date()}"))
        if highs[i] == sh.max():
            swing_high.append((float(highs[i]), f"擺動高 {idx[i].date()}"))
    return swing_low, swing_high


def _dedup(levels: list[tuple[float, str]], tol: float = 0.02) -> list[tuple[float, str]]:
    if not levels:
        return []
    levels = sorted(levels, key=lambda x: x[0])
    out = [levels[0]]
    for px, tag in levels[1:]:
        prev = out[-1][0]
        if abs(px - prev) / max(prev, 1e-9) <= tol:
            out[-1] = ( (out[-1][0] + px) / 2.0, out[-1][1] + "／" + tag )
        else:
            out.append((px, tag))
    return out


def _cluster(levels: list[tuple[float, str]], pct: float = 0.025) -> list[dict]:
    if not levels:
        return []
    levels = sorted(levels, key=lambda x: x[0])
    clusters = []
    for px, tag in levels:
        if clusters:
            c = clusters[-1]
            new_hi = px
            new_lo = c["lo"]
            new_mid = float(np.mean(c["vals"] + [px]))
            # 整檔寬度不得超過 2.5%，避免鏈式合併把 MA20 跟遠端費氏收成一條巨帶
            if (new_hi - new_lo) / max(new_mid, 1e-9) <= pct:
                c["hi"] = new_hi
                c["sources"].append(tag)
                c["vals"].append(px)
                c["mid"] = new_mid
                continue
        clusters.append({"lo": px, "hi": px, "mid": px, "vals": [px], "sources": [tag]})
    for c in clusters:
        c["lo"] = round(min(c["vals"]), 4)
        c["hi"] = round(max(c["vals"]), 4)
        c["mid"] = round(c["mid"], 4)
        del c["vals"]
    return clusters


def _source_family(tag: str) -> str:
    if "擺動" in tag:
        return "swing"
    if tag.startswith("MA") or "週MA" in tag:
        return "ma"
    if "Fib" in tag:
        return "fib"
    if "籌碼" in tag:
        return "cyq"
    if "VWAP" in tag or "唐奇安" in tag:
        return "vp"
    if "52" in tag:
        return "52w"
    return "other"


def _zone_quality(cluster: dict, df: pd.DataFrame) -> tuple[int, int]:
    """
    品質只算「來源家族」不是每條標籤。MA20+VWAP+籌碼VAH 若幾乎同價，只算 2 個家族。
    觸及改為有效彈回：當日刺進區且收陽、收盤回到區中值之上。
    """
    src = cluster.get("sources") or []
    families = { _source_family(s) for s in src }
    n_fam = len(families)
    lo, hi = cluster["lo"], cluster["hi"]
    mid = cluster["mid"]
    window = df.iloc[-120:] if len(df) > 120 else df
    lows = window["low"].to_numpy()
    highs = window["high"].to_numpy()
    close = window["close"].to_numpy()
    open_ = window["open"].to_numpy()
    pierced = (lows <= hi * 1.003) & (highs >= lo * 0.997)
    bounce = pierced & (close > open_) & (close >= mid)
    touches = int(bounce.sum())
    q = min(n_fam, 5) * 16 + min(touches, 6) * 6
    if "cyq" in families:
        q += 10
    if "swing" in families and "ma" in families:
        q += 10
    if "vp" in families and n_fam >= 2:
        q += 4
    if mid >= 50:
        step = 10 if mid >= 200 else 5
        if abs(mid - round(mid / step) * step) / max(mid, 1e-9) < 0.008:
            q += 6
    cluster["n_fam"] = n_fam
    cluster["families"] = sorted(families)
    return min(100, int(q)), touches


def _weekly_ma(df: pd.DataFrame) -> float | None:
    w = df.resample("W-FRI").agg({"close": "last"})
    w = w.dropna()
    if len(w) < 8:
        return None
    span = min(20, max(5, len(w) - 1))
    ma = w["close"].rolling(span).mean().iloc[-1]
    return None if pd.isna(ma) else float(ma)


def levels_and_plan(
    df: pd.DataFrame,
    extra_levels: list[tuple[float, str]] | None = None,
    market: str = "TW",
) -> dict[str, Any]:
    """支撐壓力聚合 → 買區／深檔／目標賣區／停損／突破買點／實盤進場與含成本 RR。"""
    row = df.iloc[-1]
    price = float(row["close"])
    atr = float(row["atr"]) if pd.notna(row["atr"]) else price * 0.02
    win52 = df.iloc[-252:] if len(df) > 252 else df
    high52 = float(win52["high"].max())
    low52 = float(win52["low"].min())
    span = high52 - low52

    swing_low, swing_high = _fractals(df)
    cands: list[tuple[float, str]] = []
    cands.extend(_dedup(swing_low))
    cands.extend(_dedup(swing_high))
    for col, name in (("ma20", "MA20"), ("ma60", "MA60"), ("ma120", "MA120"), ("ma200", "MA200"), ("vwap20", "VWAP20")):
        val = row.get(col)
        if pd.notna(val) and float(val) > 0:
            cands.append((float(val), name))
    if pd.notna(row.get("don_hi")):
        cands.append((float(row["don_hi"]), "唐奇安20高"))
    if pd.notna(row.get("don_lo")):
        cands.append((float(row["don_lo"]), "唐奇安20低"))
    wma = _weekly_ma(df)
    if wma:
        cands.append((wma, "週MA20"))
    if span > 0:
        for r, name in ((0.236, "Fib 23.6%"), (0.382, "Fib 38.2%"), (0.5, "Fib 50%"), (0.618, "Fib 61.8%"), (0.786, "Fib 78.6%")):
            cands.append((low52 + span * r, name))
    cands.append((high52, "52週高"))
    if extra_levels:
        cands.extend(extra_levels)

    supports_raw = [(p, t) for p, t in cands if p <= price * 0.995]
    resist_raw = [(p, t) for p, t in cands if p >= price * 1.005]
    supports = _cluster(supports_raw)
    resists = _cluster(resist_raw)
    for c in supports + resists:
        q, touches = _zone_quality(c, df)
        c["quality"] = q
        c["touches"] = touches
        c["n_src"] = len(c.get("sources") or [])
    supports = sorted(supports, key=lambda c: c["mid"], reverse=True)  # 近→遠
    resists = sorted(resists, key=lambda c: c["mid"])  # 近→遠

    buy = supports[0] if supports else None
    deep = supports[1] if len(supports) >= 2 else None
    target = resists[0] if resists else None
    upgraded = False
    # STOCKGENIE_EDGE_ACCURACY_V1：近壓 <3% 且後面還有檔，改用下一檔當賣點，避免貼前高就減碼。
    if target and (target["mid"] - price) / price < 0.03:
        if len(resists) >= 2:
            target = resists[1]
            upgraded = True

    # STOCKGENIE_REALISTIC_FILL_V1：紙上用區中點會高估 RR。實盤＝區內用現價、
    # 賣在壓力下緣（不是中點）、對齊跳動、扣稅費滑價；停損距現價至少 1×ATR／1.2%，最多 8%。
    friction = _friction(market)
    stop = None
    if buy:
        floor = buy["lo"]
        stop = floor - max(1.2 * atr, floor * 0.02)
        stop = max(stop, floor * 0.82)
        stop = round_tick(stop, market, "down")

    fill_entry = None
    fill_note = ""
    if buy:
        lo, hi, mid = buy["lo"], buy["hi"], buy["mid"]
        if lo <= price <= hi * 1.015:
            fill_entry = price
            fill_note = "已在買區，用現價進（收盤訊號，下一盤開盤可能滑價）"
        elif price > hi:
            fill_entry = hi
            fill_note = "限價等回買區上緣，不在中點幻想成交"
        else:
            fill_entry = mid
            fill_note = "價格尚在區下，等到區再填"
        fill_entry = round_tick(fill_entry, market, "up")

    fill_stop = stop
    if fill_entry and buy:
        floor = buy["lo"]
        struct = floor - 0.5 * atr
        min_away = max(atr, fill_entry * 0.012)
        live = min(struct, fill_entry - min_away)
        live = max(live, fill_entry * 0.92)
        if live >= fill_entry:
            live = fill_entry - min_away
        fill_stop = round_tick(live, market, "down")
        if fill_stop is None or fill_stop >= fill_entry:
            fill_stop = round_tick(fill_entry - min_away, market, "down")

    breakout = None
    if price > high52 * 0.95:
        breakout = round_tick(high52 * 1.005, market, "up")
    turtle_level = None
    if len(df) > 21:
        turtle_level = round_tick(float(df["high"].iloc[-21:-1].max()), market, "nearest")

    buy_mid = buy["mid"] if buy else None
    tgt_mid = target["mid"] if target else None
    fill_target = None
    if target:
        t_lo = target["lo"]
        t_tick = tick_size(t_lo, market)
        fill_target = round_tick(t_lo - t_tick, market, "down")
        if fill_entry and fill_target is not None and fill_target <= fill_entry:
            fill_target = round_tick(tgt_mid, market, "down") if tgt_mid and tgt_mid > fill_entry else None

    rr_plan = _rr_net(buy_mid, tgt_mid, stop, 0.0)
    rr_now = _rr_net(price, tgt_mid, stop, 0.0)
    rr_live = _rr_net(fill_entry, fill_target, fill_stop, friction)
    risk_pct_live = None
    if fill_entry and fill_stop and fill_entry > fill_stop:
        risk_pct_live = round((fill_entry - fill_stop) / fill_entry * 100, 2)

    buy_valid = bool(buy and (buy.get("quality") or 0) >= 40)
    r_multiple = None
    if fill_entry and fill_stop and fill_entry > fill_stop:
        risk_p = fill_entry - fill_stop
        r_multiple = {
            "r1": round_tick(fill_entry + risk_p, market, "nearest"),
            "r2": round_tick(fill_entry + 2 * risk_p, market, "nearest"),
            "r3": round_tick(fill_entry + 3 * risk_p, market, "nearest"),
        }

    don_lo = float(row["don_lo"]) if pd.notna(row.get("don_lo")) else price * 0.94
    bo_entry = breakout or turtle_level
    breakout_plan = None
    if bo_entry:
        if price >= bo_entry:
            bo_fill = price
            bo_fill_note = "已站上，用現價進，不是用突破線當成交價"
        else:
            bo_fill = bo_entry
            bo_fill_note = "限價等收盤站上；跌回不追"
        bo_fill = round_tick(bo_fill, market, "up")
        struct = min(don_lo - 0.2 * atr, bo_fill - 1.8 * atr)
        bo_stop = max(bo_fill * 0.92, struct)
        if bo_stop >= bo_fill:
            bo_stop = bo_fill - max(1.5 * atr, bo_fill * 0.02)
        bo_stop = round_tick(bo_stop, market, "down")
        bo_risk = (bo_fill - bo_stop) if bo_fill and bo_stop else 0
        if bo_risk > 0:
            t1 = round_tick(bo_fill + bo_risk, market, "nearest")
            t2 = round_tick(bo_fill + 2 * bo_risk, market, "nearest")
            t3 = round_tick(bo_fill + 3 * bo_risk, market, "nearest")
            breakout_plan = {
                "entry": bo_fill,
                "stop": bo_stop,
                "t1": t1,
                "t2": t2,
                "t3": t3,
                "rr": _rr_net(bo_fill, t2, bo_stop, friction) or 2.0,
                "risk_pct": round(bo_risk / bo_fill * 100, 2),
                "note": bo_fill_note + "。停損＝結構與 −8% 取較緊；目標 1R/2R/3R 已扣稅費滑價。",
                "fill_note": bo_fill_note,
            }

    trail_note = "走過 1R 後停損上移到 max(原停損, MA20−0.5×ATR)。突破單第一目標只移動停損；1R 後跌破 MA20 或 20 日無進展／滿 40 日出場。"
    ma20 = float(row["ma20"]) if pd.notna(row.get("ma20")) else price
    live_stop = fill_stop or stop
    trail_active = False
    if r_multiple and live_stop and price >= r_multiple["r1"]:
        trailed = max(live_stop, ma20 - 0.5 * atr)
        live_stop = round_tick(max(live_stop, trailed), market, "down")
        trail_active = True
    bo_live_stop = None
    bo_trail_active = False
    if breakout_plan:
        bo_live_stop = breakout_plan["stop"]
        if price >= breakout_plan["t1"]:
            trailed = max(breakout_plan["stop"], ma20 - 0.5 * atr)
            bo_live_stop = round_tick(max(breakout_plan["stop"], trailed), market, "down")
            bo_trail_active = True
            breakout_plan["live_stop"] = bo_live_stop
            breakout_plan["trail_active"] = True
        else:
            breakout_plan["live_stop"] = breakout_plan["stop"]
            breakout_plan["trail_active"] = False

    rr_for_gate = rr_live if rr_live is not None else rr_now
    return {
        "price": round(price, 4),
        "atr": round(atr, 4),
        "high52": round(high52, 4),
        "low52": round(low52, 4),
        "supports": supports[:6],
        "resists": resists[:6],
        "buy": buy,
        "deep": deep,
        "target": target,
        "target_upgraded": upgraded,
        "stop": fill_stop or stop,
        "breakout": breakout,
        "turtle_level": turtle_level,
        "buy_valid": buy_valid,
        "rr_plan": rr_live if rr_live is not None else rr_plan,
        "rr_now": rr_now,
        "rr_live": rr_live,
        "rr_paper": rr_plan,
        "rr_poor": (rr_for_gate is not None and rr_for_gate < 1.5),
        "r_multiple": r_multiple,
        "breakout_plan": breakout_plan,
        "trail_note": trail_note,
        "time_stop_days": 40,
        "live_stop": live_stop,
        "trail_active": trail_active,
        "bo_live_stop": bo_live_stop,
        "bo_trail_active": bo_trail_active,
        "fill_entry": fill_entry,
        "fill_stop": fill_stop,
        "fill_target": fill_target,
        "fill_note": fill_note,
        "risk_pct_live": risk_pct_live,
        "friction": friction,
        "market": market,
    }


def detect_signals(df: pd.DataFrame, plan: dict, tech: dict) -> list[dict]:
    signals = []
    hist = df["macd_hist"].dropna()
    if len(hist) >= 10:
        last10 = hist.iloc[-10:]
        if last10.iloc[0] <= 0 < last10.iloc[-1]:
            signals.append({"code": "MACD_GOLD", "title": "MACD 黃金交叉", "detail": "近 10 日柱狀圖翻紅", "tone": "long"})
        if last10.iloc[0] >= 0 > last10.iloc[-1]:
            signals.append({"code": "MACD_DEAD", "title": "MACD 死亡交叉", "detail": "近 10 日柱狀圖翻綠", "tone": "short"})

    ma20, ma60 = df["ma20"], df["ma60"]
    if len(df) >= 21:
        cross = (ma20 - ma60).iloc[-20:]
        if cross.iloc[0] <= 0 < cross.iloc[-1]:
            signals.append({"code": "MA_GOLD", "title": "均線黃金交叉", "detail": "近 20 日 MA20 上穿 MA60", "tone": "long"})
        if cross.iloc[0] >= 0 > cross.iloc[-1]:
            signals.append({"code": "MA_DEAD", "title": "均線死亡交叉", "detail": "近 20 日 MA20 下穿 MA60", "tone": "short"})

    price = float(df["close"].iloc[-1])
    buy = plan.get("buy")
    ma_item = next((i for i in tech["items"] if i["key"] == "均線架構"), None)
    trend = ma_item["score"] if ma_item else 0
    if buy and trend >= 10:
        dist = (price - buy["hi"]) / price
        inside = buy["lo"] <= price <= buy["hi"]
        above = 0 <= dist <= 0.015
        prior20 = float(df["high"].iloc[-21:-1].max()) if len(df) >= 22 else float(df["high"].max())
        extended = price >= prior20 * 0.985
        if (inside or above) and not extended:
            q = buy.get("quality") or 0
            if q >= 40:
                signals.append({"code": "NEAR_BUY", "title": "回檔接近買區", "detail": f"趨勢分 {trend}≥10，現價距買區上緣 {dist:.1%}，品質 {q}", "tone": "long"})
            else:
                signals.append({"code": "NEAR_BUY_WEAK", "title": "靠近弱支撐", "detail": f"買區品質 {q}<40（來源／觸及不足），不當計劃性買點", "tone": "warn"})

    if plan.get("target_upgraded") and plan.get("breakout"):
        signals.append({"code": "BREAKOUT", "title": "挑戰前高", "detail": f"收盤站上 {plan['breakout']:.2f} 為突破買點（跌回不追）", "tone": "warn"})
    elif plan.get("breakout") and price > plan["high52"] * 0.95:
        signals.append({"code": "NEAR_HIGH", "title": "接近 52 週高", "detail": f"收盤站上 {plan['breakout']:.2f} 才視為突破買點", "tone": "warn"})

    tgt = plan.get("target")
    if tgt:
        dist_r = (tgt["lo"] - price) / price
        last = df.iloc[-1]
        o, h, l, c = float(last["open"]), float(last["high"]), float(last["low"]), float(last["close"])
        rng = h - l
        cloc = (c - l) / rng if rng > 0 else 0.5
        rv = float(last["rvol"]) if "rvol" in last.index and pd.notna(last.get("rvol")) else 1.0
        dist_bar = c < o and cloc < 0.40 and rv >= 1.3
        # 只在真正貼壓（≤1%）或出貨日靠近壓力時喊賣，不再 2.5% 就減碼。
        if dist_r <= 0.01 or (dist_r <= 0.025 and dist_bar):
            signals.append({"code": "NEAR_SELL", "title": "逼近壓力", "detail": f"距賣區 {dist_r:.1%}" + ("，出貨日" if dist_bar else "，建議分批停利"), "tone": "short"})

    rsi = tech.get("rsi") or 50
    if rsi < 30:
        signals.append({"code": "RSI_OS", "title": "RSI 超賣", "detail": f"RSI {rsi:.0f}<30，僅作反彈提示、不是趨勢買點", "tone": "warn"})
    if rsi > 80:
        signals.append({"code": "RSI_OB", "title": "RSI 超買過熱", "detail": f"RSI {rsi:.0f}>80，追高風險升高", "tone": "short"})
    return signals


def _bar_vol(row) -> int:
    try:
        v = row["volume"]
        if v is None or pd.isna(v):
            return 0
        return int(float(v))
    except Exception:
        return 0


def bars_payload(df: pd.DataFrame, n: int = 180) -> list[dict]:
    if df is None or len(df) == 0:
        return []
    tail = df.iloc[-n:]
    out = []
    for ts, row in tail.iterrows():
        try:
            if hasattr(ts, "strftime"):
                t = ts.strftime("%Y-%m-%d")
            else:
                t = str(ts)[:10]
            o = round(float(row["open"]), 4)
            h = round(float(row["high"]), 4)
            l = round(float(row["low"]), 4)
            c = round(float(row["close"]), 4)
            if any(x != x or x in (float("inf"), float("-inf")) for x in (o, h, l, c)):
                continue
            ma = lambda k: None if pd.isna(row.get(k, np.nan)) else round(float(row[k]), 4)
            out.append(
                {
                    "t": t,
                    "o": o,
                    "h": h,
                    "l": l,
                    "c": c,
                    "v": _bar_vol(row),
                    "ma20": ma("ma20"),
                    "ma60": ma("ma60"),
                    "ma120": ma("ma120"),
                }
            )
        except Exception:
            continue
    return out
