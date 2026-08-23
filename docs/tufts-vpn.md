# Connect through the Tufts VPN

Tufts VPN mode is an optional macOS-only connection path for the fixed SSH target `login-prod.pax.tufts.edu:22`. It runs a non-root, application-scoped tunnel. It does not change system routes.

## Install the optional tools

Install both tools as one Homebrew pair:

```sh
brew install openconnect ocproxy
```

The viewer checks only these pairs:

- `/opt/homebrew/bin/openconnect` and `/opt/homebrew/bin/ocproxy`
- `/usr/local/bin/openconnect` and `/usr/local/bin/ocproxy`

It never searches `PATH` and never runs `brew`. A live proof of concept used Homebrew OpenConnect 9.21 and ocproxy 1.60.

If your macOS login name is not your SSH username, configure the fixed destination in `~/.ssh/config`:

```sshconfig
Host login-prod.pax.tufts.edu
    User your-ssh-username
```

Normal OpenSSH options for that host still apply. The viewer overrides only the local tunnel endpoint and proxy routing.

## Connect

1. Start the viewer.
2. Select **SSH**.
3. Select **Tufts VPN**.
4. Enter your Tufts VPN username. The viewer keeps it only in memory for the current app process.
5. Select **Connect**.
6. Complete **Phase 1/2 · Tufts VPN authentication** in the in-app terminal. Enter passwords, Duo choices, and one-time responses only when OpenConnect requests them.
7. Wait while the viewer validates an SSH identification banner through the local tunnel.
8. Complete **Phase 2/2 · SSH authentication** when OpenSSH requests input.
9. Browse and open remote CZI files normally.

There is no password, Duo, or one-time-code field. Terminal input goes directly to the active child PTY. The viewer does not retain or parse that input.

## Fixed connection design

The mode uses these fixed network endpoints:

- Gateway: `https://vpn.tufts.edu/duop`
- SSH target: `login-prod.pax.tufts.edu:22`
- Local listener: `127.0.0.1` on an ephemeral port

The viewer starts OpenConnect with `--script-tun`. Its `--script` value is a private, shell-safe helper path created in a mode-`0700` directory. The path contains no user text. The helper validates its invocation before it replaces itself, without a shell, with the matching absolute-path ocproxy executable:

```text
ocproxy -L <ephemeral-port>:login-prod.pax.tufts.edu:22
```

OpenConnect may use its own shell to start the fixed script command. No username, password, remote file path, prompt output, or other user-controlled text enters that command.

After a bounded SSH banner is received while OpenConnect is still live, the viewer starts system OpenSSH with these effective routing overrides:

```text
HostName=127.0.0.1
Port=<ephemeral-port>
HostKeyAlias=login-prod.pax.tufts.edu
ProxyCommand=none
ProxyJump=none
```

`HostKeyAlias` keeps host-key checks bound to `login-prod.pax.tufts.edu`, not the loopback address. The same authenticated SFTP session is reused for browsing, opening, and range reads.

## Cancellation and cleanup

The SSH child and VPN terminal child have separate exclusive roles, so one of each can run concurrently. Each is a private process group. Cancel, reconnect, selecting Local, connection failure, and app shutdown terminate the matching SSH, OpenConnect, shell helper, and ocproxy descendants. Connection generations reject stale readiness and authentication events.

The viewer retains the VPN terminal pump while the tunnel is active so OpenConnect output cannot fill an abandoned PTY. It collapses the completed phase when SSH authentication begins.

## Troubleshooting

### Tools not found

Install both tools with:

```sh
brew install openconnect ocproxy
```

The viewer rejects a partial pair and does not use tools from another directory or from `PATH`.

### VPN authentication fails

Select **Try again** and complete the OpenConnect prompts in the phase-one terminal. The viewer does not inspect prompt text.

### SSH host-key or authentication fails

Check the `login-prod.pax.tufts.edu` entry in `~/.ssh/config` and its known-host record. The SSH destination and host-key alias stay fixed even though the TCP connection uses loopback.

### Readiness times out

The viewer waits up to 10 minutes for a bounded SSH banner at the local endpoint. Cancel and reconnect. OpenConnect and ocproxy must both remain live for readiness to succeed.

## Licensing boundary

OpenConnect is LGPL-2.1-only and ocproxy is BSD-3-Clause. They are optional, separately installed executables. They are not linked, bundled, copied, or distributed with the viewer. The distributed application and its Rust dependency graph remain pure Rust under the project dependency policy.
