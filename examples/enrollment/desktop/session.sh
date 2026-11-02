#!/bin/sh
# Starts the XFCE session once Xvnc is ready. The desktop app starts from
# /etc/xdg/autostart and opens sign-in pages in Firefox.
set -eu

while [ ! -S /tmp/.X11-unix/X1 ]; do
  sleep 0.2
done

export BROWSER=firefox-esr
# WebKitGTK's bubblewrap sandbox cannot start inside an unprivileged
# container, and Xvnc has no GPU.
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1

exec dbus-launch --exit-with-session startxfce4
