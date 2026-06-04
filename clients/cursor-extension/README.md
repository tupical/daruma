# TaskAgent Cursor/VSCode Extension

Native sidebar extension for TaskAgent. This package is separate from
`clients/cursor-plugin`, which remains the lightweight MCP/rules glue.

## Development

```bash
npm install
npm test
```

The extension reads `taskagent.apiUrl` / `taskagent.token` settings, falling
back to `TASKAGENT_API_URL` / `TASKAGENT_TOKEN`.

On WSL with Windows `npm`, run compile directly if `npm test` falls back to
`C:\Windows` because of UNC paths:

```bash
node.exe node_modules/typescript/bin/tsc -p .
```

Packaging is intentionally left to the VSCE toolchain once the native extension
is ready for a manual Cursor smoke test.
