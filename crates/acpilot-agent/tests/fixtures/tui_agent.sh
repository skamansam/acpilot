#!/bin/sh
# A stand-in for a TUI program, for the PTY adapter's tests.
#
# It has the one thing the adapter needs from a program and a shell does not
# have by accident: an input row that goes away and comes back. `tui` draws the
# row, takes a line, clears the screen (so the row is gone), draws the answer and
# then draws the row again at rest — which is exactly the sentinel the sentinel
# check looks for.
#
#   ./tui_agent.sh turn    one answer, then the input row comes back
#   ./tui_agent.sh silent  an answer and no row: only the idle backstop can end it
#
# `clear_screen` is the point of the clear: without it the input row never
# leaves the alternate-free screen and a sentinel detector would be right to
# refuse to call the turn over.

MODE="${1:-turn}"

clear_screen() {
    printf '\033[2J\033[H'
}

row() {
    printf '\342\226\215Enter a coding task'
}

clear_screen
row

while IFS= read -r line; do
    case "$line" in
        *"/exit"*)
            clear_screen
            exit 0
            ;;
    esac

    case "$MODE" in
        silent)
            clear_screen
            printf '  \342\227\217 partial answer\r\n'
            # Long enough that the test's idle timeout fires first, short enough
            # that the process a killed shell leaves behind is gone soon after.
            sleep 3
            ;;
        *)
            clear_screen
            # The prompt appears on exactly one row. A fixture that repeated it
            # in the answer would make "the last row repeating the prompt is the
            # user's line" ambiguous, which is a fixture problem, not a parser one.
            printf '  you: %s\r\n' "$line"
            printf '  \342\200\242 Thinking\r\n'
            printf '    Thinking about the request.\r\n'
            printf '  OK: got it\r\n'
            printf '\r\n'
            printf '  \342\234\273 Churned for 1s \302\267 done\r\n'
            row
            ;;
    esac
done
