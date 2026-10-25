#!/usr/bin/env bash
# A deterministic TUI double for the P0-2 PTY probe.
#
# It reproduces, without a model or a network, the four things a real TUI does
# that a screen scraper has to survive:
#
#   1. alternate screen        — the output is gone the moment the program exits
#   2. colour + chrome         — SGR sequences that `contents()` throws away
#   3. bracketed paste         — the markers arrive *in* the input line
#   4. a mid-answer pause      — 900 ms of silence in the middle of a sentence,
#                                which is longer than the 800 ms idle guess
#
# The pause is the point: it is the cheapest way to prove an idle-timeout turn
# detector cuts an answer in half, every time, not just under load.
#
#   bash probes/fixtures/tui/fake-tui.sh
set -u

dim=$'\033[2m'
bold=$'\033[1m'
cyan=$'\033[36m'
green=$'\033[32m'
reset=$'\033[0m'

printf '\033[?1049h'   # alternate screen
printf '\033[?25l'     # hide cursor
printf '\033[?2004h'   # enable bracketed paste
printf '\033[H\033[2J'

printf '%s╭─ fake-tui ────────────────────────────────────────────────╮%s\n' "$dim" "$reset"
printf '%s│ a deterministic double: no model, no network, fixed gaps │%s\n' "$dim" "$reset"
printf '%s╰───────────────────────────────────────────────────────────╯%s\n' "$dim" "$reset"
printf '\n%s  press Enter to submit · Ctrl-D to quit%s\n\n' "$dim" "$reset"

prompt() {
    printf '%s›%s ' "$cyan" "$reset"
}

prompt

if ! IFS= read -r line; then
    printf '\n%sexit%s\n' "$dim" "$reset"
    printf '\033[?2004l\033[?25h\033[?1049l'
    exit 0
fi

# The harness is told to paste; a program that receives the markers has to strip
# them itself. Print what actually arrived, with escapes made visible.
printf '%s  input mode: echo=%s bracketed-paste=on%s\n' "$dim" "$(stty -a </dev/tty 2>/dev/null | grep -o -- '-\?echo ' | head -1 | tr -d ' ')" "$reset"
printf '%s  received: %s%s\n' "$dim" "$(printf '%s' "$line" | cat -v)" "$reset"

start=$(date +%s%N 2>/dev/null || echo 0)

printf '%s  ⋯ thinking%s\n' "$dim" "$reset"
sleep 0.4

# First token: the turn has now visibly started.
printf '  %sbot›%s The quick brown fox' "$green" "$reset"
sleep 0.03
printf ' jumps over'
sleep 0.03
printf ' the lazy dog'

# The trap. 900 ms of silence with a sentence still unfinished.
sleep 0.9

printf ', then'
sleep 0.03
printf ' keeps going'
sleep 0.03
printf ' to the end of the answer.'
printf '\n'

end=$(date +%s%N 2>/dev/null || echo 0)
if [ "$start" != "0" ] && [ "$end" != "0" ]; then
    printf '%s  ✓ done in %s ms%s\n' "$dim" "$(( (end - start) / 1000000 ))" "$reset"
else
    printf '%s  ✓ done%s\n' "$dim" "$reset"
fi
printf '\n'
prompt

# Stay alive and idle, like a real TUI. Ctrl-D exits.
while IFS= read -r _; do
    prompt
done

printf '\033[?2004l\033[?25h\033[?1049l'
exit 0
