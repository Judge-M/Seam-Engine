use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use seam_engine::{
    AuthorityContext, Capability, Gateway, GatewayDecisionEngine, GatewayError, GatewayRequest,
    GatewayScope, InMemoryAuthoritySource, IntentRequest, ReturnAmbiguity, SeamEngine, Subject,
};
use serde_json::{Value, json};

struct FixtureGateway;

#[async_trait]
impl Gateway for FixtureGateway {
    async fn execute(&self, request: GatewayRequest) -> Result<Value, GatewayError> {
        match request.scope {
            GatewayScope::Control => {
                Ok(json!({"form":"choice", "candidate":"development.issue", "confidence":0.95}))
            }
            GatewayScope::Caller => Ok(json!({"issues":["fixture-1"]})),
        }
    }
}

#[tokio::main]
async fn main() {
    let subject = Subject {
        id: "other-agent-1".into(),
        kind: "other-runtime".into(),
    };
    let authority = Arc::new(InMemoryAuthoritySource::default());
    authority
        .grant(
            subject.clone(),
            AuthorityContext {
                grant_id: "task-1".into(),
                allowed_capabilities: BTreeSet::from([
                    "development.issue.search".into(),
                    "development.repository.read".into(),
                ]),
                expires_at_epoch_seconds: None,
            },
        )
        .await;
    let gateway = Arc::new(FixtureGateway);
    let mut engine = SeamEngine::new(
        Arc::new(GatewayDecisionEngine::new(gateway.clone())),
        authority,
        gateway,
        Arc::new(ReturnAmbiguity),
    );
    for name in ["development.issue.search", "development.repository.read"] {
        engine
            .register_capability(Capability {
                name: name.into(),
                description: name.into(),
                argument_schema: json!({"type":"object"}),
            })
            .expect("catalog");
    }
    let result = engine
        .execute(IntentRequest {
            subject,
            grant_id: "task-1".into(),
            intent: "Find the issue".into(),
            arguments: json!({}),
        })
        .await
        .expect("engine");
    println!("{}: {}", result.audit.capability, result.data);
}
