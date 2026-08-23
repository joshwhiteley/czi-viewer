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

Tufts VPN and SSH identities are separate. In the viewer, enter the VPN username used by OpenConnect and an SSH profile used by OpenSSH. For example:

```text
VPN username: jdoe
SSH profile:  jwhite22@login-prod.pax.tufts.edu
```

Do not assume the VPN username is also the SSH username. The SSH profile can instead be an existing alias whose `~/.ssh/config` entry sets `User`:

```sshconfig
Host tufts-login
    HostName login-prod.pax.tufts.edu
    User jwhite22
```

The viewer still overrides `HostName` to loopback for the connection, so the network route and host-key alias remain fixed to `login-prod.pax.tufts.edu`. Other normal OpenSSH options for the selected profile still apply.

## Verify the SSH host key before first use

Tufts VPN mode requires an existing verified `known_hosts` entry for `login-prod.pax.tufts.edu`. It sets `StrictHostKeyChecking=yes`, so a missing or changed key fails without a trust prompt and before SSH accepts a password.

1. Obtain the current SSH host-key fingerprint from Tufts IT or the server administrator through a trusted, independent channel.
2. From a trusted Tufts network path or an independently trusted VPN client, run:

   ```sh
   /usr/bin/ssh login-prod.pax.tufts.edu
   ```

3. Compare the complete fingerprint shown by OpenSSH with the independently obtained fingerprint.
4. Accept it only when the fingerprints match exactly. Do not enter an SSH password before completing this check.
5. Exit the connection. The verified entry is now available to the viewer.

You can check that OpenSSH finds an entry without contacting the server:

```sh
/usr/bin/ssh-keygen -F login-prod.pax.tufts.edu
```

Never use automatic `ssh-keyscan` output as proof of identity. A key received over the same untrusted network path is not independently verified.

## Connect

1. Start the viewer.
2. Select **SSH**.
3. Confirm that the verified host-key prerequisite above is complete, then select **Tufts VPN**.
4. Enter your Tufts VPN username. The viewer keeps it only in memory for the current app process.
5. Enter the SSH profile or `user@host` destination that supplies the SSH username. For the reviewed connection, use `jwhite22@login-prod.pax.tufts.edu`.
6. Select **Connect**.
7. Complete **Phase 1/2 · Tufts VPN authentication** in the in-app terminal. Enter passwords, Duo choices, and one-time responses only when OpenConnect requests them. If prompted for a second-factor method, type the requested choice, such as `PUSH`. The viewer does not select push automatically.
8. Wait while the viewer validates an SSH identification banner through the local tunnel.
9. Complete **Phase 2/2 · SSH authentication** when OpenSSH requests input.
10. Browse and open remote CZI files normally.

There is no password, Duo, or one-time-code field. Terminal input goes directly to the active child PTY. The viewer does not retain or parse that input.

## Fixed connection design

The mode uses these fixed network endpoints:

- Gateway: `https://vpn.tufts.edu/duo`
- SSH target: `login-prod.pax.tufts.edu:22`
- Local listener: `127.0.0.1` on an ephemeral port

The viewer starts OpenConnect with `--script-tun`. DTLS remains enabled for throughput; OpenConnect performs its normal fallback to TLS when DTLS is unavailable. Its `--script` value is a private, shell-safe helper path created in a mode-`0700` directory. The path contains no user text. The helper validates its invocation before it replaces itself, without a shell, with the matching absolute-path ocproxy executable:

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
StrictHostKeyChecking=yes
```

`HostKeyAlias` keeps host-key checks bound to `login-prod.pax.tufts.edu`, not the loopback address. Strict checking requires that key to exist already and match exactly; the viewer never permits TOFU for a loopback endpoint. Therefore, another local process can occupy the ephemeral port or supply an SSH banner only to deny the connection. It cannot reach a password prompt with an unverified host key. The same authenticated SFTP session is reused for browsing, opening, and range reads.

## Cancellation and cleanup

The SSH child and VPN terminal child have separate exclusive roles, so one of each can run concurrently. Each is a private process group. Cancel, reconnect, selecting Local, connection failure, and app shutdown terminate the matching SSH, OpenConnect, shell helper, and ocproxy descendants. Connection generations reject stale readiness and authentication events.

The viewer retains the VPN terminal pump while the tunnel is active so OpenConnect output cannot fill an abandoned PTY. After VPN readiness it clears the VPN authentication transcript and continues draining later output. It then collapses the completed phase when SSH authentication begins.

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

If the verified `login-prod.pax.tufts.edu` key is absent, OpenSSH fails closed without prompting. Follow **Verify the SSH host key before first use** above. If a stored key changed, stop and verify the new fingerprint independently with Tufts IT or the server administrator. Never bypass the failure with `StrictHostKeyChecking=no`, `accept-new`, or automatic `ssh-keyscan` output.

Also check the `login-prod.pax.tufts.edu` entry in `~/.ssh/config`. The SSH destination and host-key alias stay fixed even though the TCP connection uses loopback.

### Readiness times out

The viewer waits up to 10 minutes for a bounded SSH banner at the local endpoint. Cancel and reconnect. OpenConnect and ocproxy must both remain live for readiness to succeed.

## Licensing boundary

OpenConnect is LGPL-2.1-only and ocproxy is BSD-3-Clause. They are optional, separately installed executables. They are not linked, bundled, copied, or distributed with the viewer. The distributed application and its Rust dependency graph remain pure Rust under the project dependency policy.
