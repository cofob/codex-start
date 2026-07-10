# Debian's /etc/profile resets PATH for login shells. Restore the tool locations
# supplied by codex-start so `codex-start shell` matches non-login workloads.
PATH="/home/codex/.venv/bin:/home/codex/.cargo/bin:/home/codex/.local/bin:/usr/local/cargo/bin:${PATH}"
export PATH
