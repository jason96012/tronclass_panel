"""基本面評分 + 分析師目標價 + 估值參考價。Yahoo 財報衍生欄位。"""

from __future__ import annotations

import time
from typing import Any


def _num(value, default=None):
    if value is None:
        return default
    try:
        x = float(value)
        if x != x:  # NaN
            return default
        return x
    except Exception:
        return default


def _pct(value) -> float | None:
    """把 0.36 或 36 都收成百分數（36 代表 36%）。"""
    x = _num(value)
    if x is None:
        return None
    if abs(x) <= 1.5:
        return x * 100.0
    return x


def _yield_pct(value) -> float | None:
    """殖利率：0.0112 或 1.12 都收成 1.12。"""
    x = _num(value)
    if x is None:
        return None
    if abs(x) <= 0.2:
        return x * 100.0
    return x


def fund_breakdown(info: dict) -> dict[str, Any]:
    pe = _num(info.get("trailingPE"))
    fpe = _num(info.get("forwardPE"))
    pbr = _num(info.get("priceToBook"))
    roe = _pct(info.get("returnOnEquity"))
    npm = _pct(info.get("profitMargins"))
    rev_g = _pct(info.get("revenueGrowth"))
    earn_g = _pct(info.get("earningsGrowth"))
    if earn_g is None:
        earn_g = _pct(info.get("earningsQuarterlyGrowth"))
    de = _num(info.get("debtToEquity"))
    # Yahoo 有時給 16.5（已是百分比）、有時給 0.16
    if de is not None and de < 3:
        de = de * 100.0
    div = _yield_pct(info.get("dividendYield"))
    payout = _pct(info.get("payoutRatio"))
    fcf = _num(info.get("freeCashflow"))

    items = []
    score = 0.0

    # ① 估值 ±30
    v = 0
    notes = []
    if fpe is None and pe is None:
        v -= 15
        notes.append("虧損或無 PE（−15）")
    else:
        use = fpe if fpe is not None else pe
        if use < 15:
            v += 14
            notes.append(f"預期PE {use:.1f}<15（+14）")
        elif use < 22:
            v += 9
            notes.append(f"預期PE {use:.1f}<22（+9）")
        elif use < 30:
            v += 2
            notes.append(f"預期PE {use:.1f}<30（+2）")
        elif use < 45:
            v -= 5
            notes.append(f"預期PE {use:.1f}<45（−5）")
        else:
            v -= 11
            notes.append(f"預期PE {use:.1f}≥45（−11）")
        if fpe is not None and pe is not None and pe > 0:
            if fpe < pe * 0.85:
                v += 10
                notes.append("預期PE < 靜態PE×0.85（獲利爆發 +10）")
            elif fpe > pe * 1.15:
                v -= 6
                notes.append("預期PE > 靜態PE×1.15（−6）")
    if pbr is not None:
        if pbr < 1.5:
            v += 6
            notes.append(f"PBR {pbr:.2f}<1.5（+6）")
        elif pbr > 15:
            v -= 4
            notes.append(f"PBR {pbr:.1f}>15（−4）")
    items.append({"key": "估值", "max": 30, "score": v, "note": "；".join(notes) or "資料不足"})
    score += v

    # ② 獲利品質 ±25
    q = 0
    qn = []
    if roe is None:
        qn.append("無 ROE")
    elif roe >= 20:
        q += 16
        qn.append(f"ROE {roe:.1f}%≥20（+16）")
    elif roe >= 15:
        q += 12
        qn.append(f"ROE {roe:.1f}%≥15（+12）")
    elif roe >= 10:
        q += 6
        qn.append(f"ROE {roe:.1f}%≥10（+6）")
    elif roe >= 0:
        q -= 4
        qn.append(f"ROE {roe:.1f}%（−4）")
    else:
        q -= 14
        qn.append(f"ROE {roe:.1f}%<0（−14）")
    if npm is not None:
        if npm > 20:
            q += 9
            qn.append(f"淨利率 {npm:.1f}%>20（+9）")
        elif npm > 10:
            q += 5
            qn.append(f"淨利率 {npm:.1f}%>10（+5）")
        elif npm < 0:
            q -= 8
            qn.append(f"淨利率 {npm:.1f}%<0（−8）")
    items.append({"key": "獲利品質", "max": 25, "score": q, "note": "；".join(qn) or "資料不足"})
    score += q

    # ③ 成長 ±25
    g = 0
    gn = []
    if rev_g is None:
        gn.append("無營收年增")
    elif rev_g > 20:
        g += 15
        gn.append(f"營收年增 {rev_g:.1f}%>20（+15）")
    elif rev_g > 10:
        g += 10
        gn.append(f"營收年增 {rev_g:.1f}%>10（+10）")
    elif rev_g > 0:
        g += 3
        gn.append(f"營收年增 {rev_g:.1f}%>0（+3）")
    else:
        g -= 10
        gn.append(f"營收年增 {rev_g:.1f}%<0（−10）")
    if earn_g is None:
        gn.append("無獲利年增")
    elif earn_g > 30:
        g += 10
        gn.append(f"獲利年增 {earn_g:.1f}%>30（+10）")
    elif earn_g > 0:
        g += 5
        gn.append(f"獲利年增 {earn_g:.1f}%>0（+5）")
    else:
        g -= 8
        gn.append(f"獲利年增 {earn_g:.1f}%<0（−8）")
    items.append({"key": "成長動能", "max": 25, "score": g, "note": "；".join(gn)})
    score += g

    # ④ 財務／股息 ±10
    f = 0
    fn = []
    if de is not None:
        if de < 100:
            f += 4
            fn.append(f"負債權益比 {de:.0f}%<100（+4）")
        elif de > 250:
            f -= 5
            fn.append(f"負債權益比 {de:.0f}%>250（−5）")
        else:
            fn.append(f"負債權益比 {de:.0f}%")
    if div is not None and div > 0:
        if 1.5 <= div <= 8 and (payout is None or payout < 90):
            f += 5
            fn.append(f"殖利率 {div:.2f}% 且配息可持續（+5）")
        else:
            f += 2
            fn.append(f"有配息 殖利率 {div:.2f}%（+2）")
    items.append({"key": "財務/股息", "max": 10, "score": f, "note": "；".join(fn) or "資料不足"})
    score += f

    # ⑤ FCF ±10
    c = 0
    if fcf is None:
        cn = "無 FCF 資料"
    elif fcf > 0:
        c = 6
        cn = "自由現金流為正（賺真金白銀 +6）"
    else:
        c = -8
        cn = "自由現金流為負（燒錢 −8）"
    items.append({"key": "自由現金流", "max": 10, "score": c, "note": cn})
    score += c

    score = max(-100.0, min(100.0, score))

    earnings_days = info.get("earnings_days")
    if earnings_days is None:
        earnings_days = _earnings_days(info)
    avg_vol = _num(info.get("averageDailyVolume10Day")) or _num(info.get("averageVolume"))

    rec_key = (info.get("recommendationKey") or "").lower()
    rec_mean = _num(info.get("recommendationMean"))
    n_an = info.get("numberOfAnalystOpinions")
    t_mean = _num(info.get("targetMeanPrice"))
    t_hi = _num(info.get("targetHighPrice"))
    t_lo = _num(info.get("targetLowPrice"))

    return {
        "score": round(score, 1),
        "available": bool(info.get("fetched")),
        "pe": pe,
        "forward_pe": fpe,
        "pbr": pbr,
        "roe": None if roe is None else round(roe, 2),
        "npm": None if npm is None else round(npm, 2),
        "rev_growth": None if rev_g is None else round(rev_g, 2),
        "earn_growth": None if earn_g is None else round(earn_g, 2),
        "debt_equity": None if de is None else round(de, 1),
        "div_yield": None if div is None else round(div, 2),
        "payout": None if payout is None else round(payout, 1),
        "fcf": fcf,
        "fcf_positive": None if fcf is None else fcf > 0,
        "target_mean": t_mean,
        "target_high": t_hi,
        "target_low": t_lo,
        "analyst_count": n_an,
        "rec_key": rec_key,
        "rec_mean": rec_mean,
        "shares": info.get("sharesOutstanding"),
        "earnings_days": earnings_days,
        "avg_volume": avg_vol,
        "items": items,
    }


