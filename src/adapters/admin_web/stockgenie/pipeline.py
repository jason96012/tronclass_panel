"""把一檔股票的資料組裝成看板／詳情 payload。"""

from __future__ import annotations

from typing import Any

import pandas as pd

from chips import chip_breakdown
from cyq import compute as cyq_compute
from engine import add_indicators, bars_payload, detect_signals, levels_and_plan, tech_breakdown
from integrity import align_last_to_official, check_prices
from patterns import detect as detect_candles
from positions import mark as mark_position
from pro import fund_breakdown
from forecast import analog_guard
from scorer import apply_gate_veto, apply_quality_veto, checklist, composite, resonance, targets_block, verdict
from setups import detect as detect_setups
from universe import meta as uni_meta
from vp import recent_profile


def _asof_of(df) -> str | None:
    try:
        return df.index[-1].strftime("%Y-%m-%d")
    except Exception:
        return None


def _px(v) -> float | None:
    try:
        x = float(v)
    except (TypeError, ValueError):
        return None
    if x != x:
        return None
    return round(x, 4)


def _as_dict(v) -> dict:
    return v if isinstance(v, dict) else {}


def _mid(v) -> float | None:
    if isinstance(v, dict):
        return _px(v.get("mid"))
    return _px(v)


def build_timing(
    *,
    live_ok: bool,
    hard: bool,
    dec: dict | None,
    plan: dict | None,
    setup: dict | None,
    gate_fail: list | None,
    guard: dict | None,
    price: float | None,
) -> dict[str, Any]:
    """STOCKGENIE_TIMING_V1：用人話講何時買、何時賣。不是委託單。"""
    dec = dec or {}
    plan = plan or {}
    setup = setup or {}
    guard = guard or {}
    fails = [x for x in (gate_fail or []) if x]
    key = dec.get("key") or ""
    path = dec.get("path") or setup.get("kind") or "none"
    gap = plan.get("gap_abort_pct") if plan.get("gap_abort_pct") is not None else 0.015
    limit = _px(plan.get("fill_entry")) or _mid(plan.get("buy")) or _px(dec.get("entry"))
    zone = _as_dict(plan.get("buy"))
    zlo, zhi = _px(zone.get("lo")), _px(zone.get("hi"))
    stop = (
        _px(plan.get("live_stop"))
        or _px(plan.get("fill_stop"))
        or _px(plan.get("stop"))
        or _px(dec.get("stop"))
    )
    tgt = _px(plan.get("fill_target")) or _mid(plan.get("target")) or _px(dec.get("target"))
    abort = f"下一盤開盤相對限價跳空≥{gap:.1%}則取消，禁止市價追。"
    holding = bool(dec.get("hold"))

    buy = {
        "do": False,
        "state": "blocked",
        "title": "現在不要買",
        "when": "條件未齊，空手。",
        "price": None,
        "zone": None if zlo is None else f"{zlo}–{zhi}",
        "abort": abort,
        "why": dec.get("reason") or "",
    }
    if hard or key == "error_data":
        buy["when"] = "資料異常，任何價都不買。"
        buy["why"] = "日K與證交所對不上或未還原，買賣點不可用。"
    elif holding:
        buy["state"] = "held"
        buy["title"] = "已持有，不再加買"
        buy["when"] = "持倉只管理停損與賣點，不每天重貼可買。"
        buy["why"] = dec.get("reason") or buy["when"]
    elif live_ok and key in ("buy", "plan"):
        buy.update({
            "do": True,
            "state": "now_limit",
            "title": "可以掛限價買（不是市價）",
            "when": f"下一交易日開盤集合或盤中，限價 {limit if limit is not None else '買區上緣'}。"
            + (f" 買區 {zlo}–{zhi}。" if zlo is not None else "")
            + abort,
            "price": limit,
            "why": dec.get("reason") or "回檔確認根與閘門已過。",
        })
    elif live_ok and key == "buy_breakout":
        bo = _as_dict(plan.get("breakout_plan")).get("entry")
        buy.update({
            "do": True,
            "state": "now_breakout",
            "title": "突破已確認，可以掛限價（收盤站上才算）",
            "when": f"下一交易日限價 { _px(bo) or limit }。跌回不追。" + abort,
            "price": _px(bo) or limit,
            "why": dec.get("reason") or "A 級突破。",
        })
    elif setup.get("near_buy") and not setup.get("pullback_confirm"):
        buy.update({
            "state": "wait_confirm",
            "title": "現在不要買 · 等確認根",
            "when": "價格已靠近買區，等鎚頭／陽包陰／晨星那一根收盤之後再看。普通陽線、量縮小陽都不算。",
            "price": None,
            "why": "缺確認根。",
        })
    elif key in ("wait_pullback",) or (not live_ok and zlo is not None and path in ("pullback", "watch", "none")):
        buy.update({
            "state": "wait_zone",
            "title": "現在不要買 · 等回檔",
            "when": (
                f"等收盤回到買區 {zlo}–{zhi}，且出現反轉K，且閘門全過。"
                if zlo is not None else "等回檔到有品質的買區並出現確認根。"
            ),
            "price": None,
            "why": "；".join(fails) if fails else (dec.get("reason") or "現價不在買區或閘門未過。"),
        })
    else:
        why_bits = fails or ([guard.get("conflict_why")] if guard.get("conflict_why") else [])
        if guard.get("no_edge") or guard.get("small_sample"):
            why_bits.append("歷史路徑無優勢")
        buy["when"] = "現在不要買。"
        buy["why"] = "；".join([str(x) for x in why_bits if x]) or (dec.get("reason") or "訊號未對齊。")

    sell = {
        "do": False,
        "state": "none",
        "title": "未持有，不用賣",
        "when": "沒有部位就沒有賣出動作。",
        "stop": stop,
        "target": tgt,
        "why": "",
    }
    if hard or key == "error_data":
        sell["when"] = "資料異常，不給賣點。"
        sell["stop"] = None
        sell["target"] = None
    elif holding:
        if key in ("exit_stop",):
            sell.update({
                "do": True,
                "state": "stop_now",
                "title": "現在賣 · 停損",
                "when": f"現價已到停損 {stop}。以能成交的價出場，不要等反彈。",
                "why": dec.get("reason") or "停損",
            })
        elif key in ("exit_tgt", "exit_trail", "exit_time"):
            sell.update({
                "do": True,
                "state": "exit_now",
                "title": dec.get("title") or "現在賣 · 出場",
                "when": dec.get("reason") or f"觸及賣點或時間停損。目標 {tgt}／停損 {stop}。",
                "why": dec.get("reason") or "",
            })
        elif key == "trim":
            sell.update({
                "do": True,
                "state": "trim",
                "title": "若持有：減碼",
                "when": f"現價靠近壓力。賣在 {tgt} 附近（壓力下緣減一跳），不是全部一筆出完。停損仍是 {stop}。",
                "why": dec.get("reason") or "逼近壓力",
            })
        else:
            sell.update({
                "state": "hold_manage",
                "title": "持有中 · 還沒到賣點",
                "when": (
                    f"跌到停損 {stop} 就賣。"
                    + (f" 漲到 {tgt} 減碼／停利。" if tgt else "")
                    + " 走過 1R 後停損上移；20 日無進展或滿 40 日出場。"
                ),
                "why": "持倉只管理停損，不每天重判可買。",
            })
    else:
        sell["when"] = (
            "未持有，不用賣。"
            + (f" 若之後買進：跌到 {stop} 停損賣。" if stop else "")
            + (f" 漲到 {tgt} 減碼／停利。" if tgt else "")
            + " 不要把預測錐中位當賣點。"
        )
        sell["why"] = "賣點只對持倉有意義。"
        if key == "trim":
            sell["title"] = "未持有；若已有部位則減碼"
            sell["when"] = f"未持有就空手。若你已經有這檔：現價靠近壓力，賣在 {tgt}，停損 {stop}。"

    return {"buy": buy, "sell": sell}


