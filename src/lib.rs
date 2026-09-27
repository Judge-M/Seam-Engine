//! Semantic routing and deterministic authority checks in front of an existing gateway.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::RwLock;

pub mod schema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorityContext {
    pub grant_id: String,
    pub allowed_capabilities: BTreeSet<String>,
    pub expires_at_epoch_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentRequest {
    pub subject: Subject,
    pub grant_id: String,
    pub intent: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    pub description: String,
    pub argument_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionForm {
    Choice,
    Boolean,
    Score,
}

impl DecisionForm {
    pub const fn control_capability(self) -> &'static str {
        match self {
            Self::Choice => "seam.decision.choice",
            Self::Boolean => "seam.decision.boolean",
            Self::Score => "seam.decision.score",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub form: DecisionForm,
    pub question: String,
    pub candidates: Vec<String>,
    #[serde(default)]
    pub context: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum DecisionResult {
    Choice { candidate: String, confidence: f64 },
    Boolean { value: bool, confidence: f64 },
    Score { value: f64 },
}

#[derive(Debug, Error)]
pub enum DecisionError {
    #[error("decision infrastructure unavailable")]
    Unavailable,
    #[error("invalid decision response")]
    InvalidResponse,
}

#[async_trait]
pub trait DecisionEngine: Send + Sync {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResult, DecisionError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayScope {
    Control,
    Caller,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRequest {
    pub scope: GatewayScope,
    pub capability: String,
    pub intent: String,
    pub arguments: Value,
    pub subject: Option<Subject>,
}

#[derive(Debug, Error)]
#[error("gateway operation failed")]
pub struct GatewayError;

#[async_trait]
pub trait Gateway: Send + Sync {
    async fn execute(&self, request: GatewayRequest) -> Result<Value, GatewayError>;
}

/// Uses a preconfigured gateway control route. It never calls `SeamEngine::execute`.
pub struct GatewayDecisionEngine<G> {
    gateway: Arc<G>,
}

impl<G> GatewayDecisionEngine<G> {
    pub fn new(gateway: Arc<G>) -> Self {
        Self { gateway }
    }
}

#[async_trait]
impl<G: Gateway + 'static> DecisionEngine for GatewayDecisionEngine<G> {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResult, DecisionError> {
        let capability = request.form.control_capability().to_owned();
        let data = self
            .gateway
            .execute(GatewayRequest {
                scope: GatewayScope::Control,
                capability,
                intent: request.question.clone(),
                arguments: serde_json::to_value(&request)
                    .map_err(|_| DecisionError::InvalidResponse)?,
                subject: None,
            })
            .await
            .map_err(|_| DecisionError::Unavailable)?;
        let result: DecisionResult =
            serde_json::from_value(data).map_err(|_| DecisionError::InvalidResponse)?;
        let valid = match (&request.form, &result) {
            (
                DecisionForm::Choice,
                DecisionResult::Choice {
                    candidate,
                    confidence,
                },
            ) => request.candidates.contains(candidate) && valid_score(*confidence),
            (DecisionForm::Boolean, DecisionResult::Boolean { confidence, .. }) => {
                valid_score(*confidence)
            }
            (DecisionForm::Score, DecisionResult::Score { value }) => valid_score(*value),
            _ => false,
        };
        if valid {
            Ok(result)
        } else {
            Err(DecisionError::InvalidResponse)
        }
    }
}

fn valid_score(score: f64) -> bool {
    score.is_finite() && (0.0..=1.0).contains(&score)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EngineError {
    #[error("authority denied")]
    Denied,
    #[error("no authorized capability matches")]
    Gap,
    #[error("semantic route ambiguous")]
    Ambiguous,
    #[error("invalid routing decision")]
    InvalidDecision,
    #[error("invalid gateway arguments")]
    InvalidArguments,
    #[error("decision or gateway unavailable")]
    Unavailable,
}

#[async_trait]
pub trait AuthoritySource: Send + Sync {
    /// Resolve from trusted state. The caller supplies only a subject and grant identifier.
    async fn resolve(
        &self,
        subject: &Subject,
        grant_id: &str,
    ) -> Result<AuthorityContext, EngineError>;
}

#[derive(Default)]
pub struct InMemoryAuthoritySource {
    grants: RwLock<BTreeMap<(String, String, String), AuthorityContext>>,
}

impl InMemoryAuthoritySource {
    /// Called by trusted dispatch/policy code, never by an untrusted caller.
    pub async fn grant(&self, subject: Subject, authority: AuthorityContext) {
        self.grants.write().await.insert(
            (subject.kind, subject.id, authority.grant_id.clone()),
            authority,
        );
    }

    pub async fn revoke(&self, subject: &Subject, grant_id: &str) {
        self.grants.write().await.remove(&(
            subject.kind.clone(),
            subject.id.clone(),
            grant_id.to_owned(),
        ));
    }
}

#[async_trait]
impl AuthoritySource for InMemoryAuthoritySource {
    async fn resolve(
        &self,
        subject: &Subject,
        grant_id: &str,
    ) -> Result<AuthorityContext, EngineError> {
        self.grants
            .read()
            .await
            .get(&(
                subject.kind.clone(),
                subject.id.clone(),
                grant_id.to_owned(),
            ))
            .cloned()
            .ok_or(EngineError::Denied)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationRequest {
    pub intent: String,
    pub candidates: Vec<String>,
    pub confidence: f64,
}

#[async_trait]
pub trait EscalationPolicy: Send + Sync {
    /// Return a sanctioned candidate, or `None` to report ambiguity.
    async fn resolve(&self, request: EscalationRequest) -> Result<Option<String>, EngineError>;
}

pub struct ReturnAmbiguity;

#[async_trait]
impl EscalationPolicy for ReturnAmbiguity {
    async fn resolve(&self, _request: EscalationRequest) -> Result<Option<String>, EngineError> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStep {
    pub candidates: Vec<String>,
    pub selected: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditTrace {
    pub subject: Subject,
    pub grant_id: String,
    pub capability: String,
    pub decisions: Vec<RouteStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineResult {
    pub data: Value,
    pub audit: AuditTrace,
}

#[derive(Default)]
struct Counters {
    attempts: AtomicU64,
    completed: AtomicU64,
    denied: AtomicU64,
    ambiguous: AtomicU64,
    unavailable: AtomicU64,
    decisions: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingTelemetry {
    pub attempts: u64,
    pub completed: u64,
    pub denied: u64,
    pub ambiguous: u64,
    pub unavailable: u64,
    pub decisions: u64,
}

pub struct SeamEngine<D, A, G, E> {
    decision: Arc<D>,
    authority: Arc<A>,
    gateway: Arc<G>,
    escalation: Arc<E>,
    catalog: BTreeMap<String, Capability>,
    confidence_threshold: f64,
    counters: Counters,
}

impl<D, A, G, E> SeamEngine<D, A, G, E>
where
    D: DecisionEngine + 'static,
    A: AuthoritySource + 'static,
    G: Gateway + 'static,
    E: EscalationPolicy + 'static,
{
    pub fn new(decision: Arc<D>, authority: Arc<A>, gateway: Arc<G>, escalation: Arc<E>) -> Self {
        Self {
            decision,
            authority,
            gateway,
            escalation,
            catalog: BTreeMap::new(),
            confidence_threshold: 0.8,
            counters: Counters::default(),
        }
    }

    pub fn with_confidence_threshold(mut self, threshold: f64) -> Self {
        assert!((0.0..=1.0).contains(&threshold));
        self.confidence_threshold = threshold;
        self
    }

    pub fn register_capability(&mut self, capability: Capability) -> Result<(), EngineError> {
        if capability.name.is_empty()
            || capability.name.split('.').any(str::is_empty)
            || capability.name.starts_with("seam.decision.")
        {
            return Err(EngineError::InvalidArguments);
        }
        self.catalog.insert(capability.name.clone(), capability);
        Ok(())
    }

    pub fn telemetry(&self) -> RoutingTelemetry {
        RoutingTelemetry {
            attempts: self.counters.attempts.load(Ordering::Relaxed),
            completed: self.counters.completed.load(Ordering::Relaxed),
            denied: self.counters.denied.load(Ordering::Relaxed),
            ambiguous: self.counters.ambiguous.load(Ordering::Relaxed),
            unavailable: self.counters.unavailable.load(Ordering::Relaxed),
            decisions: self.counters.decisions.load(Ordering::Relaxed),
        }
    }

    pub async fn execute(&self, request: IntentRequest) -> Result<EngineResult, EngineError> {
        self.counters.attempts.fetch_add(1, Ordering::Relaxed);
        let result = self.execute_inner(request).await;
        let counter = match &result {
            Ok(_) => &self.counters.completed,
            Err(EngineError::Denied) => &self.counters.denied,
            Err(EngineError::Ambiguous) => &self.counters.ambiguous,
            Err(EngineError::Unavailable) => &self.counters.unavailable,
            Err(_) => return result,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        result
    }

    async fn execute_inner(&self, request: IntentRequest) -> Result<EngineResult, EngineError> {
        let grant = self
            .authority
            .resolve(&request.subject, &request.grant_id)
            .await?;
        if grant.grant_id != request.grant_id {
            return Err(EngineError::Denied);
        }
        if grant
            .expires_at_epoch_seconds
            .is_some_and(|expires| expires <= now_epoch_seconds())
        {
            return Err(EngineError::Denied);
        }
        let authorized: Vec<&Capability> = self
            .catalog
            .values()
            .filter(|capability| grant.allowed_capabilities.contains(&capability.name))
            .collect();
        if authorized.is_empty() {
            return Err(EngineError::Gap);
        }

        let mut remaining = authorized;
        let mut depth = 0usize;
        let mut steps = Vec::new();
        while remaining.len() > 1 {
            let groups: BTreeMap<String, Vec<&Capability>> =
                remaining
                    .iter()
                    .fold(BTreeMap::new(), |mut groups, capability| {
                        let key = capability
                            .name
                            .split('.')
                            .take(depth + 1)
                            .collect::<Vec<_>>()
                            .join(".");
                        groups.entry(key).or_insert_with(Vec::new).push(*capability);
                        groups
                    });
            if groups.len() == 1 {
                depth += 1;
                continue;
            }
            let candidates: Vec<String> = groups.keys().cloned().collect();
            self.counters.decisions.fetch_add(1, Ordering::Relaxed);
            let result = self
                .decision
                .decide(DecisionRequest {
                    form: DecisionForm::Choice,
                    question: request.intent.clone(),
                    candidates: candidates.clone(),
                    context: json!({"depth": depth}),
                })
                .await
                .map_err(|error| match error {
                    DecisionError::InvalidResponse => EngineError::InvalidDecision,
                    DecisionError::Unavailable => EngineError::Unavailable,
                })?;
            let DecisionResult::Choice {
                candidate,
                confidence,
            } = result
            else {
                return Err(EngineError::InvalidDecision);
            };
            if !valid_score(confidence) {
                return Err(EngineError::InvalidDecision);
            }
            if !groups.contains_key(&candidate) {
                return Err(EngineError::InvalidDecision);
            }
            let selected = if confidence >= self.confidence_threshold {
                candidate
            } else {
                self.escalation
                    .resolve(EscalationRequest {
                        intent: request.intent.clone(),
                        candidates: candidates.clone(),
                        confidence,
                    })
                    .await?
                    .ok_or(EngineError::Ambiguous)?
            };
            let Some(group) = groups.get(&selected) else {
                return Err(EngineError::InvalidDecision);
            };
            remaining = group.clone();
            steps.push(RouteStep {
                candidates,
                selected,
                confidence,
            });
            depth += 1;
        }
        let capability = remaining[0];
        schema::validate(&capability.argument_schema, &request.arguments)
            .map_err(|_| EngineError::InvalidArguments)?;
        let data = self
            .gateway
            .execute(GatewayRequest {
                scope: GatewayScope::Caller,
                capability: capability.name.clone(),
                intent: request.intent,
                arguments: request.arguments,
                subject: Some(request.subject.clone()),
            })
            .await
            .map_err(|_| EngineError::Unavailable)?;
        Ok(EngineResult {
            data,
            audit: AuditTrace {
                subject: request.subject,
                grant_id: request.grant_id,
                capability: capability.name.clone(),
                decisions: steps,
            },
        })
    }
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    #[derive(Default)]
    struct FixtureGateway {
        calls: Mutex<Vec<GatewayRequest>>,
    }

    #[async_trait]
    impl Gateway for FixtureGateway {
        async fn execute(&self, request: GatewayRequest) -> Result<Value, GatewayError> {
            let scope = request.scope;
            self.calls.lock().expect("lock").push(request);
            match scope {
                GatewayScope::Control => {
                    Ok(json!({"form":"choice", "candidate":"development.issue", "confidence":0.94}))
                }
                GatewayScope::Caller => Ok(json!({"issues":["fixture-1"]})),
            }
        }
    }

    fn subject() -> Subject {
        Subject {
            id: "agent-1".into(),
            kind: "another-runtime".into(),
        }
    }

    async fn setup() -> (
        SeamEngine<
            GatewayDecisionEngine<FixtureGateway>,
            InMemoryAuthoritySource,
            FixtureGateway,
            ReturnAmbiguity,
        >,
        Arc<InMemoryAuthoritySource>,
        Arc<FixtureGateway>,
    ) {
        let store = Arc::new(InMemoryAuthoritySource::default());
        let gateway = Arc::new(FixtureGateway::default());
        let mut engine = SeamEngine::new(
            Arc::new(GatewayDecisionEngine::new(gateway.clone())),
            store.clone(),
            gateway.clone(),
            Arc::new(ReturnAmbiguity),
        );
        for name in [
            "development.issue.search",
            "development.repository.read",
            "web.search",
        ] {
            engine
                .register_capability(Capability {
                    name: name.into(),
                    description: name.into(),
                    argument_schema: json!({"type":"object", "additionalProperties": false}),
                })
                .expect("catalog");
        }
        (engine, store, gateway)
    }

    fn intent() -> IntentRequest {
        IntentRequest {
            subject: subject(),
            grant_id: "grant-1".into(),
            intent: "Find the issue".into(),
            arguments: json!({}),
        }
    }

    async fn grant(store: &InMemoryAuthoritySource) {
        store
            .grant(
                subject(),
                AuthorityContext {
                    grant_id: "grant-1".into(),
                    allowed_capabilities: BTreeSet::from([
                        "development.issue.search".into(),
                        "development.repository.read".into(),
                    ]),
                    expires_at_epoch_seconds: None,
                },
            )
            .await;
    }

    #[tokio::test]
    async fn another_runtime_can_route_through_control_and_caller_gateway_scopes() {
        let (engine, store, gateway) = setup().await;
        grant(&store).await;
        let result = engine.execute(intent()).await.expect("execute");
        assert_eq!(result.audit.capability, "development.issue.search");
        assert_eq!(result.audit.decisions.len(), 1);
        let calls = gateway.calls.lock().expect("lock");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].scope, GatewayScope::Control);
        assert_eq!(calls[0].capability, "seam.decision.choice");
        assert!(calls[0].subject.is_none());
        assert_eq!(
            calls[0].arguments["candidates"],
            json!(["development.issue", "development.repository"])
        );
        assert_eq!(calls[1].scope, GatewayScope::Caller);
        assert_eq!(calls[1].capability, "development.issue.search");
        assert_eq!(engine.telemetry().completed, 1);
        assert_eq!(engine.telemetry().decisions, 1);
    }

    #[tokio::test]
    async fn unknown_or_revoked_grant_is_denied_before_decision() {
        let (engine, store, gateway) = setup().await;
        assert_eq!(
            engine.execute(intent()).await.unwrap_err(),
            EngineError::Denied
        );
        grant(&store).await;
        store.revoke(&subject(), "grant-1").await;
        assert_eq!(
            engine.execute(intent()).await.unwrap_err(),
            EngineError::Denied
        );
        assert!(gateway.calls.lock().expect("lock").is_empty());
        assert_eq!(engine.telemetry().denied, 2);
    }

    #[tokio::test]
    async fn expired_grant_is_denied() {
        let (engine, store, gateway) = setup().await;
        store
            .grant(
                subject(),
                AuthorityContext {
                    grant_id: "grant-1".into(),
                    allowed_capabilities: BTreeSet::from(["development.issue.search".into()]),
                    expires_at_epoch_seconds: Some(1),
                },
            )
            .await;
        assert_eq!(
            engine.execute(intent()).await.unwrap_err(),
            EngineError::Denied
        );
        assert!(gateway.calls.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn arguments_are_validated_before_gateway_execution() {
        let (engine, store, gateway) = setup().await;
        grant(&store).await;
        let mut request = intent();
        request.arguments = json!({"unexpected": true});
        assert_eq!(
            engine.execute(request).await.unwrap_err(),
            EngineError::InvalidArguments
        );
        assert_eq!(gateway.calls.lock().expect("lock").len(), 1);
    }

    struct FixedDecision {
        candidate: String,
        confidence: f64,
    }

    #[async_trait]
    impl DecisionEngine for FixedDecision {
        async fn decide(&self, _request: DecisionRequest) -> Result<DecisionResult, DecisionError> {
            Ok(DecisionResult::Choice {
                candidate: self.candidate.clone(),
                confidence: self.confidence,
            })
        }
    }

    #[tokio::test]
    async fn invented_candidate_never_reaches_gateway() {
        let store = Arc::new(InMemoryAuthoritySource::default());
        grant(&store).await;
        let gateway = Arc::new(FixtureGateway::default());
        let mut engine = SeamEngine::new(
            Arc::new(FixedDecision {
                candidate: "web.search".into(),
                confidence: 1.0,
            }),
            store,
            gateway.clone(),
            Arc::new(ReturnAmbiguity),
        );
        for name in [
            "development.issue.search",
            "development.repository.read",
            "web.search",
        ] {
            engine
                .register_capability(Capability {
                    name: name.into(),
                    description: "".into(),
                    argument_schema: json!({}),
                })
                .expect("catalog");
        }
        assert_eq!(
            engine.execute(intent()).await.unwrap_err(),
            EngineError::InvalidDecision
        );
        assert!(gateway.calls.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn low_confidence_returns_ambiguity() {
        let store = Arc::new(InMemoryAuthoritySource::default());
        grant(&store).await;
        let gateway = Arc::new(FixtureGateway::default());
        let mut engine = SeamEngine::new(
            Arc::new(FixedDecision {
                candidate: "development.issue".into(),
                confidence: 0.2,
            }),
            store,
            gateway.clone(),
            Arc::new(ReturnAmbiguity),
        );
        for name in ["development.issue.search", "development.repository.read"] {
            engine
                .register_capability(Capability {
                    name: name.into(),
                    description: "".into(),
                    argument_schema: json!({}),
                })
                .expect("catalog");
        }
        assert_eq!(
            engine.execute(intent()).await.unwrap_err(),
            EngineError::Ambiguous
        );
        assert!(gateway.calls.lock().expect("lock").is_empty());
    }

    struct ScriptedDecision {
        choices: Mutex<VecDeque<String>>,
    }

    #[async_trait]
    impl DecisionEngine for ScriptedDecision {
        async fn decide(&self, _request: DecisionRequest) -> Result<DecisionResult, DecisionError> {
            Ok(DecisionResult::Choice {
                candidate: self
                    .choices
                    .lock()
                    .expect("lock")
                    .pop_front()
                    .expect("choice"),
                confidence: 0.99,
            })
        }
    }

    #[tokio::test]
    async fn routing_can_narrow_across_more_than_two_layers() {
        let store = Arc::new(InMemoryAuthoritySource::default());
        let names = [
            "development.issue.search",
            "research.news.search",
            "research.academic.paper.search",
            "research.academic.paper.fetch",
        ];
        store
            .grant(
                subject(),
                AuthorityContext {
                    grant_id: "grant-1".into(),
                    allowed_capabilities: names.iter().map(|name| (*name).to_owned()).collect(),
                    expires_at_epoch_seconds: None,
                },
            )
            .await;
        let gateway = Arc::new(FixtureGateway::default());
        let choices = [
            "research",
            "research.academic",
            "research.academic.paper.search",
        ];
        let mut engine = SeamEngine::new(
            Arc::new(ScriptedDecision {
                choices: Mutex::new(choices.iter().map(|choice| (*choice).to_owned()).collect()),
            }),
            store,
            gateway,
            Arc::new(ReturnAmbiguity),
        );
        for name in names {
            engine
                .register_capability(Capability {
                    name: name.into(),
                    description: String::new(),
                    argument_schema: json!({"type":"object"}),
                })
                .expect("catalog");
        }
        let result = engine.execute(intent()).await.expect("route");
        assert_eq!(result.audit.capability, "research.academic.paper.search");
        assert_eq!(result.audit.decisions.len(), 3);
        assert_eq!(engine.telemetry().decisions, 3);
    }
}
