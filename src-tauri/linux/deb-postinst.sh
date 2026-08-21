#!/bin/sh
# Post-install cleanup for the hex-motor-gui -> hexmeow-gui rebrand (234bf30).
#
# `Conflicts`/`Replaces: hex-motor-gui` in tauri.conf.json already makes dpkg
# drop the files of an old *dpkg-installed* hex-motor-gui on upgrade. What it
# cannot touch is a stale hex-motor-gui.desktop that no package owns: one left
# by AppImage desktop integration, or hand-copied. Those show up as a second
# launcher next to hexmeow-gui and still start the app, because
# `mainBinaryName` stayed `hex-motor-gui` across the rebrand, so nothing about
# them looks broken.
#
# This is the Linux counterpart of windows/nsis-hooks.nsh, which deletes the
# old Start Menu and Desktop shortcuts during the NSIS migration.

set -e

LEGACY_NAME=hex-motor-gui

# Debian Policy 6.1: honour DPKG_ROOT so the script also works when dpkg
# populates a target root without chrooting into it. Empty for a normal install.
DPKG_ROOT="${DPKG_ROOT:-}"

# Ask the right package database when operating on a target root.
legacy_is_owned() {
  if [ -n "$DPKG_ROOT" ]; then
    dpkg-query --root="$DPKG_ROOT" -S "$1" >/dev/null 2>&1
  else
    dpkg-query -S "$1" >/dev/null 2>&1
  fi
}

case "$1" in
  configure)
    for dir in /usr/share/applications /usr/local/share/applications; do
      entry="$dir/$LEGACY_NAME.desktop"
      [ -f "$DPKG_ROOT$entry" ] || continue
      # Never delete a file some other package is responsible for.
      if legacy_is_owned "$entry"; then
        continue
      fi
      echo "hexmeow-gui: removing the pre-rebrand launcher $entry"
      rm -f "$DPKG_ROOT$entry"
    done

    # Per-user entries are out of bounds for a maintainer script, so report
    # them instead of deleting them.
    for entry in "$DPKG_ROOT"/root/.local/share/applications/"$LEGACY_NAME"*.desktop \
                 "$DPKG_ROOT"/home/*/.local/share/applications/"$LEGACY_NAME"*.desktop; do
      [ -f "$entry" ] || continue
      echo "hexmeow-gui: a pre-rebrand launcher is still installed for one user:" >&2
      echo "hexmeow-gui:   $entry" >&2
      echo "hexmeow-gui: delete it to get rid of the duplicate app entry." >&2
    done

    # The bundler generates no triggers, so refresh the caches here.
    if command -v update-desktop-database >/dev/null 2>&1; then
      update-desktop-database -q "$DPKG_ROOT/usr/share/applications" || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
      gtk-update-icon-cache -q -f "$DPKG_ROOT/usr/share/icons/hicolor" || true
    fi
    ;;

  abort-upgrade | abort-remove | abort-deconfigure) ;;

  *)
    echo "postinst called with unknown argument \`$1'" >&2
    exit 1
    ;;
esac

exit 0
