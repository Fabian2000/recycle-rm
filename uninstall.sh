#!/usr/bin/env bash
# Remove recycle-rm again: deletes the installed `rm` and the PATH lines that
# install.sh added. Pass the same scope flag you installed with, e.g.
#   ./uninstall.sh            # per-user (~/.local/bin)
#   ./uninstall.sh --system   # system-wide (/usr/local/bin, needs root)
#   PREFIX=/opt/x ./uninstall.sh
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/install.sh" --uninstall "$@"
