#!/bin/sh
# Rich pane content for the terminal-core gate (poc core-gate).
#
# It deliberately exercises the parts of a terminal a naive "restore the text"
# implementation gets wrong:
#
#   * styled runs (bold, colour, underline on a coloured background) and a reset;
#   * CJK double-width characters;
#   * a line longer than the pane, so wrapping is involved;
#   * enough output to push real content into the scrollback;
#   * a final marker the scenario waits for, so the comparison is not racing the
#     program.
#
# Everything is emitted last for the visible screen; the numbered lines before it
# are what lands in history.

i=0
while [ "$i" -lt 70 ]; do
    printf 'history line %s\n' "$i"
    i=$((i + 1))
done

printf '\033[1;31mBOLD-RED\033[0m plain \033[4;44mUNDERLINE-ON-BLUE\033[0m\n'
printf '\344\270\255\346\226\207\345\256\275\345\255\227\347\254\246 ABC \344\270\255\346\226\207\n'
printf 'wrapped: '
i=0
while [ "$i" -lt 60 ]; do
    printf 'w'
    i=$((i + 1))
done
printf '\n'
printf 'CORE-GATE-READY\n'