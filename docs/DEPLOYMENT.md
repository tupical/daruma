# Deployment

Daruma exposes HTTP on `:8080` and pairing/TLS discovery on `:8443` by default.
Start the server with:

```bash
daruma-server
```

Pair a desktop client from an admin token:

```bash
curl -H "Authorization: Bearer $DARUMA_TOKEN" \
  http://SERVER:8080/v1/devices/pair/ticket
daruma-desktop pair 'daruma://pair?host=SERVER:8443&token=...&fpr=sha256:...'
```

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

Put a TLS reverse proxy in front of the API and keep the server private.

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

Open `443/tcp` publicly. Keep `8080/tcp` and `8443/tcp` bound to localhost or a
private network. For pairing, issue the ticket over the public API and use the
advertised `host`/`tls_fingerprint` values from `/v1/devices/pair/ticket`.

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
