# BackPack Hub for MikroTik

Tunnel hub for RouterOS 7.20+ on x86_64, with a web panel. Install it the same way on both routers (Iran and abroad).

1. Turn on container mode once, then press the router's button or power-cycle it:
   `/system/device-mode/update container=yes`
2. Install the `container` package (System → Packages) if it is missing.
3. In the router terminal:
   ```
   /tool fetch url="https://raw.githubusercontent.com/TrafXCloud/trafx-tun/main/bp-hub/install.rsc"; /import install.rsc
   ```
   It prints the panel address and a password at the end.

Remove: `/tool fetch url="https://raw.githubusercontent.com/TrafXCloud/trafx-tun/main/bp-hub/uninstall.rsc"; /import uninstall.rsc`

Image contents and licenses: see /NOTICE inside the image. BackPack (AGPL-3.0) is shipped unmodified, source: https://github.com/AminMGMT/BackPack
