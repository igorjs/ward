# ward-client (Rust)

Rust library client for the [ward](https://github.com/igorjs/ward) sandbox
daemon. A library extraction of the gRPC client code already used by
`ward-cli`, packaged for embedding into other Rust applications.

> **Status: pre-release, first-cut scaffold.** Tracked in
> [issue #42](https://github.com/igorjs/ward/issues/42).

## Install (when published)

```sh
cargo add ward-client
```

## Quick start

```rust
use ward_client::{WardClient, CreateOptions, EgressMode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = WardClient::connect_default().await?;

    let sb = client.create_sandbox(CreateOptions {
        image: "alpine:latest".into(),
        egress: EgressMode::Deny,
        ..Default::default()
    }).await?;

    let result = client.run(&sb.id, &["echo", "hello"]).await?;
    println!("{}", result.stdout);

    client.remove_sandbox(&sb.id).await?;
    Ok(())
}
```

## Why a separate crate

`ward-cli` already speaks gRPC to the daemon. Lifting that code into
`ward-client` lets other Rust applications (custom CI runners, ward-aware
test harnesses, embedded ward-in-a-larger-binary scenarios) reuse the
exact same client without rebuilding the CLI or scraping its source. The
CLI then depends on this crate, keeping a single source of truth.

For non-Rust clients see the
[Python](../../python/),
[TypeScript](../../typescript/), and
[Go](../../go/) SDKs.

## Licence

Apache-2.0. Independent of ward's AGPL-3.0 daemon licence so embedders
don't inherit copyleft on their own code (per
[ADR-005](../../../docs/adr/005-sdk-strategy.md) and
[ADR-006](../../../docs/adr/006-licensing.md)).
