#!/bin/sh
# Installs agent/AGENTS.md as the global instructions of every coding agent on
# the laptop, for kirill and root. One canonical copy; tools point to it.
set -eu
HOST=${DS_HOST:-root@192.168.31.235}
cd "$(dirname "$0")/.."
NPU_GUIDE=${NPU_GUIDE:-$HOME/HDD/hexagon-sdk/NPU_GUIDE.md}

ssh "$HOST" 'install -d -o kirill -g users /home/kirill/.config/agents'
scp -q agent/AGENTS.md "$HOST:/home/kirill/.config/agents/AGENTS.md"
[ -f "$NPU_GUIDE" ] && scp -q "$NPU_GUIDE" "$HOST:/home/kirill/.config/agents/NPU_GUIDE.md"
ssh "$HOST" 'set -eu
A=/home/kirill/.config/agents/AGENTS.md
chown kirill:users /home/kirill/.config/agents/*; chmod 644 /home/kirill/.config/agents/*
link() { # link <home> <owner>
    h=$1; o=$2; g=$(id -gn "$o")
    install -d -o "$o" -g "$g" "$h/.codex" "$h/.config/opencode" "$h/.claude"
    ln -sfn "$A" "$h/.codex/AGENTS.md"
    ln -sfn "$A" "$h/.config/opencode/AGENTS.md"
    # Claude Code reads CLAUDE.md and supports @imports. Keep any own lines.
    c="$h/.claude/CLAUDE.md"
    grep -qsF "@$A" "$c" || { printf "@%s\n" "$A" | cat - "$c" 2>/dev/null > "$c.new" || true; mv "$c.new" "$c"; }
    chown -h "$o:$g" "$h/.codex/AGENTS.md" "$h/.config/opencode/AGENTS.md" "$c"
}
link /home/kirill kirill
link /root root
ls -la /home/kirill/.codex/AGENTS.md /home/kirill/.config/opencode/AGENTS.md /root/.codex/AGENTS.md /root/.config/opencode/AGENTS.md
head -3 /home/kirill/.claude/CLAUDE.md /root/.claude/CLAUDE.md'
