# Seam Engine

Seam Engine is a semantic decision plane for agents that sits in front of existing unified AI/tool gateway infrastructure.

```text
any agent runtime ──> Seam Engine ──> existing gateway
Seam SDK          ──> Seam Engine ──> existing gateway
```

Seam SDK is optional. Engine accepts generic `Subject`, grant, intent, and arguments values; it has no ticket, worker loop, sandbox, or SDK dependency.

Engine resolves a caller's grant from a trusted `AuthoritySource`, filters the catalog to authorized semantic capabilities, and asks a `DecisionEngine` to choose only from those candidates. It can narrow a large catalog across any number of namespace layers. Deterministic code checks candidate membership, confidence threshold, expiration, and argument schema before sending the semantic capability to `Gateway`. Low-confidence choices go to a replaceable `EscalationPolicy`; the default reports ambiguity.

```text
caller intent
  → trusted authority lookup
  → authorized candidate set
  → layered System 1 choice
  → deterministic validation
  → gateway adapter
  → existing gateway chooses concrete execution
```

Models and tools are logical capabilities to Engine. It chooses a sanctioned semantic route such as `inference.code.reason` or `development.issue.search`. The existing gateway chooses the actual model, tool, provider, MCP server, credentials, retries, and load balancing. Engine does not host models or provide a gateway, provider registry, MCP registry, or generic workflow engine.

## System 1 connectivity

`DecisionEngine` supports choice, boolean, and score requests. The included `GatewayDecisionEngine` sends them through the same replaceable gateway port as `seam.decision.choice`, `seam.decision.boolean`, or `seam.decision.score`. These are Engine-internal control requests with `GatewayScope::Control` and no caller subject. The gateway must configure these routes directly, without routing them back through Engine. Any compatible decision implementation can sit behind that gateway. Engine does not load models or manage their infrastructure.

Caller grants cover only caller capabilities. The catalog refuses `seam.decision.*` entries, so a caller cannot gain the control-plane route through semantic routing.

## Run the standalone fixture

```sh
cargo test --all-targets
cargo run --example standalone
```

The example uses a generic non-Seam caller, trusted in-memory grant source, a fixture gateway, and the gateway-backed decision adapter. No SDK is needed. Production deployments should replace the in-memory grant source, fixture gateway, and default escalation behavior as appropriate.

The [combined example](https://github.com/Judge-M/Seam/tree/main/examples/combined) shows the optional SDK adapter.

Dual licensed under MIT or Apache-2.0.
