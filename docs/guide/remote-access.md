# Remote access

The daemon and the TUI speak the same wire protocol over either a Unix socket or
a token-authenticated WebSocket, so "remote" is just a transport choice. This
page covers attaching from another machine.

## Token attach

On first start the daemon writes a bearer token to `<data_dir>/token`
(`~/.local/share/favetto/token` by default, mode `0600`). Copy it to the client
machine and point the TUI at the daemon's WebSocket:

```bash
favetto tui --remote ws://HOST:7878/rpc --token-file ~/.local/share/favetto/token
```

If the daemon listens somewhere else, set `--listen` on the daemon and use the
same address here. `FAVETTO_URL` provides a default remote URL so you can run
`favetto tui` with no flags:

```bash
export FAVETTO_URL=ws://HOST:7878/rpc
favetto tui
```

## TLS (`wss://`)

The client attaches over TLS when the URL uses the `wss://` scheme:

```bash
favetto tui --remote wss://HOST:7878/rpc --token-file ~/.local/share/favetto/token
```

The TLS handshake is validated against the bundled Mozilla root store; there is
no flag to accept an invalid certificate. `wss://` is what makes remote attach
safe over an untrusted network, and the connect timeout still bounds a stalled
handshake.

There are two supported deployments:

- **Plaintext behind a TLS-terminating reverse proxy (recommended).** The daemon
  keeps serving `ws://` (loopback by default) and the proxy (nginx, Caddy,
  Traefik, …) terminates TLS and forwards the WebSocket upgrade. Clients use
  `wss://PROXY_HOST`. No daemon change is required; make sure the proxy forwards
  the `Authorization` header and the `/rpc` upgrade path.
- **Direct `wss://` endpoint.** If something else already terminates TLS in
  front of the daemon (or you front it yourself), point the client straight at
  the `wss://` endpoint. Certificate validation applies to whatever host the URL
  names.

`wss://` also works with pairing: `exchange_pair_code` rewrites the WebSocket
scheme to `https://` and strips the `/rpc` path for the one-off
`POST /pair/exchange` call, so
`favetto tui --remote wss://HOST:7878/rpc --pair-code 123456` talks to
`https://HOST:7878` and then attaches over the validated WebSocket.

## Pairing

Copying the token is fine for a trusted machine. For a one-off session, ask the
daemon for a short-lived code and exchange it for the token during attach:

```bash
# on any machine that can reach the daemon's HTTP endpoint
favetto pair --url http://HOST:7878
# pairing code (valid 60s): 123456
# attach with: favetto tui --remote ws://HOST:7878/rpc --pair-code 123456
```

```bash
favetto tui --remote ws://HOST:7878/rpc --pair-code 123456
```

The code is valid for 60 seconds and is consumed on first use. Under the hood
`favetto pair` calls `POST /pair/generate`; the client calls `POST /pair/exchange`
with the code to obtain the token.

## Rotate the token

If the token leaks, rotate it on the daemon host:

```bash
favetto token-rotate
# new token written to /home/you/.local/share/favetto/token
```

Existing clients keep working until their next connection; use `--data-dir` to
rotate a token outside the default location.

## Security

- The WebSocket API rejects requests without a valid bearer token
  (`-32001 Unauthorized`).
- A bearer token sent over `ws://` is plaintext on the wire; use `wss://` (or a
  TLS-terminating proxy) whenever the network is not fully trusted.
- The Unix socket is local and trusted; anyone who can open it can control the
  daemon.
- The pairing code is short-lived and single-use. Do not expose the HTTP
  endpoint to untrusted networks.

See the [CLI reference](../reference/cli) for every flag, and the
[remote API reference](../reference/remote-api) for the protocol.
