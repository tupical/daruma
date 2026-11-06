# Deployment

Daruma exposes HTTP on `:8080` and pairing/TLS discovery on `:8443` by default.
Start the server with:

```bash
daruma-server
```

Pair a desktop client from an admin token:

```bash
curl -s -H "Authorization: Bearer $DARUMA_TOKEN" \
  http://SERVER:8080/v1/devices/pair/ticket | jq -r .pairing_url
daruma-desktop pair 'daruma://pair?host=SERVER:8443&token=...&fpr=sha256:...'
```

The ticket returns `host` (`DARUMA_HOSTNAME:DARUMA_TLS_PORT`) and
`tls_fingerprint` (`sha256:<hex>`, the same value as `fpr` in `pairing_url`).
`daruma-desktop pair` connects to `host` over TLS, pins the server's
self-signed certificate to that fingerprint (the handshake aborts on a
mismatch), and sends `POST /v1/devices/pair` with `token` and the observed
`tls_fingerprint`; the server rejects a missing or different fingerprint.
The pairing connection must therefore reach Daruma's own TLS listener: a proxy
that terminates TLS with another certificate breaks pairing.

`paired.json` in `DARUMA_DATA_DIR` keeps the verified fingerprint with the
server URL (`https://HOST:PORT` from the ticket), and `sync` and `devices` use it
for that scheme, host and port from then on, so no CA setup is needed for the
self-signed certificate.

- A certificate that differs from the pin ends the handshake before the bearer
  token is sent; the error shows the expected and the received fingerprint.
- Replacing the certificate (for example, removing `<data_dir>/tls`) needs a new
  ticket and `daruma-desktop pair` on each device, after checking the new
  fingerprint as shown under "Public". Nothing re-trusts a server on its
  own, and that applies to a certificate from a public CA served on the pairing
  port too.
- `DARUMA_API_URL` with another scheme, host or port does not carry the pin: that
  URL gets the system's trust store. The paired token is still used unless
  `DARUMA_TOKEN` is set, so point the override only at the same server.
- A `paired.json` from before this keeps the system's trust store and has no
  pin. If the system does not trust the server, `sync` says so; run `pair` with a
  new ticket to add the pin.

A replica belongs to one server, named by the identity the server reports for
its event log. That identity lives in the server's database, so a new address or
certificate keeps it, and restoring a backup onto another machine keeps it too; a
fresh database gets a new one, and devices treat that as another server.

- Pairing with another server selects that server's own replica under
  `DARUMA_DATA_DIR`: the first server keeps `replica.sqlite`, others use
  `servers/<server_id>/replica.sqlite`. `replicas.json` records which is active.
  The previous replica stays as it is, with its queue; `daruma-desktop replica`
  lists them all, and pairing with that server again brings it back.
- `sync` reads `GET /v1/events/origin` first and stops before sending or fetching
  anything if the server is not this replica's, or does not answer. That includes
  `DARUMA_API_URL` pointing elsewhere, so use another `DARUMA_DATA_DIR` for another
  server without pairing. It also means upgrading the server before the devices.
- A replica that held data before bindings records no server, so `sync` stops on
  it whichever server it reaches, even if it never synced and only has a queue. If
  everything in it is this server's, run `daruma-desktop replica adopt`. If it
  belongs to another server, pair with that server and adopt it there, or keep it
  and use another `DARUMA_DATA_DIR`. That word is not checked: adopting another
  server's replica sends it its queue. A replica that started empty on this version
  needs no adopt: what is written on it offline goes with the first server.
- A `replicas.json` that cannot be read stops every command rather than opening a
  replica by guess. Delete it only if the data directory holds one replica.

## LAN Only

Use this when all devices are on the same trusted network.

```bash
DARUMA_HOSTNAME=my-host.local DARUMA_TLS_PORT=8443 daruma-server
```

Open firewall ports `8080/tcp` and `8443/tcp` on the LAN only. Leave mDNS on
unless your network blocks it; clients can run `daruma-desktop discover`.

## VPN

Bind the server normally, but expose ports only on the VPN interface or firewall
group.

```bash
DARUMA_HOSTNAME=daruma.vpn.example DARUMA_TLS_PORT=8443 daruma-server
```

Pair with the VPN hostname. Keep `8080/tcp` and `8443/tcp` closed on the public
interface; only VPN peers should reach them.

## Public

Put a TLS reverse proxy in front of the HTTP API, and publish the pairing TLS
port directly. The proxy's certificate differs from the self-signed one in the
ticket, so pairing cannot go through `443`.

```nginx
server {
  listen 443 ssl;
  server_name daruma.example.com;

  location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
  }
}
```

```bash
DARUMA_HOSTNAME=daruma.example.com DARUMA_TLS_PORT=8443 daruma-server
```

Open `443/tcp` and `8443/tcp` publicly; keep `8080/tcp` closed by the firewall
(both listeners bind `0.0.0.0`). Daruma terminates TLS on `8443` itself, and
the ticket sends clients to `daruma.example.com:8443`. The TLS listener serves
the same API as `8080` with the same authentication; `POST /v1/devices/pair`
is the only unauthenticated write, single-use and rate limited. If `8443` cannot be
public, open it only while pairing, or pair over LAN/VPN. Forwarding is fine
only as plain TCP passthrough (no TLS termination) on the same public port,
because the ticket advertises `DARUMA_TLS_PORT`.

Issue the ticket over the public API and check that the certificate served on
the pairing port is the one in the ticket before pairing:

```bash
curl -s -H "Authorization: Bearer $DARUMA_TOKEN" \
  https://daruma.example.com/v1/devices/pair/ticket | jq -r .tls_fingerprint
openssl s_client -connect daruma.example.com:8443 </dev/null 2>/dev/null \
  | openssl x509 -outform DER | sha256sum
```

The `sha256sum` digest must equal the ticket's `tls_fingerprint` without the
`sha256:` prefix.

## Manual telemetry retention

Domain rows in a tenant SQLite `events` table are the event-sourced source of
truth and must not be deleted. Operational metrics share that table but carry
`event_class = 'telemetry'`; rotate them manually by age in one transaction:

```sql
BEGIN IMMEDIATE;
DELETE FROM events
WHERE event_class = 'telemetry'
  AND occurred_at < '2026-08-01T00:00:00+00:00';
COMMIT;
```

Back up the tenant database and replace the example UTC cutoff first. Sequence
gaps are intentional (`seq > cursor`), deleted numbers are never reused, and
the cabinet continues reading retained telemetry.
