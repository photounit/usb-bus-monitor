#!/bin/sh
# One-time setup so USB Bus Monitor can see live USB traffic without sudo.
# Loads the kernel's usbmon module now and at every boot, and lets the
# logged-in desktop user (and the plugdev group) read /dev/usbmon*.
# Note: usbmon shows all USB traffic, including keyboards, to those users.
set -e
[ "$(id -u)" = 0 ] || exec sudo sh "$0" "$@"

echo usbmon > /etc/modules-load.d/usbmon.conf
echo 'SUBSYSTEM=="usbmon", GROUP="plugdev", MODE="0640", TAG+="uaccess"' > /etc/udev/rules.d/70-usbmon.rules
udevadm control --reload
modprobe usbmon
udevadm trigger --subsystem-match=usbmon --action=add
udevadm settle
echo "usbmon is set up. USB Bus Monitor will pick it up within a few seconds."
