# Why ward

| Concern          | Docker             | E2B / Daytona (SaaS) | ward                                 |
| ---------------- | ------------------ | -------------------- | ------------------------------------ |
| Kernel isolation | shared host kernel | yes (cloud microVMs) | yes (local microVMs)                 |
| Local-first      | yes                | no, cloud dependency | yes                                  |
| Egress controls  | weak by default    | yes                  | deny default + per-sandbox allowlist |
| Resource caps    | yes (cgroups)      | yes                  | per-VM CPU + memory + PID + timeout  |
| Vendor lock-in   | none               | yes                  | none, AGPL daemon, open SDKs         |

Docker is great at long-running services; it's wasteful and weakly
isolated for ephemeral jobs. SaaS sandboxes have strong isolation but
require sending workloads to someone else's infrastructure. ward fills
the gap: strong local isolation, simple developer UX, no cloud account.
