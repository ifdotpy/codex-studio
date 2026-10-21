#!/bin/sh
# Studio's `open` for agent commands on a macOS host (docs/vm-browser-bridge.md, section 6.4).
# One http or https URL goes to the person's browser through the Studio launcher next to this
# folder. Every other use runs the system `open` with the same arguments.
if [ "$#" -eq 1 ]; then
  case "$1" in
    [Hh][Tt][Tt][Pp]://* | [Hh][Tt][Tt][Pp][Ss]://*)
      case "$0" in
        */*) folder=${0%/*} ;;
        *) folder=. ;;
      esac
      exec "$folder/../studio-browser" "$1"
      ;;
  esac
fi
exec /usr/bin/open "$@"
