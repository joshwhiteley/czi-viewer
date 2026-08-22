# Browse remote CZI files with SSH

Use the in-app browser to open a remote CZI through an existing OpenSSH profile.

1. Start the viewer.
2. Select **SSH**.
3. In **Remote files**, enter an SSH profile or host alias.
4. Select **Connect**.
5. If SSH asks for input, click the authentication transcript and type the password, 2FA code, or host-key response.
6. Browse from Home, go Up, Refresh the current directory, or enter an absolute path and select **Go**.
7. Double-click a directory to enter it.
8. Double-click a `.czi` file, or select it and select **Open selected CZI**.

The viewer opens one read-only SFTP session after authentication. Browsing, opening, and reading the selected CZI use that session. They do not prompt again. Change the profile or select **Reconnect** when you need a new session.

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

Use **Use Terminal fallback** only if embedded SSH cannot authenticate. The viewer then shows a copyable command. Keep that Terminal session open while the remote CZI is in use.
