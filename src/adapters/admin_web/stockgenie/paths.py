"""執行期可寫目錄。部署端 src/ 可能唯讀，快取必須放 run/，不可寫進 stockgenie/cache。"""

from __future__ import annotations

import os
from pathlib import Path


def repo_root() -> Path:
    here = Path(__file__).resolve().parent
    for p in [here, *here.parents]:
        if (p / "src" / "adapters" / "admin_web" / "stockgenie" / "app.py").is_file():
            return p
    return here


def ensure_dir(p: Path) -> Path:
    try:
        p.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return p


def cache_dir() -> Path:
    env = (os.environ.get("STOCKGENIE_CACHE") or "").strip()
    candidates: list[Path] = []
    if env:
        candidates.append(Path(env))
    candidates.append(repo_root() / "run" / "stockgenie-cache")
    candidates.append(Path("/tmp/stockgenie-cache"))
    for p in candidates:
        try:
            p.mkdir(parents=True, exist_ok=True)
            probe = p / ".writable"
            probe.write_text("ok")
            probe.unlink()
            return p
        except OSError:
            continue
    return candidates[-1]
