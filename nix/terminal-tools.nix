# A minimal-but-usable interactive terminal userland — the shared /bin set for
# the bash and foot sandboxes. Deliberately small: a shell, coreutils, a pager,
# the everyday text tools, `tree`, and ncurses — the last both for
# `clear`/`tput`/`reset` and, crucially, the **terminfo database** so `TERM`
# resolves (it carries `xterm-256color` and `foot` alike). No editor, no
# network tools — add per-sandbox if a workload needs them.
{ pkgs }:

with pkgs; [
  bash        # interactive shell (readline; SYS_BASHRC = /etc/bashrc)
  coreutils   # ls, cat, cp, mv, rm, mkdir, …
  less        # pager
  gnugrep     # grep / egrep / fgrep
  gnused      # sed
  findutils   # find, xargs
  which       # which
  tree        # directory tree
  ncurses     # clear / tput / reset / infocmp + the terminfo DB
]
