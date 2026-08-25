#!/usr/bin/env bash
# Disable Tailscale on the deployment host. Run manually on the host that
# owns the service; this project cannot stop a system-wide daemon remotely.
set -euo pipefail

echo '[tailscale] stopping the local Tailscale connection...'
if command -v tailscale >/dev/null 2>&1; then
  tailscale down || true
fi

# Stop only local processes that were launched from this project, if any.
pkill -f 'ts-https-proxy\.py' 2>/dev/null || true
rm -f logs/tailscaled.pid logs/ts-https-proxy.pid
rm -rf tailscale-state

echo '[tailscale] project-level proxy/state has been removed.'
echo '[tailscale] To disable a system service permanently, run on the host:'
echo '  sudo systemctl disable --now tailscaled'
