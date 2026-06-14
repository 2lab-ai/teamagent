# llmux demo — recorded with charmbracelet/vhs (https://github.com/charmbracelet/vhs)
#
#   PATH="$PWD/target/release:$HOME/.local/bin:$PATH" \
#   LLMUX_DEMO_CONFIG=/tmp/llmux-demo.json vhs demo/llmux.tape
#
# Prerequisites (kept OUT of this tape because they touch real accounts):
#   - `llmux` on PATH (a release build) and `claude` (real Claude Code) on PATH.
#   - A demo config at $LLMUX_DEMO_CONFIG (default /tmp/llmux-demo.json): a copy of
#     a real config with "proxy.port": 3457 so it never collides with a production
#     daemon on 3456.
#
# Layout: a tmux split — left pane runs REAL Claude Code pointed at llmux, right
# pane is the live llmux server TUI (scheduler + accounts + traffic). Keys are
# scripted; the `claude` and `llmux` processes are real. The tmux uses a private
# socket (`-L lxdemo`) with no config (`-f /dev/null`) so a host tmux-resurrect /
# continuum setup can't restore an unrelated session into the frame.

Output screenshots/llmux-demo.gif

Require tmux
Require llmux
Require claude

Set Shell "bash"
Set FontSize 13
Set Width 1560
Set Height 860
Set Padding 14
Set Framerate 24

# --- setup (hidden): build the two-pane tmux, start both real processes ---
Hide
Type "export LLMUX_CONFIG=${LLMUX_DEMO_CONFIG:-/tmp/llmux-demo.json}; clear"
Enter
Type "tmux -L lxdemo -f /dev/null new-session -d -s d -x 236 -y 56"
Enter
Type "tmux -L lxdemo split-window -h -t d -l 42%"
Enter
# right pane: the live llmux server TUI (binds :3457 from LLMUX_CONFIG, IS the dashboard)
Type "tmux -L lxdemo send-keys -t d:0.1 'llmux server' Enter"
Enter
Sleep 4s
# left pane: real Claude Code, pointed at the llmux proxy
Type "tmux -L lxdemo select-pane -t d:0.0"
Enter
Type "tmux -L lxdemo send-keys -t d:0.0 'ANTHROPIC_BASE_URL=http://localhost:3457 ANTHROPIC_MODEL=claude-sonnet-4-6 claude --dangerously-skip-permissions' Enter"
Enter
Sleep 5s
Type "tmux -L lxdemo attach -t d"
Enter
Show

# --- the actual demo (kept light: a couple of small requests, not a big
#     agentic burst, so it stays under the per-account short rate limit) ---
Sleep 2s
Type@45ms "In one sentence, what does a multi-account LLM proxy with quota-aware scheduling give a Claude Code user? Answer directly without using any tools."
Sleep 700ms
Enter

# Claude works through llmux: the right pane's request counter, active account,
# and token totals move as real traffic flows.
Sleep 24s
# hold the final frame
Sleep 3s
