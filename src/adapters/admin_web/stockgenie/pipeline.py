"""把一檔股票的資料組裝成看板／詳情 payload。"""

from __future__ import annotations

from typing import Any

import pandas as pd

from chips import chip_breakdown
from cyq import compute as cyq_compute
from engine import add_indicators, bars_payload, detect_signals, levels_and_plan, tech_breakdown
from integrity import check_prices
from patterns import detect as detect_candles
from positions import mark as mark_position
from pro import fund_breakdown
from scorer import checklist, composite, resonance, targets_block, verdict
from setups import detect as detect_setups
from universe import meta as uni_meta
from vp import recent_profile


def analyze(
    symbol: str,
    df_raw: pd.DataFrame,
    info: dict,
    t86_series: list[dict] | None,
    rps: int | None,
    include_chart: bool = False,
    regime: dict | None = None,
    sector_rs: int | None = None,
    factor_rank: int | None = None,
    margin: dict | None = None,
    exdiv: dict | None = None,
    monthly: dict | None = None,
    beta: float | None = None,
    official_close: float | None = None,
    apply_holding: bool = True,
) -> dict[str, Any]:
    meta = uni_meta(symbol)
    market = meta["market"]
    df = add_indicators(df_raw)
    tech = tech_breakdown(df)
    fund = fund_breakdown(info or {})
    vol20 = float(df["vol20"].iloc[-1]) if pd.notna(df["vol20"].iloc[-1]) else None
    chips = None
    if market == "TW":
        chips = chip_breakdown(t86_series or [], vol20, margin=margin, ret20=tech.get("ret20"))

    extra = []
    integ = check_prices(df, exdiv=exdiv, official_close=official_close)
    vp40 = recent_profile(df, 40)
    if vp40:
        extra.append((vp40["poc"], "近40日POC"))
        extra.append((vp40["val"], "近40日VAL"))
        extra.append((vp40["vah"], "近40日VAH"))
    cyq = cyq_compute(df, fund.get("shares"))
    if cyq:
        if cyq.get("peak"):
            extra.append((cyq["peak"], "籌碼峰"))
        if cyq.get("avg_cost"):
            extra.append((cyq["avg_cost"], "籌碼均成本"))
        rng = cyq.get("range90") or []
        if len(rng) == 2:
            extra.append((rng[0], "籌碼VAL"))
            extra.append((rng[1], "籌碼VAH"))

    plan = levels_and_plan(df, extra_levels=extra or None, market=market)
    signals = detect_signals(df, plan, tech)
    pats = detect_candles(df)
    for p in pats:
        signals.append({"code": p["code"], "title": p["title"], "detail": p["detail"], "tone": p["tone"]})

    setup = detect_setups(df, plan, tech, rps=rps, patterns=pats)
    setup["integrity"] = integ
    setup["exdiv"] = exdiv
    setup["vp"] = vp40
    setup["monthly"] = monthly
    for s in setup.get("signals") or []:
        signals.append(s)
    if integ.get("issues"):
        signals.append({"code": "INTEGRITY", "title": "價格還原檢查", "detail": "；".join(integ["issues"]), "tone": "warn"})
    if vp40 and vp40.get("defending"):
        signals.append({"code": "VP_DEFEND", "title": "近40日量能防守", "detail": f"POC {vp40['poc']}", "tone": "long"})
    if monthly and monthly.get("yoy") is not None:
        yoy = monthly["yoy"]
        if yoy <= -20:
            signals.append({"code": "REV_DROP", "title": "月營收急縮", "detail": f"年增 {yoy:.1f}%", "tone": "short"})
        elif yoy >= 30:
            signals.append({"code": "REV_SURGE", "title": "月營收強增", "detail": f"年增 {yoy:.1f}%", "tone": "long"})

    if rps is not None and rps >= 90 and next((i["score"] for i in tech["items"] if i["key"] == "均線架構"), 0) >= 10:
        signals.append({"code": "RPS_LEADER", "title": "歐奈爾領導股", "detail": f"RPS {rps}≥90 且趨勢多頭", "tone": "long"})
    if sector_rs is not None and sector_rs >= 80:
        signals.append({"code": "SEC_LEAD", "title": "族群領先", "detail": f"同產業 20 日相對強度 {sector_rs}", "tone": "long"})
    elif sector_rs is not None and sector_rs < 30:
        signals.append({"code": "SEC_LAG", "title": "族群落後", "detail": f"同產業 20 日相對強度 {sector_rs}", "tone": "short"})

    fund_score = fund["score"] if fund.get("available") else None
    chip_score = chips["score"] if chips and chips.get("available") else None
    comp = composite(market, tech["score"], fund_score, chip_score)
    last = {"close": float(df["close"].iloc[-1])}
    checks = checklist(market, last, tech, fund, chips, rps, regime=regime, setups=setup, plan=plan, sector_rs=sector_rs)
    reso = resonance(market, tech["score"], fund_score, chips, fund, last["close"])
    dec = verdict(
        market, comp, tech, fund, chips, plan, reso, signals,
        setups=setup, regime=regime, sector_rs=sector_rs, rps=rps,
    )
    targets = targets_block(last["close"], fund, plan)

    last_dt = df.index[-1].strftime("%Y-%m-%d")
    if apply_holding:
        overlay = mark_position({
            "symbol": symbol,
            "price": last["close"],
            "plan": plan,
            "tech": tech,
            "asof": last_dt,
            "verdict": dec,
        })
        if overlay:
            dec = overlay
    payload = {
        "symbol": symbol,
        "name": meta["name"],
        "sector": meta["sector"],
        "market": market,
        "price": round(last["close"], 4),
        "asof": last_dt,
        "change20": round(float(df["ret20"].iloc[-1]) * 100, 2) if pd.notna(df["ret20"].iloc[-1]) else None,
        "tech": tech,
        "fund": fund,
        "chips": chips,
        "composite": comp,
        "plan": plan,
        "setup": setup,
        "regime": regime,
        "sector_rs": sector_rs,
        "factor_rank": factor_rank,
        "signals": signals,
        "patterns": pats,
        "cyq": cyq,
        "rps": rps,
        "checklist": checks,
        "checks_ok": sum(1 for c in checks if c["ok"]),
        "checks_n": len(checks),
        "resonance": reso,
        "verdict": dec,
        "targets": targets,
        "integrity": integ,
        "vp40": vp40,
        "beta": None if beta is None else round(float(beta), 2),
        "exdiv": exdiv,
        "margin": margin,
        "monthly": monthly,
    }
    if include_chart:
        payload["bars"] = bars_payload(df, 180)
    return payload