def _mismatch_payload(
    symbol: str,
    meta: dict,
    df_raw,
    integ: dict,
    info: dict | None,
    t86_series,
    rps,
    regime,
    margin,
    exdiv,
    monthly,
    beta,
    include_chart: bool,
) -> dict[str, Any]:
    """報價硬阻斷：不產出 MA／RSI／ATR／買賣區／預測。"""
    fund = fund_breakdown(info or {})
    chips = None
    market = meta.get("market") or "TW"
    try:
        last = float(df_raw["close"].iloc[-1])
    except Exception:
        last = None
    reason = "資料源未完成除權息/減資還原，停止產出技術訊號。"
    detail = "；".join(integ.get("issues") or []) or reason
    payload = {
        "status": "DATA_MISMATCH",
        "live_ok": False,
        "gate_fail": ["價格還原正常"],
        "symbol": symbol,
        "name": meta.get("name") or symbol,
        "sector": meta.get("sector") or "",
        "market": market,
        "price": None if last is None else round(last, 4),
        "asof": _asof_of(df_raw),
        "change20": None,
        "tech": {"score": 0, "items": [], "rsi": None, "withheld": True},
        "fund": fund,
        "chips": chips,
        "composite": {"score": 0, "bias": "資料異常", "bias_key": "error"},
        "plan": {"live_ok": False, "sandbox": False, "hard_stop": True, "buy": None, "stop": None, "target": None},
        "setup": {"integrity": integ},
        "regime": regime,
        "sector_rs": None,
        "factor_rank": None,
        "signals": [{"code": "DATA_MISMATCH", "title": "資料源不一致", "detail": detail, "tone": "warn"}],
        "patterns": [],
        "cyq": None,
        "rps": rps,
        "checklist": [
            {"key": "價格還原正常", "ok": False, "detail": detail, "group": "gate"},
        ],
        "checks_ok": 0,
        "checks_n": 1,
        "resonance": False,
        "verdict": {
            "key": "error_data",
            "title": "ERROR · 資料源不一致",
            "tone": "bad",
            "reason": reason,
            "flags": ["DATA_MISMATCH"],
            "entry": None,
            "stop": None,
            "target": None,
            "rr": None,
            "path": "blocked",
        },
        "targets": {
            "analyst": {"mean": None, "count": 0, "high": None, "low": None, "upside_pct": None},
            "valuation": None,
            "disclaimer": reason,
        },
        "integrity": integ,
        "vp40": None,
        "beta": None if beta is None else round(float(beta), 2),
        "exdiv": exdiv,
        "margin": margin,
        "monthly": monthly,
        "forecast": {"ok": False, "bias_label": "資料源不一致，停止產出技術訊號"},
        "hist": {"n": 0},
    }
    payload["timing"] = build_timing(
        live_ok=False, hard=True, dec=payload["verdict"], plan=payload["plan"],
        setup={}, gate_fail=["價格還原正常"], guard={}, price=last,
    )
    if include_chart:
        try:
            payload["bars"] = bars_payload(df_raw, 180)
        except Exception:
            payload["bars"] = []
    return payload


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
    official_asof: str | None = None,
    apply_holding: bool = True,
) -> dict[str, Any]:
    meta = uni_meta(symbol)
    market = meta["market"]
    integ = check_prices(df_raw, exdiv=exdiv, official_close=official_close, official_asof=official_asof)
    # STOCKGENIE_QUOTE_HARDSTOP_V1：報價對不上證交所 → 不跑 MA／RSI／ATR／買賣區。
    if integ.get("hard_stop"):
        return _mismatch_payload(
            symbol, meta, df_raw, integ, info, t86_series, rps, regime,
            margin, exdiv, monthly, beta, include_chart,
        )
    work = align_last_to_official(df_raw, official_close, official_asof)
    df = add_indicators(work)
    tech = tech_breakdown(df)
    fund = fund_breakdown(info or {})
    vol20 = float(df["vol20"].iloc[-1]) if pd.notna(df["vol20"].iloc[-1]) else None
    chips = None
    if market == "TW":
        chips = chip_breakdown(t86_series or [], vol20, margin=margin, ret20=tech.get("ret20"))

    extra = []
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
    # STOCKGENIE_GATE_VETO_V1：任一核心閘門 ✕ → 否決實盤進場，改觀望。
    dec, live_ok, gate_fail = apply_gate_veto(checks, dec)
    guard = {}
    try:
        guard = analog_guard(df, regime=regime, weekly=(setup or {}).get("weekly"))
    except Exception:
        guard = {}
    q_reasons = []
    if guard.get("no_edge") or guard.get("small_sample"):
        q_reasons.append("歷史路徑無邊際優勢（N<50／頻率／Wilson／中位報酬）")
    if guard.get("conflict"):
        q_reasons.append(guard.get("conflict_why") or "樣本與大盤／週線衝突")
    if guard.get("regime_shift"):
        q_reasons.append("波動狀態已切換，舊樣本不適用")
    dec, live_ok = apply_quality_veto(dec, live_ok, q_reasons)
    plan = dict(plan or {})
    plan["live_ok"] = live_ok
    plan["sandbox"] = not live_ok
    plan["friction_note"] = "來回摩擦已含稅費、點差與隔夜缺口，不再用 0.45% 低估"
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
    timing = build_timing(
        live_ok=live_ok, hard=False, dec=dec, plan=plan, setup=setup,
        gate_fail=gate_fail, guard=guard, price=last["close"],
    )
    payload = {
        "status": "OK",
        "live_ok": live_ok,
        "timing": timing,
        "gate_fail": gate_fail,
        "guard": guard,
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
        "rr": None if not full.get("live_ok") else v.get("rr"),
        "buy_mid": None if not full.get("live_ok") else (v.get("entry") or plan.get("fill_entry") or (None if not plan.get("buy") else plan["buy"]["mid"])),
        "stop": None if not full.get("live_ok") else (v.get("stop") or plan.get("fill_stop") or plan.get("stop")),
        "target_mid": None if not full.get("live_ok") else (v.get("target") if v.get("path") == "breakout" else (plan.get("fill_target") or (None if not plan.get("target") else plan["target"]["mid"]))),
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
        "live_ok": bool(full.get("live_ok")),
        "buy_when": ((full.get("timing") or {}).get("buy") or {}).get("title"),
        "sell_when": ((full.get("timing") or {}).get("sell") or {}).get("title"),
    }