# 財報窗口：公布日前 EARNINGS_BEFORE 日至後 EARNINGS_AFTER 日（距財報日曆日）。
EARNINGS_BEFORE = 15
EARNINGS_AFTER = 7


def earnings_in_window(ed) -> bool:
    if ed is None:
        return False
    try:
        d = int(ed)
    except Exception:
        return False
    return -EARNINGS_AFTER <= d <= EARNINGS_BEFORE


def valuation_target(price: float, pe, fpe) -> float | None:
    """估值參考價 = 現價 × (靜態PE ÷ 預期PE)。"""
    if not price or not pe or not fpe or fpe == 0:
        return None
    return round(price * (pe / fpe), 4)


def _earnings_days(info: dict) -> int | None:
    """距下次財報的日曆日。已過或無資料 → None。"""
    ts = info.get("earningsTimestamp") or info.get("earningsTimestampStart")
    if ts is None:
        ed = info.get("earningsDate")
        if isinstance(ed, (list, tuple)) and ed:
            ed = ed[0]
        ts = ed
    if ts is None:
        return None
    try:
        if hasattr(ts, "timestamp"):
            unix = ts.timestamp()
        else:
            unix = float(ts)
            if unix > 1e12:
                unix /= 1000.0
        days = int((unix - time.time()) / 86400)
        if days < -EARNINGS_AFTER or days > 400:
            return None
        return days
    except Exception:
        return None


def rec_is_buy(rec_key: str, rec_mean) -> bool:
    if rec_key in ("buy", "strong_buy"):
        return True
    if rec_mean is not None and rec_mean <= 2.5:
        return True
    return False
