# @igorjs/ward-sdk (TypeScript)

TypeScript client for the [ward](https://github.com/igorjs/ward) sandbox
daemon. Wraps the gRPC API at [`proto/ward.proto`](../../proto/ward.proto)
with a Promise-based `WardClient` class and full type definitions.

> **Status: pre-release, first-cut scaffold.** Tracked in
> [issue #40](https://github.com/igorjs/ward/issues/40).

## Install (when published)

```sh
npm install @igorjs/ward-sdk
```

For local development:

```sh
cd sdks/typescript
npm install
npm run proto:generate
npm run build
```

## Quick start

```ts
import { WardClient } from "@igorjs/ward-sdk";

const client = WardClient.connect(); // ~/.ward/ward.sock

const sb = await client.createSandbox({
  image: "alpine:latest",
  egress: "deny",
});

const result = await client.run(sb.id, ["echo", "hello from inside"]);
console.log(result.stdout);

await client.removeSandbox(sb.id);
```

For long-running output, async iterate the event stream:

```ts
for await (const event of client.streamOutput(sb.id, result.pid)) {
  if (event.kind === "stdout") console.log(event.line);
  else if (event.kind === "exit") {
    console.log(`finished with code ${event.exitCode}`);
    break;
  }
}
```

## Why TypeScript

Web UIs that embed ward (browser dev tools surface, Electron desktop apps,
internal admin consoles) and Node-based agents (LangChain-JS, MasterAI,
Vercel AI SDK projects) need a TypeScript client. The browser variant
goes through gRPC-Web; the Node variant uses `@grpc/grpc-js` with a
Unix-socket transport.

## Licence

Apache-2.0. Independent of ward's AGPL-3.0 daemon licence per
[ADR-005](../../docs/adr/005-sdk-strategy.md) and
[ADR-006](../../docs/adr/006-licensing.md).
