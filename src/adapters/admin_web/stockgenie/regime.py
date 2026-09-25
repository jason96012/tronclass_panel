"""大盤狀態（regime）：指數相對 200 日線、VIX。空頭市禁止可買。"""

from __future__ import annotations

from typing import Any

import pandas as pd


def detect_regime(
    market: str,
    index_df: pd.DataFrame | None,
    vix_df: pd.DataFrame | None = None,
    breadth: float | None = None,
) -> dict[str, Any]:
    """
    bull    : 指數收盤 > MA200
    caution : 收盤在 MA120–MA200 之間，或 VIX 高於 20 日均 ×1.1
    bear    : 收盤 < MA120，或 < MA200 且 MA200 下彎
    unknown : 沒有指數日線時不攔截（allow_long=True）
    """
    if index_df is None or len(index_df) < 80:
        return {
            "key": "unknown",
            "allow_long": True,
            "label": "大盤資料不足，不攔截個股",
            "index": None,
            "ma120": None,
            "ma200": None,
            "vix": None,
            "vix_hot": False,
            "symbol": None,
            "breadth": breadth,
        }

    close = index_df["close"]
    px = float(close.iloc[-1])
    ma120 = float(close.rolling(120).mean().iloc[-1]) if len(close) >= 120 else None
    ma200 = float(close.rolling(200).mean().iloc[-1]) if len(close) >= 200 else None
    ma200_slope = 0.0
    if ma200 is not None and len(close) >= 221:
        prev = float(close.rolling(200).mean().iloc[-21])
        ma200_slope = ma200 - prev

    vix_px = None
    vix_hot = False
    if vix_df is not None and len(vix_df) >= 21:
        vix_px = float(vix_df["close"].iloc[-1])
        vix_sma = float(vix_df["close"].rolling(20).mean().iloc[-1])
        vix_hot = vix_sma > 0 and vix_px > vix_sma * 1.1

    if ma200 is not None and px > ma200:
        key = "bull"
        label = f"大盤多頭（收盤 {px:.0f} > MA200 {ma200:.0f}）"
    elif ma120 is not None and px > ma120:
        key = "caution"
        label = f"大盤警戒（收盤 {px:.0f} 在 MA120–MA200 之間）"
    elif ma200 is not None and px < ma200 and ma200_slope < 0:
        key = "bear"
        label = f"大盤空頭（收盤 {px:.0f} < MA200 {ma200:.0f} 且 200 日線下彎）"
    elif ma120 is not None and px < ma120:
        key = "bear"
        label = f"大盤空頭（收盤 {px:.0f} < MA120 {ma120:.0f}）"
    else:
        key = "caution"
        label = "大盤資料不足長均線，列為警戒"

    if vix_hot and key == "bull":
        key = "caution"
        label = f"VIX {vix_px:.1f} 高於 20 日均 ×1.1，大盤改警戒"
    elif vix_hot and key != "bear":
        key = "caution"
        label += f"；VIX {vix_px:.1f} 偏熱"

    if breadth is not None and breadth < 0.40 and key == "bull":
        key = "caution"
        label += f"；站上MA20僅 {breadth:.0%}，廣度不足"

    return {
        "key": key,
        "allow_long": key in ("bull", "unknown"),
        "label": label,
        "index": round(px, 2),
        "ma120": None if ma120 is None else round(ma120, 2),
        "ma200": None if ma200 is None else round(ma200, 2),
        "vix": None if vix_px is None else round(vix_px, 2),
        "vix_hot": vix_hot,
        "symbol": "^TWII" if market == "TW" else "^GSPC",
        "market": market,
        "breadth": None if breadth is None else round(float(breadth), 3),
    }
