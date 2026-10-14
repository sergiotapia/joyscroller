### Installing

- **macOS:** download the `.dmg` for your Mac: `arm64` for Apple Silicon (M1 and later), `x86_64` for Intel. Open it and drag Joyscroller onto Applications. Joyscroller isn't signed by Apple, so the first time you open it macOS says it can't check it: open **System Settings → Privacy & Security**, scroll down and click **Open Anyway**. Joyscroller then opens in your browser; it has no window or Dock icon of its own, and **Turn off** in its taskbar stops it. When it first reads your browser's login, macOS asks whether it may use your browser's key in the Keychain; after an update it may ask again.
- **Windows:** unzip the `.zip` for your PC (`x64`, or `arm64` for Windows on Arm) and run `joyscroller.exe`. It isn't signed, so SmartScreen may warn: click **More info**, then **Run anyway**. Keep its window open while it runs.
- **Linux:** unpack the `.tar.gz` and run `./joyscroller`.

Every file's SHA-256 is in `SHA256SUMS`.
