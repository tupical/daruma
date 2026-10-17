# Daruma Cursor/VSCode Extension

Native sidebar extension for Daruma. This package is separate from
`clients/cursor-plugin`, which remains the lightweight MCP/rules glue.

## Development

```bash
npm install
npm test
```

The extension reads the `daruma.apiUrl` / `daruma.token` settings when you
set them explicitly (user, workspace or folder scope); otherwise it uses the
`DARUMA_API_URL` / `DARUMA_TOKEN` environment variables, and only then the
built-in default `http://localhost:8080` with no token.

On WSL with Windows `npm`, run compile directly if `npm test` falls back to
`C:\Windows` because of UNC paths:

```bash
node.exe node_modules/typescript/bin/tsc -p .
```

Packaging is intentionally left to the VSCE toolchain once the native extension
is ready for a manual Cursor smoke test.
