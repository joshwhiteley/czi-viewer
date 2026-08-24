# Browse remote CZI files with SSH

Use the in-app browser to open a remote CZI through an existing OpenSSH profile.

1. Start the viewer.
2. Select **SSH**.
3. Keep **Direct SSH** selected. In **Remote files**, enter an SSH profile or host alias.
4. Select **Connect**.
5. If SSH asks for input, click the authentication transcript and type the password, 2FA code, or host-key response.
6. Browse from Home, go Up, Refresh the current directory, or enter an absolute path and select **Go**.
7. Double-click a directory to enter it.
8. Double-click a `.czi` file, or select it and select **Open selected CZI**.

The viewer opens one read-only SFTP session after authentication. Browsing, opening, and reading the selected CZI use that session. They do not prompt again. Change the profile or select **Reconnect** when you need a new session.

For an optional viewer-managed connection through a Cisco AnyConnect-compatible VPN, see [AnyConnect VPN mode](anyconnect-vpn.md). It takes a VPN username, HTTPS gateway, and exact SSH `user@host` route, establishes one local ocproxy endpoint, then reuses the same embedded SSH and persistent SFTP transport described here.

## Authentication console

The console appears only for SSH authentication. It is not a password field.

- Click the transcript before typing.
- Keystrokes go directly to the system `ssh` process.
- The viewer does not retain passwords, one-time codes, or prompt input.
- The console collapses after SFTP VERSION succeeds.
- You can expand it later to inspect its bounded sanitized transcript.
- Select **Cancel authentication** to stop a pending connection.

## Browser limits and safety

The browser resolves Home through SFTP, reads one directory at a time, scans at most 4,096 entries, and shows at most 200 safe directories and `.czi` files. The filename filter is local. Remote paths are SFTP packets only; they are never added to an OpenSSH command line.

If embedded SSH cannot authenticate, correct the profile or SSH configuration and select **Reconnect**. The app does not provide a Terminal fallback workflow.

The embedded transport honors its validated connection configuration. Direct SSH keeps the normal interactive argument vector and `StrictHostKeyChecking=ask` behavior unchanged. AnyConnect VPN mode adds command-line overrides for `HostName=127.0.0.1`, the private local port, `HostKeyAlias=<ssh-host>`, `ProxyCommand=none`, `ProxyJump=none`, and `StrictHostKeyChecking=yes` before the destination. It requires an existing verified host key and never offers a first-use trust prompt. This preserves known-host verification for the real server while preventing a configured proxy from bypassing the local endpoint.
