## CZI Viewer for Apple Silicon

Download the `.dmg`, open it, and drag **CZI Viewer** to **Applications**.

This preview is ad-hoc signed but not Developer ID signed or notarized. The detached Ed25519 update-manifest signature does not change Gatekeeper behavior. On first launch, Control-click **CZI Viewer** in Finder and select **Open**. If macOS still blocks it, use **System Settings → Privacy & Security → Open Anyway**.

The first updater-capable version is a manual bootstrap: older versions do not contain the trusted update public key and cannot securely install it automatically.

The core local viewer and BaSiCPy fitting helper are bundled. AnyConnect VPN and remote-access tools remain separately installed options.

The release includes the DMG, ZIP, Rust and Python CycloneDX SBOMs, third-party notices, `SHA256SUMS`, the canonical `…-update.json`, and its raw 64-byte `…-update.json.sig`. GitHub build-provenance attestations cover the CI-built archives, checksums, SBOMs, and notices.