def summary_row(full: dict) -> dict:
    plan = full["plan"]
    v = full["verdict"]
    setup = full.get("setup") or {}
    return {
        "symbol": full["symbol"],
        "name": full["name"],
        "sector": full["sector"],
        "market": full["market"],
        "price": full["price"],
        "asof": full["asof"],
        "change20": full["change20"],
        "composite": full["composite"]["score"],
        "bias": full["composite"]["bias"],
        "bias_key": full["composite"]["bias_key"],
        "tech": full["tech"]["score"],
        "fund": full["fund"]["score"] if full["fund"].get("available") else None,
        "chips": None if not full.get("chips") else full["chips"].get("score"),
        "verdict_key": v["key"],
        "verdict": v["title"],
        "tone": v["tone"],
        "rr": v["rr"],
        "buy_mid": v.get("entry") or plan.get("fill_entry") or (None if not plan.get("buy") else plan["buy"]["mid"]),
        "stop": v.get("stop") or plan.get("fill_stop") or plan.get("stop"),
        "target_mid": v.get("target") if v.get("path") == "breakout" else (plan.get("fill_target") or (None if not plan.get("target") else plan["target"]["mid"])),
        "resonance": full["resonance"],
        "rps": full.get("rps"),
        "rvol": setup.get("rvol"),
        "setup_kind": setup.get("kind"),
        "breakout_grade": setup.get("breakout_grade"),
        "vcp": bool((setup.get("contraction") or {}).get("vcp") or (setup.get("contraction") or {}).get("htf")),
        "zone_q": None if not plan.get("buy") else plan["buy"].get("quality"),
        "path": v.get("path"),
        "sector_rs": full.get("sector_rs"),
        "factor_rank": full.get("factor_rank"),
        "checks_ok": full["checks_ok"],
        "checks_n": full["checks_n"],
        "rsi": full["tech"]["rsi"],
    }
