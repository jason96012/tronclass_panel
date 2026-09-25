"""台股三大法人籌碼面（證交所 T86）。"""

from __future__ import annotations

from typing import Any


def _clip(x: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, x))


def chip_breakdown(series: list[dict], vol20: float | None, margin: dict | None = None, ret20: float | None = None) -> dict[str, Any]:
    """
    series: 由舊到新的 T86 列，每列含 total / trust / date。
    比重 = 10日淨買超(股) ÷ (20日均量 × 10)
    基礎分 = clip(比重 × 1500, ±60)
    """
    if not series:
        return {
            "score": 0,
            "available": False,
            "note": "無籌碼資料",
            "items": [],
            "net10": 0,
            "net5": 0,
            "trust5": 0,
            "streak_buy": 0,
            "streak_sell": 0,
        }

    last20 = series[-20:]
    last10 = series[-10:]
    last5 = series[-5:]
    last3 = series[-3:]
    net20 = sum(r.get("total") or 0 for r in last20)
    net10 = sum(r.get("total") or 0 for r in last10)
    net5 = sum(r.get("total") or 0 for r in last5)
    trust5 = sum(r.get("trust") or 0 for r in last5)
    foreign10 = sum(r.get("foreign") or 0 for r in last10)
    trust10 = sum(r.get("trust") or 0 for r in last10)
    dealer10 = sum(r.get("dealer") or 0 for r in last10)

    denom = (vol20 or 0) * 10
    weight = (net10 / denom) if denom > 0 else 0.0
    base = _clip(weight * 1500.0, -60, 60)

    streak_buy = 0
    streak_sell = 0
    for r in reversed(series):
        tot = r.get("total") or 0
        if tot > 0 and streak_sell == 0:
            streak_buy += 1
        elif tot < 0 and streak_buy == 0:
            streak_sell += 1
        else:
            break

    extra = 0
    notes = [f"10日淨買超 {net10:,} 股", f"佔10日量 {weight:.2%}", f"基礎分 {base:.0f}"]
    if len(series) >= 15:
        notes.append(f"20日淨買超 {net20:,} 股")
        if net20 > 0 and net10 > 0:
            extra += 4
            notes.append("10日與20日同向淨買（+4）")
        elif net20 < 0 and net10 > 0:
            extra -= 4
            notes.append("20日仍賣、10日轉買（分歧 −4）")
    if streak_buy >= 3:
        extra += 20
        notes.append(f"連續買超 {streak_buy} 天（+20）")
    if streak_sell >= 3:
        extra -= 20
        notes.append(f"連續賣超 {streak_sell} 天（−20）")
    if trust5 > 0 and net5 > 0:
        extra += 10
        notes.append("投信5日與三大法人5日同向淨買（+10）")
    if foreign10 > 0 and trust10 > 0:
        extra += 8
        notes.append("外資＋投信 10 日同向淨買（+8）")
    elif foreign10 < 0 and trust10 < 0:
        extra -= 8
        notes.append("外資＋投信 10 日同向淨賣（−8）")
    if foreign10 < 0 and trust10 > 0:
        extra -= 6
        notes.append("外資賣、投信買（分歧 −6）")
    trust_streak = 0
    for r in reversed(series):
        if (r.get("trust") or 0) > 0:
            trust_streak += 1
        else:
            break
    if trust_streak >= 3:
        extra += 6
        notes.append(f"投信連買 {trust_streak} 日（+6）")
    if margin:
        mchg = margin.get("margin_chg") or 0
        if ret20 is not None and mchg > 0 and ret20 < 0:
            extra -= 8
            notes.append("融資增加且價跌（散戶抬轎 −8）")
        elif mchg < 0 and (ret20 or 0) > 0:
            extra += 4
            notes.append("融資減少且價漲（籌碼轉強 +4）")

    score = _clip(base + extra, -100, 100)
    return {
        "score": round(score, 1),
        "available": True,
        "net20": net20,
        "net10": net10,
        "net5": net5,
        "trust5": trust5,
        "foreign10": foreign10,
        "trust10": trust10,
        "dealer10": dealer10,
        "trust_streak": trust_streak,
        "margin": margin,
        "weight": round(weight, 5),
        "streak_buy": streak_buy,
        "streak_sell": streak_sell,
        "days": [{"date": r["date"], "total": r.get("total") or 0, "trust": r.get("trust") or 0} for r in last10],
        "items": [
            {"key": "10日淨買佔比", "max": 60, "score": round(base, 1), "note": f"比重 {weight:.2%} ×1500"},
            {"key": "連續買賣超", "max": 20, "score": 20 if streak_buy >= 3 else (-20 if streak_sell >= 3 else 0), "note": f"買超連{streak_buy}／賣超連{streak_sell}"},
            {"key": "法人同向", "max": 10, "score": 10 if (trust5 > 0 and net5 > 0) else 0, "note": "投信5日>0 且 三大5日>0"},
            {
                "key": "外資投信",
                "max": 8,
                "score": 8 if (foreign10 > 0 and trust10 > 0) else (-8 if foreign10 < 0 and trust10 < 0 else 0),
                "note": f"外資10日 {foreign10:,}／投信10日 {trust10:,}",
            },
            {
                "key": "融資變化",
                "max": 8,
                "score": (-8 if (margin and (margin.get("margin_chg") or 0) > 0 and (ret20 or 0) < 0) else (4 if (margin and (margin.get("margin_chg") or 0) < 0 and (ret20 or 0) > 0) else 0)),
                "note": f"融資餘額 {None if not margin else margin.get('margin')} 變 {(margin or {}).get('margin_chg')}",
            },
        ],
        "note": "；".join(notes),
    }
