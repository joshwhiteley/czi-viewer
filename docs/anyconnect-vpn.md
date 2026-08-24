# Connect through an AnyConnect VPN

AnyConnect VPN mode is an optional macOS-only connection path for SSH/SFTP servers reachable through a Cisco AnyConnect-compatible VPN. It creates a non-root, application-scoped tunnel and does not change macOS routes or DNS.

## Install the optional tools

Install OpenConnect and ocproxy separately:

```sh
brew install openconnect ocproxy
```

CZI Viewer accepts one complete Homebrew tool pair at these fixed paths:

- Apple Silicon: `/opt/homebrew/bin/openconnect` and `/opt/homebrew/bin/ocproxy`
- Intel Homebrew: `/usr/local/bin/openconnect` and `/usr/local/bin/ocproxy`

The app does not bundle these tools, search `PATH`, or run Homebrew.

## Connection fields

Select **SSH → AnyConnect VPN** and enter:

```text
VPN Username: <your VPN username>
VPN Gateway:  https://vpn.university.edu/group
SSH Profile:  researcher@login.university.edu
```

The gateway must be an HTTPS URL without embedded credentials or a fragment. The SSH profile must use the exact `user@host` form. SSH aliases and custom ports are not accepted in this mode because the host is also used for ocproxy forwarding and host-key verification. SSH uses port 22.

VPN and SSH identities are independent. The app keeps these values only in memory for the current process.

## Connect

1. Select **SSH**, then **AnyConnect VPN**.
2. Enter the VPN username, HTTPS gateway, and SSH `user@host` route.
3. Select **Connect**.
4. Complete **Phase 1/2 · AnyConnect VPN authentication** in the in-app terminal.
5. Wait while the viewer checks for an SSH identification banner through the local tunnel.
6. Complete **Phase 2/2 · SSH authentication**.
7. Browse and open remote CZI files through the persistent read-only SFTP session.

Authentication input goes directly to the active PTY. The viewer does not parse, store, log, or echo passwords, one-time codes, or interactive responses.

## Host-key verification

AnyConnect mode sets `StrictHostKeyChecking=yes` and uses the host from the SSH route as `HostKeyAlias`. Before connecting, verify and store that server's SSH host key through a trusted independent channel. A missing or changed key fails closed without a first-use trust prompt.

For example, after independently confirming the fingerprint:

```sh
/usr/bin/ssh researcher@login.university.edu
/usr/bin/ssh-keygen -F login.university.edu
```

Do not bypass a host-key failure with `StrictHostKeyChecking=no`, `accept-new`, or unverified `ssh-keyscan` output.

## Network and process boundaries

For a validated gateway and SSH route, the viewer starts OpenConnect with a fixed argument shape equivalent to:

```text
openconnect --user <vpn-user> --script-tun --script <private-helper> \
  --protocol=anyconnect <https-gateway>
```

The private helper starts ocproxy with one loopback forwarding rule:

```text
ocproxy -L <ephemeral-port>:<ssh-host>:22
```

OpenSSH then receives fixed overrides:

```text
HostName=127.0.0.1
Port=<ephemeral-port>
HostKeyAlias=<ssh-host>
ProxyCommand=none
ProxyJump=none
StrictHostKeyChecking=yes
```

Only validated username, gateway, and route values vary. Tool paths, flags, helper invocation, local routing, SSH port, and strict host-key behavior remain fixed. Values are passed as individual process arguments or validated environment entries, never through user-controlled shell interpolation.

## Cancellation and cleanup

The viewer owns one OpenConnect process group, one ocproxy child, and one SSH/SFTP child. **Cancel**, **Reconnect**, changing connection fields, switching to a local file, or exiting terminates the relevant process groups and removes the private helper.

The VPN transcript is cleared after the tunnel becomes ready. The later SSH transcript is bounded in memory.

## Troubleshooting

### Tools not found

Install both tools in the same supported Homebrew prefix:

```sh
brew install openconnect ocproxy
```

### Gateway rejected

Use the complete HTTPS login URL supplied by your institution. Do not include a username, password, or URL fragment.

### SSH route rejected

Use `user@host`, for example `researcher@login.university.edu`. AnyConnect mode does not accept an SSH alias, explicit port, IPv6 literal, or more than one `@`.

### SSH host-key or authentication fails

Confirm that `known_hosts` contains the independently verified key for the exact route host. Stop and verify unexpected key changes with the server administrator.

### Readiness times out

Confirm that the VPN permits the SSH host on port 22 and that the gateway/session completed successfully. Cancellation and timeout terminate the tunnel instead of leaving a background process running.
