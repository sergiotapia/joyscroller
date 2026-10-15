### Installing

- **macOS:** download the `.dmg` for your Mac: `arm64` for Apple Silicon (M1 and later), `x86_64` for Intel. Open it and drag Joyscroller onto Applications. Joyscroller isn't signed by Apple, so the first time you open it macOS says *"Joyscroller" Not Opened*: click **Done**, not **Move to Trash** (the highlighted button, which deletes it). Then open **System Settings → Privacy & Security**, scroll to the bottom and click **Open Anyway**. Joyscroller then opens in your browser; it has no window or Dock icon of its own, and **Turn off** in its taskbar stops it. When it first reads your browser's login, macOS asks for your Mac's password so it can use your browser's "Safe Storage" key in the Keychain: type it and click **Always Allow**. After each update it asks once more. If you use Safari, Joyscroller can only read its logins once you turn on **Joyscroller** in **System Settings → Privacy & Security → Full Disk Access** (click **+** if it isn't listed) and open it again. After each update, remove it from that list with **−** and add it again.
- **Linux:** unpack the `.tar.gz` and run `./joyscroller`.

Every file's SHA-256 is in `SHA256SUMS`.
