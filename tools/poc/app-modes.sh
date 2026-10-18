#!/bin/sh
# Pane-side application-mode probe for `poc modes`.
#
# Stands in for a full-screen program: it asks the terminal (tmux) for the input
# modes a real TUI enables, then stays alive so a client can attach, read the modes
# from the per-pane metadata and restore them.  Nothing here is tmux-specific - each
# sequence is the standard one an editor or pager sends.
#
# The scenario knows what to expect: see `crates/poc-cli/src/scenarios.rs::modes`.
set -eu

printf '\033[?1002h'  # DECSET 1002: button-drag mouse reporting
printf '\033[?1006h'  # DECSET 1006: SGR mouse encoding
printf '\033[?1h'     # DECCKM: application cursor keys
printf '\033='        # DECKPAM: application keypad
printf '\033[?25l'    # DECSET 25 off: hide the cursor
printf '\033[4h'      # IRM: insert mode (not a private mode)
printf '\033[?6h'     # DECSET 6: origin mode
printf '\033[?7l'     # DECSET 7 off: no automatic line wrapping
printf '\033[?2004h'  # DECSET 2004: bracketed paste (tmux reports it from 3.7)
printf 'modes probe ready'

# Keep the pane alive with the modes in place.  No prompt, no further output: the
# scenario reads the modes, not the screen.
exec sleep 600