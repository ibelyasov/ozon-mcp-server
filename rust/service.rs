use crate::config::Config;
use crate::contracts;
use crate::evidence::{
    self, MAX_IMAGE_WIRE_BYTES, MAX_JSON_UTF16, envelope, error_code, safe_message,
};
use crate::gateway::Gateway;
use crate::images;
use crate::marketplace::item_error;
use crate::marketplace::{self, Normalized, ReviewRequest};
use crate::search::SearchArgs;
use crate::store::{Store, StoredRef};
use crate::wire::{ImagePayload, ToolReply};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(55);
const PRODUCT_BATCH_CONFIRMATION_RESERVE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

struct Inner {
    store: StdMutex<Store>,
    gateway: Mutex<Option<GatewayBackend>>,
    admission: Arc<Semaphore>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    shutdown: CancellationToken,
}

#[cfg(test)]
enum FakeAction {
    Value(Value),
    WaitForCancellation { cleanup: Duration },
}

#[cfg(test)]
struct FakeGateway {
    contexts: std::collections::VecDeque<Value>,
    context_delay: Duration,
    searches: std::collections::VecDeque<FakeAction>,
    products: std::collections::VecDeque<FakeAction>,
    reviews: std::collections::VecDeque<FakeAction>,
    image_content_observed: bool,
    context_signature: Option<Value>,
}

#[cfg(test)]
impl FakeGateway {
    async fn context(&mut self, cancel: &CancellationToken) -> Result<Value> {
        if !self.context_delay.is_zero() {
            tokio::select! {
                _ = cancel.cancelled() => return Err(anyhow!("CANCELLED: fake context cancelled")),
                _ = tokio::time::sleep(self.context_delay) => {}
            }
        }
        let mut context = self
            .contexts
            .pop_front()
            .ok_or_else(|| anyhow!("SOURCE_CHANGED: fake context script exhausted"))?;
        let signature = context.get("signature").cloned().unwrap_or(Value::Null);
        if self
            .context_signature
            .as_ref()
            .is_some_and(|previous| previous != &signature)
        {
            self.image_content_observed = false;
        }
        self.context_signature = Some(signature);
        if self.image_content_observed {
            context["capabilities"]["image_content"] = json!("available");
        }
        Ok(context)
    }

    async fn search(&mut self, _args: SearchArgs, cancel: &CancellationToken) -> Result<Value> {
        run_fake_action(self.searches.pop_front(), cancel, "search").await
    }

    async fn product(&mut self, _product: &str, cancel: &CancellationToken) -> Result<Value> {
        run_fake_action(self.products.pop_front(), cancel, "product").await
    }

    async fn reviews(
        &mut self,
        _path: &str,
        _limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        run_fake_action(self.reviews.pop_front(), cancel, "reviews").await
    }
}

#[cfg(test)]
async fn run_fake_action(
    action: Option<FakeAction>,
    cancel: &CancellationToken,
    operation: &str,
) -> Result<Value> {
    match action {
        Some(FakeAction::Value(value)) => Ok(value),
        Some(FakeAction::WaitForCancellation { cleanup }) => {
            cancel.cancelled().await;
            tokio::time::sleep(cleanup).await;
            Err(anyhow!("CANCELLED: fake {operation} cancelled"))
        }
        None => Err(anyhow!("SOURCE_CHANGED: fake {operation} script exhausted")),
    }
}

enum GatewayBackend {
    Real(Gateway),
    #[cfg(test)]
    Fake(FakeGateway),
}

impl GatewayBackend {
    async fn context(&mut self, cancel: &CancellationToken) -> Result<Value> {
        match self {
            Self::Real(gateway) => gateway.context(cancel).await,
            #[cfg(test)]
            Self::Fake(gateway) => gateway.context(cancel).await,
        }
    }

    async fn search(&mut self, args: SearchArgs, cancel: &CancellationToken) -> Result<Value> {
        match self {
            Self::Real(gateway) => gateway.search(args, cancel).await,
            #[cfg(test)]
            Self::Fake(gateway) => gateway.search(args, cancel).await,
        }
    }

    async fn product(&mut self, product: &str, cancel: &CancellationToken) -> Result<Value> {
        match self {
            Self::Real(gateway) => gateway.product(product, cancel).await,
            #[cfg(test)]
            Self::Fake(gateway) => gateway.product(product, cancel).await,
        }
    }

    async fn reviews(
        &mut self,
        path: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        match self {
            Self::Real(gateway) => gateway.reviews(path, limit, cancel).await,
            #[cfg(test)]
            Self::Fake(gateway) => gateway.reviews(path, limit, cancel).await,
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Real(gateway) => gateway.shutdown().await,
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
        }
    }

    fn mark_image_content_available(&mut self) {
        match self {
            Self::Real(gateway) => gateway.mark_image_content_available(),
            #[cfg(test)]
            Self::Fake(gateway) => gateway.image_content_observed = true,
        }
    }
}

impl Service {
    pub async fn new(config: &Config) -> Result<Self> {
        let mut store = Store::open(&config.root).context("open research store")?;
        store.maintain().context("maintain research store")?;
        Ok(Self {
            inner: Arc::new(Inner {
                store: StdMutex::new(store),
                gateway: Mutex::new(None),
                admission: Arc::new(Semaphore::new(8)),
                tasks: Mutex::new(vec![]),
                shutdown: CancellationToken::new(),
            }),
        })
    }

    pub async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> ToolReply {
        if !contracts::tool_names().contains(&name) {
            return contracts::failure("INVALID_ARGUMENT", "Unknown tool.", research_arg(&args));
        }
        if let Err(error) = contracts::validate_input(name, &args) {
            return contracts::failure(
                "INVALID_ARGUMENT",
                &safe_message(&error),
                research_arg(&args),
            );
        }
        if price_range_inverted(&args) {
            return contracts::failure(
                "INVALID_ARGUMENT",
                "priceRange.minMinor must not exceed maxMinor.",
                research_arg(&args),
            );
        }
        let call_permit = match self.inner.admission.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return contracts::failure(
                    "SERVER_BUSY",
                    "Eight requests are already active or waiting.",
                    research_arg(&args),
                );
            }
        };
        let (tx, rx) = oneshot::channel();
        let inner = self.inner.clone();
        let tool = name.to_owned();
        let research = research_arg(&args).map(str::to_owned);
        let request_cancel = cancel.child_token();
        let deadline = Instant::now() + DEADLINE;
        let operation_deadline = deadline - Duration::from_secs(11);
        let mut tasks = self.inner.tasks.lock().await;
        tasks.retain(|task| !task.is_finished());
        let handle = tokio::spawn(async move {
            let _call_permit = call_permit;
            let work_inner = inner.clone();
            let work_cancel = request_cancel.child_token();
            let work_cancel_for_task = work_cancel.clone();
            let mut work = tokio::spawn(async move {
                execute(
                    work_inner,
                    &tool,
                    args,
                    work_cancel_for_task,
                    operation_deadline,
                )
                .await
            });
            let reply = tokio::select! {
                result = &mut work => result.unwrap_or_else(|_| contracts::failure("SOURCE_CHANGED", "The server task failed.", research.as_deref())),
                _ = request_cancel.cancelled() => {
                    work_cancel.cancel();
                    let reply = contracts::failure("CANCELLED", "The request was cancelled.", research.as_deref());
                    let _ = tx.send(reply);
                    let _ = work.await;
                    return;
                }
                _ = inner.shutdown.cancelled() => {
                    work_cancel.cancel();
                    let reply = contracts::failure("CANCELLED", "The server is shutting down.", research.as_deref());
                    let _ = tx.send(reply);
                    let _ = work.await;
                    return;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    match tokio::time::timeout(Duration::from_secs(5), &mut work).await {
                        Ok(result) => result.unwrap_or_else(|_| contracts::failure("SOURCE_CHANGED", "The server task failed.", research.as_deref())),
                        Err(_) => {
                            work_cancel.cancel();
                            let reply = contracts::failure("UPSTREAM_TIMEOUT", "The request deadline expired and cleanup is still completing.", research.as_deref());
                            let _ = tx.send(reply);
                            let _ = work.await;
                            return;
                        }
                    }
                }
            };
            let _ = tx.send(reply);
        });
        tasks.push(handle);
        drop(tasks);
        rx.await.unwrap_or_else(|_| {
            contracts::failure(
                "CANCELLED",
                "The request ended before a result was available.",
                research_arg_fallback(name),
            )
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.inner.shutdown.cancel();
        let gateway_result = {
            let mut guard = self.inner.gateway.lock().await;
            match guard.as_mut() {
                Some(gateway) => gateway.shutdown().await,
                None => Ok(()),
            }
        };
        let mut tasks = self.inner.tasks.lock().await;
        for handle in tasks.drain(..) {
            let _ = handle.await;
        }
        gateway_result
    }
}

// Bind presentation to continuation authority before any source operation.
fn presentation_args(inner: &Inner, name: &str, mut args: Value) -> Result<Value> {
    if !matches!(
        name,
        "ozon_search" | "ozon_get_products" | "ozon_get_reviews"
    ) {
        return Ok(args);
    }
    let mut bindings = Vec::new();
    if name == "ozon_get_products" {
        // Cursor views are resolved only after per-item research/context checks.
        args["_viewExplicit"] = json!(args.get("view").is_some());
    } else if let Some(cursor) = args.pointer("/start/cursor").and_then(Value::as_str) {
        let kind = if name == "ozon_search" {
            "search_cursor"
        } else {
            "review_cursor"
        };
        let stored = locked_store(inner)?.get_ref(cursor, kind)?;
        bindings.push(
            stored
                .value
                .get("presentation")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"view":"full"})),
        );
    } else if let Some(cursor) = args
        .pointer("/start/refinementsCursor")
        .and_then(Value::as_str)
    {
        let stored = locked_store(inner)?.get_ref(cursor, "search_refinements")?;
        bindings.push(stored.value.clone());
    }
    for binding in bindings {
        let keys: &[&str] = if name == "ozon_get_reviews" {
            &["view", "includeFacets"]
        } else {
            &["view", "repeatMode"]
        };
        for &key in keys {
            if let Some(value) = binding.get(key).filter(|v| !v.is_null()) {
                if args.get(key).is_some_and(|requested| requested != value) {
                    return Err(anyhow!(
                        "INVALID_ARGUMENT: continuation presentation cannot be changed"
                    ));
                }
                args[key] = value.clone();
            }
        }
        if args.pointer("/start/refinementsCursor").is_some()
            && let Some(limit) = binding.get("limit")
        {
            if args.get("refinementLimit").is_some_and(|v| v != limit) {
                return Err(anyhow!(
                    "INVALID_ARGUMENT: refinement page limit cannot be changed"
                ));
            }
            args["refinementLimit"] = limit.clone();
        }
    }
    if args.get("view").is_none() {
        args["view"] = json!("compact");
    }
    if name == "ozon_search" {
        if args.get("repeatMode").is_none() {
            args["repeatMode"] = json!("full");
        }
        if args.get("refinementLimit").is_none() {
            args["refinementLimit"] = json!(12);
        }
    }
    if matches!(name, "ozon_search" | "ozon_get_reviews") && args.get("includeFacets").is_none() {
        args["includeFacets"] =
            json!(args["view"] == "full" && args.pointer("/start/cursor").is_none());
    }
    Ok(args)
}

fn presentation_binding(args: &Value) -> Value {
    json!({"view":args["view"],"repeatMode":args.get("repeatMode").cloned().unwrap_or(json!("full")),"includeFacets":args.get("includeFacets").cloned().unwrap_or(json!(false))})
}

async fn execute(
    inner: Arc<Inner>,
    name: &str,
    args: Value,
    cancel: CancellationToken,
    deadline: Instant,
) -> ToolReply {
    let args = match presentation_args(&inner, name, args) {
        Ok(args) => args,
        Err(error) => return contracts::failure(error_code(&error), &safe_message(&error), None),
    };
    let result = match name {
        "ozon_get_context" => get_context(&inner, &cancel, deadline).await,
        "ozon_search" => search(&inner, &args, &cancel, deadline).await,
        "ozon_get_products" => products(&inner, &args, &cancel, deadline).await,
        "ozon_get_reviews" => reviews(&inner, &args, &cancel, deadline).await,
        "ozon_get_images" => get_images(&inner, &args, &cancel, deadline).await,
        "ozon_list_research" => journal(&inner, name, &args),
        "ozon_get_research" => journal(&inner, name, &args),
        "ozon_append_research_note" => journal(&inner, name, &args),
        _ => Err(anyhow!("INVALID_ARGUMENT: unknown tool")),
    };
    match result {
        Ok((mut value, images)) => {
            let projected = (|| -> Result<Value> {
                if name == "ozon_search" {
                    crate::presentation::apply_repeat_mode(
                        &mut value,
                        args["repeatMode"].as_str().unwrap_or("full"),
                    )?;
                }
                if matches!(
                    name,
                    "ozon_search" | "ozon_get_products" | "ozon_get_reviews"
                ) {
                    let view = value
                        .get("view")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| args["view"].as_str().unwrap_or("compact"))
                        .to_owned();
                    crate::presentation::apply(name, value, &view)
                } else {
                    Ok(value)
                }
            })();
            match projected {
                Ok(value) => success(name, value, images, research_arg(&args)),
                Err(error) => contracts::failure(
                    error_code(&error),
                    &safe_message(&error),
                    research_arg(&args),
                ),
            }
        }
        Err(error) => contracts::failure(
            error_code(&error),
            &safe_message(&error),
            research_arg(&args),
        ),
    }
}

async fn get_context(
    inner: &Inner,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Value, Vec<ImagePayload>)> {
    let raw = gateway_call(inner, cancel, deadline, |gateway, token| {
        Box::pin(gateway.context(token))
    })
    .await?;
    let context = bind_context(inner, &raw)?;
    let n = marketplace::normalize_context(&with_context_id(raw, &context));
    let observed = n.data["observedAt"].as_str().unwrap_or("").to_owned();
    Ok((
        envelope(
            None,
            n.data.clone(),
            &context,
            n.evidence,
            n.warnings,
            &observed,
        ),
        vec![],
    ))
}

async fn search(
    inner: &Inner,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Value, Vec<ImagePayload>)> {
    if let Some(cursor) = args
        .pointer("/start/refinementsCursor")
        .and_then(Value::as_str)
    {
        if cancel.is_cancelled() {
            return Err(anyhow!("CANCELLED: request cancelled"));
        }
        let mut store = locked_store(inner)?;
        let context = cached_context(&store)?;
        let context_id = required_text(&context, "contextId")?;
        let value = crate::refinements::continue_page(
            &mut store,
            cursor,
            research_arg(args),
            context_id,
            args["view"].as_str(),
            args["repeatMode"].as_str(),
            args["refinementLimit"].as_u64().map(|n| n as usize),
        )?;
        return Ok((value, vec![]));
    }
    let context = observe_context(inner, cancel, deadline).await?;
    let context_id = required_text(&context, "contextId")?;
    let start = &args["start"];
    let (research_id, search_args, summary) =
        if let Some(query) = start.get("query").and_then(Value::as_str) {
            let rid = match research_arg(args) {
                Some(id) => {
                    locked_store(inner)?.ensure_research(id, context_id)?;
                    id.to_owned()
                }
                None => locked_store(inner)?.create_research(query, &context)?,
            };
            (
                rid,
                make_search_args(start, args, Some(query), None, None),
                format!("Search: {query}"),
            )
        } else if let Some(reference) = start.get("searchRef").and_then(Value::as_str) {
            let stored = locked_store(inner)?.get_ref(reference, "search_ref")?;
            verify_bound(&stored, args, context_id)?;
            (
                stored.research_id.clone(),
                make_search_args(
                    start,
                    args,
                    None,
                    stored.value.get("searchUrl").and_then(Value::as_str),
                    None,
                ),
                "Applied observed search refinement".into(),
            )
        } else {
            let stored =
                locked_store(inner)?.get_ref(required_text(start, "cursor")?, "search_cursor")?;
            verify_bound(&stored, args, context_id)?;
            (
                stored.research_id.clone(),
                make_search_args(
                    start,
                    args,
                    None,
                    None,
                    stored.value.get("cursor").and_then(Value::as_str),
                ),
                "Continued search".into(),
            )
        };
    let mut raw = match gateway_call(inner, cancel, deadline, move |gateway, token| {
        Box::pin(gateway.search(search_args, token))
    })
    .await
    {
        Ok(raw) => raw,
        Err(error) => {
            if error_code(&error) == "CANCELLED" {
                record_cancelled(inner, &research_id)?;
            }
            return Err(error);
        }
    };
    if let Err(error) = confirm_context(inner, cancel, deadline, context_id).await {
        if error_code(&error) == "CANCELLED" {
            record_cancelled(inner, &research_id)?;
        }
        return Err(error);
    }
    raw["_presentation"] = presentation_binding(args);
    let include = args
        .get("includeFacets")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let n = {
        let mut store = locked_store(inner)?;
        let mut n = marketplace::normalize_search(
            &raw,
            &mut store,
            &research_id,
            context_id,
            context.get("regionVerification").and_then(Value::as_str) == Some("verified"),
            include,
        )?;
        let observed = oldest_observation(&n.evidence);
        // Admit refinement continuation before publishing the candidate baseline.
        crate::refinements::first_page(
            &mut store,
            &research_id,
            &mut n.data,
            &context,
            &observed,
            args["view"].as_str().unwrap_or("compact"),
            args["repeatMode"].as_str().unwrap_or("full"),
            args["refinementLimit"].as_u64().unwrap_or(12) as usize,
        )?;
        store.record_search(
            &research_id,
            &summary,
            &n.product_refs,
            &n.evidence,
            &mut n.data["items"],
        )?;
        n
    };
    if cancel.is_cancelled() {
        record_cancelled(inner, &research_id)?;
        return Err(anyhow!("CANCELLED: request cancelled"));
    }
    let observed = oldest_observation(&n.evidence);
    let value = envelope(
        Some(&research_id),
        n.data,
        &context,
        n.evidence,
        n.warnings,
        &observed,
    );
    Ok((value, vec![]))
}

async fn products(
    inner: &Inner,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Value, Vec<ImagePayload>)> {
    let context = observe_context(inner, cancel, deadline).await?;
    let context_id = required_text(&context, "contextId")?.to_owned();
    let selectors = args["products"]
        .as_array()
        .ok_or_else(|| anyhow!("INVALID_ARGUMENT: products is required"))?;
    let mut resolved = Vec::with_capacity(selectors.len());
    let mut inferred: Option<String> = research_arg(args).map(str::to_owned);
    for selector in selectors {
        let resolved_product = resolve_product(inner, selector).and_then(|resolved_product| {
            if resolved_product
                .context_id
                .as_ref()
                .is_some_and(|bound| bound != &context_id)
            {
                return Err(anyhow!(
                    "CONTEXT_CHANGED: product reference belongs to another context"
                ));
            }
            if let Some(rid) = &resolved_product.research_id {
                if inferred.as_ref().is_some_and(|v| v != rid) {
                    return Err(anyhow!("INVALID_REFERENCE: mixed-research product batch"));
                }
                inferred = Some(rid.clone());
            }
            Ok(resolved_product)
        });
        resolved.push(resolved_product);
    }
    let mut selected_view = args["view"].as_str().unwrap_or("compact").to_owned();
    let mut view_bound = args["_viewExplicit"].as_bool().unwrap_or(false);
    for item in &mut resolved {
        let Ok(product) = item else {
            continue;
        };
        let Some(binding) = &product.presentation else {
            continue;
        };
        let view = binding
            .get("view")
            .and_then(Value::as_str)
            .unwrap_or("full");
        if view_bound && selected_view != view {
            *item = Err(anyhow!(
                "INVALID_REFERENCE: product cursor presentation cannot be changed"
            ));
        } else {
            selected_view = view.to_owned();
            view_bound = true;
        }
    }
    let research_id = match inferred {
        Some(rid) => {
            locked_store(inner)?.ensure_research(&rid, &context_id)?;
            rid
        }
        None => locked_store(inner)?.create_research("Direct product lookup", &context)?,
    };
    let includes = args
        .get("include")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| {
            if selected_view == "full" {
                vec!["characteristics".into(), "offers".into()]
            } else {
                vec!["characteristics".into()]
            }
        });
    let mut results = vec![];
    let mut all_evidence = vec![];
    let mut product_refs = vec![];
    let mut warnings = vec![];
    let confirmation_reserve = PRODUCT_BATCH_CONFIRMATION_RESERVE
        .min(deadline.saturating_duration_since(Instant::now()) / 2);
    let product_deadline = deadline - confirmation_reserve;
    let mut fetched = Vec::with_capacity(selectors.len());
    for resolved in resolved {
        if cancel.is_cancelled() {
            break;
        }
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                fetched.push(Err(error));
                continue;
            }
        };
        let target = resolved.target;
        let item_include = resolved.include.unwrap_or_else(|| includes.clone());
        let cached = resolved.cached_raw;
        let fetched_live = cached.is_none();
        let raw_result = match cached {
            Some(raw) => Ok(raw),
            None => {
                gateway_call(inner, cancel, product_deadline, move |gateway, token| {
                    Box::pin(async move { gateway.product(&target, token).await })
                })
                .await
            }
        };
        fetched.push(raw_result.map(|mut raw| {
            if fetched_live {
                raw["_continuationObservedAt"] = json!(evidence::now());
            }
            raw["_presentation"] = json!({"view":selected_view,"repeatMode":"full"});
            (raw, item_include, fetched_live)
        }));
        tokio::task::yield_now().await;
    }
    let needs_confirmation = fetched.iter().any(|item| matches!(item, Ok((_, _, true))));
    let confirmation_error = if needs_confirmation {
        if Instant::now() < deadline {
            confirm_context(inner, cancel, deadline, &context_id)
                .await
                .err()
                .map(|error| (error_code(&error).to_owned(), safe_message(&error)))
        } else {
            Some((
                "UPSTREAM_TIMEOUT".to_owned(),
                "The product batch exhausted its context confirmation budget.".to_owned(),
            ))
        }
    } else {
        None
    };
    for (selector, fetched) in selectors.iter().zip(fetched) {
        let (raw, item_include, fetched_live) = match fetched {
            Ok(fetched) => fetched,
            Err(error) => {
                results.push(item_error(
                    selector,
                    error_code(&error),
                    &safe_message(&error),
                ));
                continue;
            }
        };
        if fetched_live && let Some((code, message)) = &confirmation_error {
            results.push(item_error(selector, code, message));
            continue;
        }
        let normalization = {
            let mut store = locked_store(inner)?;
            marketplace::normalize_product(
                &raw,
                selector,
                &mut store,
                &research_id,
                &context_id,
                &item_include,
            )
        };
        match normalization {
            Ok(normalized) => {
                results.push(normalized.result);
                all_evidence.extend(normalized.evidence);
                product_refs.extend(normalized.product_refs);
                warnings.extend(normalized.warnings)
            }
            Err(error) => results.push(item_error(
                selector,
                error_code(&error),
                &safe_message(&error),
            )),
        }
    }
    let n = Normalized {
        data: json!({"results":results}),
        evidence: all_evidence,
        warnings: dedup_warnings(warnings),
        product_refs,
    };
    record(
        inner,
        &research_id,
        "products",
        "Fetched product details",
        &n,
    )?;
    if cancel.is_cancelled() {
        record_cancelled(inner, &research_id)?;
        return Err(anyhow!("CANCELLED: request cancelled"));
    }
    let observed = oldest_observation(&n.evidence);
    let mut value = envelope(
        Some(&research_id),
        n.data,
        &context,
        n.evidence,
        n.warnings,
        &observed,
    );
    value["view"] = json!(selected_view);
    Ok((value, vec![]))
}

async fn reviews(
    inner: &Inner,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Value, Vec<ImagePayload>)> {
    let context = observe_context(inner, cancel, deadline).await?;
    let cid = required_text(&context, "contextId")?;
    let start = &args["start"];
    let (
        rid,
        pref,
        sku,
        path,
        source,
        cached_raw,
        prior_seen_review_keys,
        prior_seen_ref,
        prior_seen_depth,
        prior_no_progress_pages,
        prior_page_made_progress,
    ) = if let Some(pref) = start.get("productRef").and_then(Value::as_str) {
        let r = locked_store(inner)?.get_ref(pref, "product")?;
        verify_bound(&r, args, cid)?;
        (
            r.research_id,
            pref.to_owned(),
            required_text(&r.value, "sku")?.to_owned(),
            r.value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            r.value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            None,
            vec![],
            None,
            0,
            0,
            false,
        )
    } else {
        let (key, kind) = if start.get("reviewSearchRef").is_some() {
            ("reviewSearchRef", "review_search")
        } else {
            ("cursor", "review_cursor")
        };
        let reference = required_text(start, key)?;
        let store = locked_store(inner)?;
        let r = store.get_ref(reference, kind)?;
        verify_bound(&r, args, cid)?;
        if kind == "review_cursor"
            && !r.value.as_object().is_some_and(|value| {
                value.contains_key("seenRef")
                    && value.contains_key("noProgressPages")
                    && value.contains_key("pageMadeProgress")
            })
        {
            return Err(anyhow!(
                "INVALID_REFERENCE: review cursor predates cumulative coverage; restart reviews from productRef"
            ));
        }
        let cached_raw = r.value.get("cachedRaw").cloned();
        let prior_seen_ref = if kind == "review_cursor" {
            r.value
                .get("seenRef")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            None
        };
        let prior_no_progress_pages = if kind == "review_cursor" {
            r.value
                .get("noProgressPages")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value < marketplace::MAX_REVIEW_NO_PROGRESS_PAGES)
                .ok_or_else(|| anyhow!("INVALID_REFERENCE: invalid review cursor progress"))?
        } else {
            0
        };
        let prior_page_made_progress = if kind == "review_cursor" {
            r.value
                .get("pageMadeProgress")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("INVALID_REFERENCE: invalid review cursor progress"))?
        } else {
            false
        };
        let loaded_seen = load_review_seen(
            &store,
            prior_seen_ref.as_deref(),
            &r.research_id,
            cid,
            cancel,
            deadline,
        )?;
        debug_assert!(loaded_seen.nodes_read <= marketplace::REVIEW_SEEN_CHECKPOINT_INTERVAL);
        (
            r.research_id,
            required_text(&r.value, "productRef")?.to_owned(),
            required_text(&r.value, "sku")?.to_owned(),
            r.value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            required_text(&r.value, "sourceUrl")?.to_owned(),
            cached_raw,
            loaded_seen.keys,
            prior_seen_ref,
            loaded_seen.depth,
            prior_no_progress_pages,
            prior_page_made_progress,
        )
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
    let fetched_live = cached_raw.is_none();
    let raw_result = match cached_raw {
        Some(raw) => Ok(raw),
        None => {
            let call_path = path.clone();
            gateway_call(inner, cancel, deadline, move |gateway, token| {
                Box::pin(async move { gateway.reviews(&call_path, limit, token).await })
            })
            .await
        }
    };
    let mut raw = match raw_result {
        Ok(raw) => raw,
        Err(error) => {
            if error_code(&error) == "CANCELLED" {
                record_cancelled(inner, &rid)?;
            }
            return Err(error);
        }
    };
    if fetched_live && let Err(error) = confirm_context(inner, cancel, deadline, cid).await {
        if error_code(&error) == "CANCELLED" {
            record_cancelled(inner, &rid)?;
        }
        return Err(error);
    }
    raw["_presentation"] = presentation_binding(args);
    let mut n = {
        let mut store = locked_store(inner)?;
        marketplace::normalize_reviews(
            &raw,
            &mut store,
            ReviewRequest {
                research_id: &rid,
                context_id: cid,
                product_ref: &pref,
                sku: &sku,
                source_url: &source,
                limit,
                prior_seen_review_keys: &prior_seen_review_keys,
                prior_seen_ref: prior_seen_ref.as_deref(),
                prior_seen_depth,
                current_path: &path,
                prior_no_progress_pages,
                prior_page_made_progress,
            },
        )?
    };
    let include_facets = args["includeFacets"].as_bool().unwrap_or(false);
    if !include_facets {
        n.data["refinements"] = json!([]);
    }
    n.data["refinementsIncluded"] = json!(include_facets);
    record(inner, &rid, "reviews", "Fetched product reviews", &n)?;
    if cancel.is_cancelled() {
        record_cancelled(inner, &rid)?;
        return Err(anyhow!("CANCELLED: request cancelled"));
    }
    let observed = oldest_observation(&n.evidence);
    Ok((
        envelope(
            Some(&rid),
            n.data,
            &context,
            n.evidence,
            n.warnings,
            &observed,
        ),
        vec![],
    ))
}

async fn get_images(
    inner: &Inner,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Value, Vec<ImagePayload>)> {
    let context = observe_context(inner, cancel, deadline).await?;
    let cid = required_text(&context, "contextId")?;
    let refs = args["imageRefs"]
        .as_array()
        .ok_or_else(|| anyhow!("INVALID_ARGUMENT: imageRefs is required"))?;
    let mut bindings = vec![];
    let mut rid = research_arg(args).map(str::to_owned);
    for image_ref in refs.iter().filter_map(Value::as_str) {
        match locked_store(inner)?.get_ref(image_ref, "image") {
            Ok(r) => {
                if rid.as_ref().is_some_and(|id| id != &r.research_id) {
                    return Err(anyhow!("INVALID_REFERENCE: mixed-research image batch"));
                }
                rid = Some(r.research_id.clone());
                bindings.push((image_ref.to_owned(), r))
            }
            Err(e) => bindings.push((
                image_ref.to_owned(),
                StoredRef {
                    research_id: rid.clone().unwrap_or_default(),
                    context_id: String::new(),
                    value: json!({"bindingError":safe_message(&e),"bindingCode":error_code(&e)}),
                },
            )),
        }
    }
    let rid = rid.ok_or_else(|| anyhow!("INVALID_REFERENCE: image references are unavailable"))?;
    locked_store(inner)?.ensure_research(&rid, cid)?;
    let mut results = vec![];
    let mut blocks = vec![];
    let mut evidence_values = vec![];
    let mut wire_bytes = 0usize;
    let mut image_content_observed = false;
    for (image_ref, binding) in bindings {
        if binding.value.get("bindingError").is_some() {
            results.push(image_error(
                &image_ref,
                binding.value["bindingCode"]
                    .as_str()
                    .unwrap_or("INVALID_REFERENCE"),
                binding.value["bindingError"]
                    .as_str()
                    .unwrap_or("Reference unavailable."),
            ));
            continue;
        }
        if binding.context_id != cid {
            results.push(image_error(
                &image_ref,
                "CONTEXT_CHANGED",
                "The image belongs to another context.",
            ));
            continue;
        }
        let url = required_text(&binding.value, "url")?.to_owned();
        let token = cancel.child_token();
        let fetch_token = token.clone();
        let fetch = images::fetch_image(&url, &fetch_token);
        tokio::pin!(fetch);
        let fetched = tokio::select! {
            result = &mut fetch => Ok(result),
            _ = tokio::time::sleep_until(deadline) => {
                token.cancel();
                let _ = fetch.await;
                Err(())
            }
        };
        match fetched {
            Ok(Ok(image)) => {
                let approx = image.data.len();
                if wire_bytes + approx > MAX_IMAGE_WIRE_BYTES {
                    results.push(image_error(
                        &image_ref,
                        "RESULT_TOO_LARGE",
                        "The image batch exceeds the 8 MiB wire limit.",
                    ));
                    continue;
                }
                wire_bytes += approx;
                image_content_observed = true;
                let ev_id = format!("evidence_{}", Uuid::new_v4());
                let observed = &image.retrieved_at;
                let mut ev = evidence::observed_evidence(
                    binding.value.get("sourceUrl").and_then(Value::as_str),
                    cid,
                    binding.value.get("sku").and_then(Value::as_str),
                    &[binding
                        .value
                        .get("fieldPath")
                        .and_then(Value::as_str)
                        .unwrap_or("")],
                    json!([
                        {"fieldPath":"/sha256","value":image.sha256},
                        {"fieldPath":"/mimeType","value":image.mime_type},
                        {"fieldPath":"/width","value":image.width},
                        {"fieldPath":"/height","value":image.height},
                        {"fieldPath":"/retrievedAt","value":image.retrieved_at},
                        {"fieldPath":"/sourceKind","value":binding.value["sourceKind"]},
                        {"fieldPath":"/sourceRef","value":binding.value["sourceRef"]}
                    ]),
                    observed,
                );
                ev["evidenceRef"] = json!(ev_id);
                evidence_values.push(ev);
                blocks.push(ImagePayload {
                    data: image.data,
                    mime_type: image.mime_type.clone(),
                });
                results.push(json!({"imageRef":image_ref,"status":"ok","sourceKind":binding.value["sourceKind"],"sourceRef":binding.value["sourceRef"],"evidenceRefs":[ev_id],"mimeType":image.mime_type,"width":image.width,"height":image.height,"sha256":image.sha256,"retrievedAt":image.retrieved_at,"contentIndex":blocks.len()}))
            }
            Ok(Err(e)) => results.push(image_error(&image_ref, error_code(&e), &safe_message(&e))),
            Err(()) => results.push(image_error(
                &image_ref,
                "UPSTREAM_TIMEOUT",
                "Image retrieval exceeded the request deadline.",
            )),
        }
        tokio::task::yield_now().await;
    }
    if let Err(error) =
        confirm_context_after_images(inner, cancel, deadline, cid, image_content_observed).await
    {
        if error_code(&error) == "CANCELLED" {
            record_cancelled(inner, &rid)?;
        }
        return Err(error);
    }
    let n = Normalized {
        data: json!({"results":results}),
        evidence: evidence_values,
        warnings: vec![],
        product_refs: vec![],
    };
    record(inner, &rid, "images", "Fetched referenced images", &n)?;
    if cancel.is_cancelled() {
        record_cancelled(inner, &rid)?;
        return Err(anyhow!("CANCELLED: request cancelled"));
    }
    let observed = evidence::now();
    Ok((
        envelope(
            Some(&rid),
            n.data,
            &context,
            n.evidence,
            n.warnings,
            &observed,
        ),
        blocks,
    ))
}

fn journal(inner: &Inner, name: &str, args: &Value) -> Result<(Value, Vec<ImagePayload>)> {
    let mut store = locked_store(inner)?;
    let (rid, data, context, warnings) = match name {
        "ozon_list_research" => (
            None,
            store.list(args)?,
            cached_context(&store)?,
            vec![marketplace::warning(
                "HISTORICAL_CONTEXT",
                "Listed research may belong to a different captured context.",
                &[],
            )],
        ),
        "ozon_get_research" => {
            let rid = required_text(args, "researchId")?;
            store.lease(rid)?;
            (
                Some(rid.to_owned()),
                store.read(args)?,
                cached_context(&store)?,
                vec![marketplace::warning(
                    "HISTORICAL_CONTEXT",
                    "Journal data retains its original captured context.",
                    &[],
                )],
            )
        }
        "ozon_append_research_note" => {
            let rid = required_text(args, "researchId")?;
            let context = store.research_context(rid)?;
            let data = store.append_note(args)?;
            (Some(rid.to_owned()), data, context, vec![])
        }
        _ => return Err(anyhow!("INVALID_ARGUMENT: unknown journal method")),
    };
    let observed = evidence::now();
    Ok((
        envelope(rid.as_deref(), data, &context, vec![], warnings, &observed),
        vec![],
    ))
}

async fn observe_context(
    inner: &Inner,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value> {
    let raw = gateway_call(inner, cancel, deadline, |gateway, token| {
        Box::pin(gateway.context(token))
    })
    .await?;
    bind_context(inner, &raw)
}
async fn confirm_context(
    inner: &Inner,
    cancel: &CancellationToken,
    deadline: Instant,
    expected: &str,
) -> Result<()> {
    let current = observe_context(inner, cancel, deadline).await?;
    if required_text(&current, "contextId")? != expected {
        return Err(anyhow!(
            "CONTEXT_CHANGED: marketplace context changed during the operation"
        ));
    }
    Ok(())
}
async fn confirm_context_after_images(
    inner: &Inner,
    cancel: &CancellationToken,
    deadline: Instant,
    expected: &str,
    image_content_observed: bool,
) -> Result<()> {
    let raw = gateway_call(inner, cancel, deadline, move |gateway, token| {
        Box::pin(async move {
            if image_content_observed {
                gateway.mark_image_content_available();
            }
            gateway.context(token).await
        })
    })
    .await?;
    let current = bind_context(inner, &raw)?;
    if required_text(&current, "contextId")? != expected {
        return Err(anyhow!(
            "CONTEXT_CHANGED: marketplace context changed during the operation"
        ));
    }
    Ok(())
}
fn bind_context(inner: &Inner, raw: &Value) -> Result<Value> {
    let signature = raw
        .get("signature")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "regionLabel": raw.get("regionLabel").cloned().unwrap_or(Value::Null),
                "regionVerification": raw.get("regionVerification").cloned().unwrap_or(Value::Null),
                "accountState": raw.get("accountState").cloned().unwrap_or(Value::Null)
            })
        });
    let mut store = locked_store(inner)?;
    let previous = store.meta("current_context")?;
    let id = previous
        .as_ref()
        .filter(|v| v.get("signature") == Some(&signature))
        .and_then(|v| v.get("contextId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("context_{}", Uuid::new_v4()));
    let mut context =
        evidence::public_context(&with_context_id(raw.clone(), &json!({"contextId":id})));
    context["contextId"] = json!(id);
    store.set_meta(
        "current_context",
        &json!({"signature":signature,"contextId":context["contextId"],"context":context}),
    )?;
    Ok(context)
}
fn cached_context(store: &Store) -> Result<Value> {
    Ok(store.meta("current_context")?.and_then(|v|v.get("context").cloned()).unwrap_or_else(||json!({"contextId":"context_unobserved","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"unknown"})))
}
fn with_context_id(mut raw: Value, context: &Value) -> Value {
    raw["contextId"] = context
        .get("contextId")
        .cloned()
        .unwrap_or_else(|| json!("context-unknown"));
    raw
}

type GatewayFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>>;
async fn gateway_call<F>(
    inner: &Inner,
    cancel: &CancellationToken,
    deadline: Instant,
    f: F,
) -> Result<Value>
where
    F: for<'a> FnOnce(&'a mut GatewayBackend, &'a CancellationToken) -> GatewayFuture<'a>,
{
    if cancel.is_cancelled() {
        return Err(anyhow!("CANCELLED: request cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(anyhow!(
            "UPSTREAM_TIMEOUT: browser queue budget expired before the operation started"
        ));
    }
    let mut gateway_guard = tokio::select! {
        guard = inner.gateway.lock() => guard,
        _ = cancel.cancelled() => return Err(anyhow!("CANCELLED: request cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Err(anyhow!("UPSTREAM_TIMEOUT: browser queue budget expired before the operation started")),
    };
    if cancel.is_cancelled() {
        return Err(anyhow!("CANCELLED: request cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(anyhow!(
            "UPSTREAM_TIMEOUT: browser queue budget expired before the operation started"
        ));
    }
    if gateway_guard.is_none() {
        *gateway_guard = Some(GatewayBackend::Real(Gateway::from_env().await?));
    }
    let gateway = gateway_guard
        .as_mut()
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: gateway initialization failed"))?;
    let operation_cancel = cancel.child_token();
    let operation_token = operation_cancel.clone();
    let result = {
        let operation = f(gateway, &operation_token);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => result,
            _ = cancel.cancelled() => {
                operation_cancel.cancel();
                let _ = operation.await;
                Err(anyhow!("CANCELLED: request cancelled"))
            }
            _ = tokio::time::sleep_until(deadline) => {
                operation_cancel.cancel();
                let _ = operation.await;
                Err(anyhow!("UPSTREAM_TIMEOUT: active browser operation deadline expired"))
            }
        }
    };
    drop(gateway_guard);
    result
}

fn record(inner: &Inner, rid: &str, kind: &str, summary: &str, n: &Normalized) -> Result<()> {
    locked_store(inner)?.record(rid, kind, summary, &n.product_refs, &n.evidence)
}
fn oldest_observation(evidence_values: &[Value]) -> String {
    evidence_values
        .iter()
        .filter_map(|value| value.get("observedAt").and_then(Value::as_str))
        .min()
        .map(str::to_owned)
        .unwrap_or_else(evidence::now)
}
fn record_cancelled(inner: &Inner, rid: &str) -> Result<()> {
    locked_store(inner)?.record(rid, "cancelled", "Request cancelled", &[], &[])
}
fn locked_store(inner: &Inner) -> Result<std::sync::MutexGuard<'_, Store>> {
    inner
        .store
        .lock()
        .map_err(|_| anyhow!("SOURCE_CHANGED: store lock poisoned"))
}
fn success(
    name: &str,
    value: Value,
    images: Vec<ImagePayload>,
    research: Option<&str>,
) -> ToolReply {
    if evidence::utf16_len(&value).map_or(true, |n| n > MAX_JSON_UTF16) {
        return contracts::failure(
            "RESULT_TOO_LARGE",
            "The structured result exceeds 60000 UTF-16 units.",
            research,
        );
    }
    if let Err(e) = contracts::validate_output(name, &value) {
        return contracts::failure(
            "SOURCE_CHANGED",
            &format!("Generated result failed its contract: {}", safe_message(&e)),
            research,
        );
    }
    ToolReply {
        structured: Some(value),
        error: false,
        text: None,
        images,
    }
}
fn make_search_args(
    start: &Value,
    args: &Value,
    query: Option<&str>,
    url: Option<&str>,
    cursor: Option<&str>,
) -> SearchArgs {
    let price = start.get("priceRange");
    SearchArgs {
        query: query.map(str::to_owned),
        search_url: url.map(str::to_owned),
        next_cursor: cursor.map(str::to_owned),
        include_facets: Some(
            args.get("includeFacets")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        ),
        sort: None,
        price_min: price
            .and_then(|p| p.get("minMinor"))
            .and_then(Value::as_u64),
        price_max: price
            .and_then(|p| p.get("maxMinor"))
            .and_then(Value::as_u64),
        limit: args.get("limit").and_then(Value::as_u64).unwrap_or(12) as usize,
    }
}
struct ResolvedProduct {
    target: String,
    presentation: Option<Value>,
    research_id: Option<String>,
    include: Option<Vec<String>>,
    cached_raw: Option<Value>,
    context_id: Option<String>,
}
fn resolve_product(inner: &Inner, selector: &Value) -> Result<ResolvedProduct> {
    if let Some(reference) = selector.get("productRef").and_then(Value::as_str) {
        let r = locked_store(inner)?.get_ref(reference, "product")?;
        return Ok(ResolvedProduct {
            presentation: None,
            target: r
                .value
                .get("url")
                .or_else(|| r.value.get("sku"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("INVALID_REFERENCE: malformed product reference"))?
                .into(),
            research_id: Some(r.research_id),
            include: None,
            cached_raw: None,
            context_id: Some(r.context_id),
        });
    }
    if let Some(cursor) = selector.get("cursor").and_then(Value::as_str) {
        let r = locked_store(inner)?.get_ref(cursor, "product_cursor")?;
        return Ok(ResolvedProduct {
            presentation: Some(
                r.value
                    .get("presentation")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({"view":"full"})),
            ),
            target: r
                .value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            research_id: Some(r.research_id),
            include: r.value.get("include").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
            cached_raw: r.value.get("cachedRaw").cloned(),
            context_id: Some(r.context_id),
        });
    }
    if let Some(sku) = selector.get("sku").and_then(Value::as_str) {
        return Ok(ResolvedProduct {
            presentation: None,
            target: sku.into(),
            research_id: None,
            include: None,
            cached_raw: None,
            context_id: None,
        });
    }
    let url = required_text(selector, "url")?;
    validate_product_url(url)?;
    Ok(ResolvedProduct {
        presentation: None,
        target: url.into(),
        research_id: None,
        include: None,
        cached_raw: None,
        context_id: None,
    })
}
fn verify_bound(stored: &StoredRef, args: &Value, context_id: &str) -> Result<()> {
    if stored.context_id != context_id {
        return Err(anyhow!(
            "CONTEXT_CHANGED: reference belongs to another context"
        ));
    }
    if let Some(rid) = research_arg(args)
        && rid != stored.research_id
    {
        return Err(anyhow!(
            "INVALID_REFERENCE: reference belongs to another research"
        ));
    }
    Ok(())
}
fn validate_product_url(input: &str) -> Result<()> {
    let url = Url::parse(input).map_err(|_| anyhow!("INVALID_ARGUMENT: invalid product URL"))?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !url.path().starts_with("/product/")
    {
        return Err(anyhow!("INVALID_ARGUMENT: expected an Ozon product URL"));
    }
    Ok(())
}
fn image_error(reference: &str, code: &str, message: &str) -> Value {
    let allowed = match code {
        "INVALID_REFERENCE" | "CONTEXT_CHANGED" | "RESEARCH_EXPIRED" | "SOURCE_BLOCKED"
        | "SOURCE_CHANGED" | "UPSTREAM_TIMEOUT" | "SERVER_BUSY" | "NOT_FOUND"
        | "RESULT_TOO_LARGE" => code,
        _ => "SOURCE_CHANGED",
    };
    json!({"imageRef":reference,"status":"error","error":{"code":allowed,"message":message,"retryable":matches!(allowed,"SOURCE_BLOCKED"|"UPSTREAM_TIMEOUT"|"SERVER_BUSY")}})
}
fn dedup_warnings(values: Vec<Value>) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|v| {
            seen.insert(
                v.get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .take(30)
        .collect()
}
fn price_range_inverted(args: &Value) -> bool {
    args.pointer("/start/priceRange").is_some_and(|p| {
        match (
            p.get("minMinor").and_then(Value::as_u64),
            p.get("maxMinor").and_then(Value::as_u64),
        ) {
            (Some(a), Some(b)) => a > b,
            _ => false,
        }
    })
}
fn required_text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: missing {key}"))
}
fn research_arg(args: &Value) -> Option<&str> {
    args.get("researchId").and_then(Value::as_str)
}
fn research_arg_fallback(_name: &str) -> Option<&str> {
    None
}

#[derive(Debug)]
struct LoadedReviewSeen {
    keys: Vec<String>,
    depth: usize,
    nodes_read: usize,
}

fn load_review_seen(
    store: &Store,
    seen_ref: Option<&str>,
    research_id: &str,
    context_id: &str,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<LoadedReviewSeen> {
    let mut current = seen_ref.map(str::to_owned);
    let mut visited = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut expected_count = None;
    let mut expected_depth = None;
    let mut head_depth = 0usize;
    let mut nodes_read = 0usize;
    while let Some(reference) = current {
        if cancel.is_cancelled() {
            return Err(anyhow!("CANCELLED: request cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "UPSTREAM_TIMEOUT: review coverage deadline expired"
            ));
        }
        if !visited.insert(reference.clone())
            || visited.len() > marketplace::REVIEW_SEEN_CHECKPOINT_INTERVAL
        {
            return Err(anyhow!("SOURCE_CHANGED: invalid review coverage chain"));
        }
        let node = store
            .get_ref(&reference, "review_seen")
            .map_err(|_| anyhow!("SOURCE_CHANGED: review coverage state is unavailable"))?;
        nodes_read += 1;
        if node.research_id != research_id || node.context_id != context_id {
            return Err(anyhow!("SOURCE_CHANGED: invalid review coverage binding"));
        }
        let count = node
            .value
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid review coverage count"))?;
        expected_count.get_or_insert(count);
        let depth = node
            .value
            .get("depth")
            .and_then(Value::as_u64)
            .and_then(|depth| usize::try_from(depth).ok())
            .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid review coverage depth"))?;
        if nodes_read == 1 {
            head_depth = depth;
        }
        if expected_depth.is_some_and(|expected| expected != depth) {
            return Err(anyhow!("SOURCE_CHANGED: invalid review coverage depth"));
        }
        let checkpoint = node.value.get("kind").and_then(Value::as_str) == Some("checkpoint");
        let node_keys = node
            .value
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid review coverage state"))?;
        if node_keys.is_empty()
            || node_keys.len()
                > if checkpoint {
                    marketplace::MAX_SEEN_REVIEWS
                } else {
                    30
                }
            || checkpoint
                && serde_json::to_vec(&node.value)?.len()
                    > marketplace::MAX_REVIEW_SEEN_CHECKPOINT_BYTES
        {
            return Err(anyhow!("SOURCE_CHANGED: invalid review coverage chunk"));
        }
        for key in node_keys {
            let key = key
                .as_str()
                .filter(|key| valid_review_identity(key))
                .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid review coverage identity"))?;
            keys.insert(key.to_owned());
        }
        if keys.len() > marketplace::MAX_SEEN_REVIEWS {
            return Err(anyhow!("SOURCE_CHANGED: review coverage exceeds its bound"));
        }
        if checkpoint {
            if depth != 0
                || node
                    .value
                    .get("parent")
                    .is_some_and(|value| !value.is_null())
            {
                return Err(anyhow!(
                    "SOURCE_CHANGED: invalid review coverage checkpoint"
                ));
            }
            current = None;
        } else if node.value.get("kind").and_then(Value::as_str) == Some("delta")
            && (1..marketplace::REVIEW_SEEN_CHECKPOINT_INTERVAL).contains(&depth)
        {
            current = node
                .value
                .get("parent")
                .and_then(Value::as_str)
                .map(str::to_owned);
            expected_depth = Some(depth - 1);
            if current.is_none() && depth != 1 {
                return Err(anyhow!("SOURCE_CHANGED: truncated review coverage chain"));
            }
        } else {
            return Err(anyhow!("SOURCE_CHANGED: invalid review coverage node"));
        }
    }
    if expected_count != Some(keys.len() as u64) && expected_count.is_some() {
        return Err(anyhow!(
            "SOURCE_CHANGED: inconsistent review coverage state"
        ));
    }
    Ok(LoadedReviewSeen {
        keys: keys.into_iter().collect(),
        depth: head_depth,
        nodes_read,
    })
}

fn valid_review_identity(key: &str) -> bool {
    if let Some(hash) = key.strip_prefix("id:") {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    } else if let Some(id) = key.strip_prefix("anonymous:") {
        Uuid::parse_str(id).is_ok()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_context(signature: &str) -> Value {
        json!({
            "sourceUrl":"https://www.ozon.ru/", "regionLabel":"Москва",
            "regionVerification":"verified", "accountState":"anonymous",
            "accessState":"available", "signature":signature, "capabilities":{}
        })
    }

    async fn scripted_service(
        contexts: Vec<Value>,
        searches: Vec<FakeAction>,
        products: Vec<FakeAction>,
    ) -> (tempfile::TempDir, Service) {
        let temp = tempfile::Builder::new()
            .prefix("service-scripted-")
            .tempdir()
            .unwrap();
        std::fs::set_permissions(
            temp.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let service = Service::new(&Config::at(temp.path().to_path_buf()).unwrap())
            .await
            .unwrap();
        *service.inner.gateway.lock().await = Some(GatewayBackend::Fake(FakeGateway {
            contexts: contexts.into(),
            context_delay: Duration::ZERO,
            searches: searches.into(),
            products: products.into(),
            reviews: std::collections::VecDeque::new(),
            image_content_observed: false,
            context_signature: None,
        }));
        (temp, service)
    }

    fn product_fixture(sku: &str) -> Value {
        json!({"sku":sku,"name":format!("Product {sku}"),"url":format!("https://www.ozon.ru/product/item-{sku}/"),"cardPrice":null,"priceRegular":100.0,"available":true,"rating":null,"reviews":null,"seller":null,"images":[],"characteristics":{},"description":{"text":"","images":[]},"variants":{"status":"available","items":[],"hasNext":false,"nextPath":null},"offers":{"status":"unsupported","items":[],"hasNext":null,"nextPath":null},"warnings":[]})
    }

    fn failure_code(reply: &ToolReply) -> String {
        serde_json::from_str::<Value>(reply.text.as_deref().unwrap()).unwrap()["error"]["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    #[test]
    fn rejects_non_product_routes() {
        assert!(validate_product_url("https://www.ozon.ru/search/?text=x").is_err());
        assert!(validate_product_url("https://www.ozon.ru/product/name-123/").is_ok());
    }
    #[test]
    fn range_order_is_semantic() {
        assert!(price_range_inverted(
            &json!({"start":{"query":"x","priceRange":{"minMinor":2,"maxMinor":1}}})
        ));
    }

    #[tokio::test]
    async fn journal_tools_work_without_starting_a_browser() {
        let temp = tempfile::Builder::new()
            .prefix("service-journal-")
            .tempdir()
            .unwrap();
        std::fs::set_permissions(
            temp.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let config = Config::at(temp.path().to_path_buf()).unwrap();
        let context = json!({"contextId":"ctx","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"unknown"});
        let research_id = {
            let mut store = Store::open(&config.root).unwrap();
            store.create_research("Offline research", &context).unwrap()
        };
        let service = Service::new(&config).await.unwrap();
        let token = CancellationToken::new();
        let held = service
            .inner
            .admission
            .clone()
            .acquire_many_owned(8)
            .await
            .unwrap();
        let busy = service
            .call("ozon_list_research", json!({}), token.clone())
            .await;
        assert!(busy.error);
        assert_eq!(
            serde_json::from_str::<Value>(busy.text.as_deref().unwrap()).unwrap()["error"]["code"],
            "SERVER_BUSY"
        );
        drop(held);
        let list = service
            .call("ozon_list_research", json!({}), token.clone())
            .await;
        assert!(!list.error);
        contracts::validate_output("ozon_list_research", list.structured.as_ref().unwrap())
            .unwrap();
        let note = service
            .call(
                "ozon_append_research_note",
                json!({"researchId":research_id,"operationId":"offline-op","kind":"requirements","text":"No browser is required."}),
                token.clone(),
            )
            .await;
        assert!(!note.error);
        let read = service
            .call(
                "ozon_get_research",
                json!({"researchId":research_id,"section":"notes"}),
                token,
            )
            .await;
        assert_eq!(
            read.structured.as_ref().unwrap()["data"]["payload"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn compact_search_delta_and_snapshot_expansion_preserve_observations() {
        let context = fake_context("same");
        let first_raw = json!({"searchUrl":"https://www.ozon.ru/search/?text=x","items":[{"sku":"1","name":"Server","price":100.0,"priceType":"unknown","currency":"RUB","seller":"First seller","url":"https://www.ozon.ru/product/item-1/"}],"hasNext":false});
        let mut changed_raw = first_raw.clone();
        changed_raw["items"][0]["price"] = json!(120.0);
        changed_raw["items"][0]["seller"] = json!("Second seller");
        let (_temp, service) = scripted_service(
            vec![context; 4],
            vec![FakeAction::Value(first_raw), FakeAction::Value(changed_raw)],
            vec![],
        )
        .await;
        let first = service
            .call(
                "ozon_search",
                json!({"start":{"query":"server"}}),
                CancellationToken::new(),
            )
            .await;
        assert!(!first.error, "{:?}", first.text);
        let first = first.structured.unwrap();
        assert_eq!(first["view"], "compact");
        assert_eq!(first["data"]["items"][0]["novelty"]["status"], "new");
        assert_eq!(first["evidence"], json!([]));
        let rid = first["researchId"].as_str().unwrap();
        let baseline = first["data"]["items"][0]["productRef"].clone();
        let second = service
            .call(
                "ozon_search",
                json!({"researchId":rid,"start":{"query":"other search"},"repeatMode":"delta"}),
                CancellationToken::new(),
            )
            .await;
        assert!(!second.error, "{:?}", second.text);
        let second = second.structured.unwrap();
        let row = &second["data"]["items"][0];
        assert_eq!(row["representation"], "delta");
        assert_eq!(row["baselineProductRef"], baseline);
        assert_eq!(row["prices"][0]["amountMinor"], 12000);
        assert_eq!(row["prices"][0]["type"], "unknown");
        assert_eq!(row["seller"]["name"], "Second seller");
        let snapshot = service
            .call(
                "ozon_get_research",
                json!({"researchId":rid,"section":"candidates","productRefs":[baseline]}),
                CancellationToken::new(),
            )
            .await;
        assert!(!snapshot.error, "{:?}", snapshot.text);
        let snapshot = snapshot.structured.unwrap();
        assert_eq!(
            snapshot["data"]["payload"][0]["prices"][0]["amountMinor"],
            10000
        );
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn search_cursors_bind_view_and_local_refinements_need_no_gateway() {
        let context = fake_context("same");
        let raw = json!({"searchUrl":"https://www.ozon.ru/search/?text=x","items":[{"sku":"1","name":"Server","price":1.0,"url":"https://www.ozon.ru/product/item-1/"}],"hasNext":true,"nextCursor":"source-cursor", "facets":{"items":[{"title":"Type","options":[{"label":"A","searchUrl":"https://www.ozon.ru/category/a-1/"},{"label":"B","searchUrl":"https://www.ozon.ru/category/b-2/"}]}]}});
        let (_temp, service) =
            scripted_service(vec![context; 2], vec![FakeAction::Value(raw)], vec![]).await;
        let first = service.call("ozon_search", json!({"start":{"query":"x"},"view":"comparison","includeFacets":true,"refinementLimit":1}), CancellationToken::new()).await;
        assert!(!first.error, "{:?}", first.text);
        let first = first.structured.unwrap();
        let cursor = first["data"]["nextCursor"].as_str().unwrap();
        let invalid = service
            .call(
                "ozon_search",
                json!({"start":{"cursor":cursor},"view":"full"}),
                CancellationToken::new(),
            )
            .await;
        assert!(invalid.error);
        assert_eq!(failure_code(&invalid), "INVALID_ARGUMENT");
        let metadata_cursor = first["data"]["refinementsNextCursor"].as_str().unwrap();
        let more = service
            .call(
                "ozon_search",
                json!({"start":{"refinementsCursor":metadata_cursor}}),
                CancellationToken::new(),
            )
            .await;
        assert!(!more.error, "{:?}", more.text);
        let more = more.structured.unwrap();
        assert_eq!(more["view"], "comparison");
        assert_eq!(more["observedAt"], first["observedAt"]);
        assert_eq!(more["data"]["items"], json!([]));
        assert_eq!(more["data"]["refinements"].as_array().unwrap().len(), 1);
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_product_cursors_cannot_change_valid_sibling_view() {
        let source = fake_context("same");
        let (_temp, service) = scripted_service(
            vec![source.clone(); 2],
            vec![],
            vec![FakeAction::Value(product_fixture("1"))],
        )
        .await;
        let context = bind_context(&service.inner, &source).unwrap();
        let (rid, cursors) = {
            let mut store = locked_store(&service.inner).unwrap();
            let rid = store.create_research("valid", &context).unwrap();
            let foreign = store.create_research("foreign", &context).unwrap();
            let cursors = ["full", "comparison"]
                .iter()
                .map(|view| {
                    store
                        .put_ref(
                            &foreign,
                            "product_cursor",
                            &json!({"cachedRaw":product_fixture("2"),"presentation":{"view":view}}),
                            Some(1800),
                        )
                        .unwrap()
                })
                .collect::<Vec<_>>();
            (rid, cursors)
        };
        let reply = service.call("ozon_get_products", json!({"researchId":rid,"products":[{"cursor":cursors[0]},{"cursor":cursors[1]},{"sku":"1"}]}), CancellationToken::new()).await;
        assert!(!reply.error, "{:?}", reply.text);
        let value = reply.structured.unwrap();
        assert_eq!(value["view"], "compact");
        assert_eq!(
            value["data"]["results"][0]["error"]["code"],
            "INVALID_REFERENCE"
        );
        assert_eq!(
            value["data"]["results"][1]["error"]["code"],
            "INVALID_REFERENCE"
        );
        assert_eq!(value["data"]["results"][2]["status"], "ok");
        assert!(value["data"]["results"][2]["product"].get("url").is_none());
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_refinement_admission_does_not_advance_candidate_baseline() {
        let context = fake_context("same");
        let raw = json!({"searchUrl":"https://www.ozon.ru/search/?text=x","items":[{"sku":"1","name":"Server","price":1.0,"url":"https://www.ozon.ru/product/item-1/"}],"hasNext":false});
        let mut too_large = raw.clone();
        too_large["items"][0]["price"] = json!(2.0);
        too_large["facets"] = json!({"items":[{"title":"Huge","options":[{"label":"x".repeat(9000),"searchUrl":"https://www.ozon.ru/category/x-1/"}]}]});
        let (_temp, service) = scripted_service(
            vec![context; 6],
            vec![
                FakeAction::Value(raw.clone()),
                FakeAction::Value(too_large),
                FakeAction::Value(raw),
            ],
            vec![],
        )
        .await;
        let first = service
            .call(
                "ozon_search",
                json!({"start":{"query":"x"}}),
                CancellationToken::new(),
            )
            .await
            .structured
            .unwrap();
        let rid = first["researchId"].as_str().unwrap();
        let failed = service
            .call(
                "ozon_search",
                json!({"researchId":rid,"start":{"query":"x"},"includeFacets":true}),
                CancellationToken::new(),
            )
            .await;
        assert!(failed.error);
        assert_eq!(failure_code(&failed), "RESULT_TOO_LARGE");
        let last = service
            .call(
                "ozon_search",
                json!({"researchId":rid,"start":{"query":"x"}}),
                CancellationToken::new(),
            )
            .await;
        assert!(!last.error, "{:?}", last.text);
        let last = last.structured.unwrap();
        assert_eq!(
            last["data"]["items"][0]["novelty"]["previousProductRef"],
            first["data"]["items"][0]["productRef"]
        );
        assert_eq!(last["data"]["items"][0]["novelty"]["status"], "unchanged");
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scripted_search_persists_recoverable_evidence_through_service() {
        let context = fake_context("same");
        let raw = json!({"searchUrl":"https://www.ozon.ru/search/?text=x","items":[{"sku":"1","name":"x","price":1.5,"priceType":"unknown","currency":"RUB","url":"https://www.ozon.ru/product/item-1/"}],"hasNext":false});
        let (_temp, service) = scripted_service(
            vec![context.clone(), context],
            vec![FakeAction::Value(raw)],
            vec![],
        )
        .await;
        let reply = service
            .call(
                "ozon_search",
                json!({"start":{"query":"x"},"includeFacets":false}),
                CancellationToken::new(),
            )
            .await;
        assert!(!reply.error, "{}", reply.text.unwrap_or_default());
        let value = reply.structured.unwrap();
        contracts::validate_output("ozon_search", &value).unwrap();
        let rid = value["researchId"].as_str().unwrap();
        let stored = locked_store(&service.inner)
            .unwrap()
            .read(&json!({"researchId":rid,"section":"evidence"}))
            .unwrap();
        assert!(!stored["payload"][0]["facts"].as_array().unwrap().is_empty());
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn search_region_warning_matches_authoritative_response_context() {
        let cases = [
            (
                json!({"sourceUrl":"https://www.ozon.ru/","regionLabel":"Москва","regionVerification":"verified","accountState":"anonymous","accessState":"available","signature":"verified","capabilities":{}}),
                json!({"regionVerified":false}),
                false,
            ),
            (
                json!({"sourceUrl":"https://www.ozon.ru/","regionLabel":"Москва","regionVerification":"unverified","accountState":"anonymous","accessState":"available","signature":"unverified","capabilities":{}}),
                json!({"regionVerified":true}),
                true,
            ),
            (
                json!({"sourceUrl":"https://www.ozon.ru/","regionLabel":"Москва","accountState":"anonymous","accessState":"available","signature":"label-only","capabilities":{}}),
                json!({"regionVerified":true}),
                true,
            ),
        ];

        for (context, raw_context, expect_warning) in cases {
            let raw = json!({
                "searchUrl":"https://www.ozon.ru/search/?text=x",
                "items":[],
                "hasNext":false,
                "context":raw_context
            });
            let (_temp, service) = scripted_service(
                vec![context.clone(), context],
                vec![FakeAction::Value(raw)],
                vec![],
            )
            .await;
            let reply = service
                .call(
                    "ozon_search",
                    json!({"start":{"query":"x"},"includeFacets":false}),
                    CancellationToken::new(),
                )
                .await;
            assert!(!reply.error, "{}", reply.text.unwrap_or_default());
            let value = reply.structured.unwrap();
            let has_warning = value["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "REGION_UNVERIFIED");
            assert_eq!(has_warning, expect_warning, "context={}", value["context"]);
            service.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn successful_image_observation_updates_context_capability() {
        let source_context = fake_context("same");
        let (_temp, service) = scripted_service(
            vec![
                source_context.clone(),
                source_context.clone(),
                fake_context("changed"),
            ],
            vec![],
            vec![],
        )
        .await;
        let bound_context = bind_context(&service.inner, &source_context).unwrap();
        confirm_context_after_images(
            &service.inner,
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
            bound_context["contextId"].as_str().unwrap(),
            true,
        )
        .await
        .unwrap();

        let reply = service
            .call("ozon_get_context", json!({}), CancellationToken::new())
            .await;
        assert!(!reply.error, "{}", reply.text.unwrap_or_default());
        let capabilities = reply.structured.unwrap()["data"]["capabilities"]
            .as_array()
            .unwrap()
            .clone();
        assert!(capabilities.iter().any(|capability| {
            capability["name"] == "image_content" && capability["status"] == "available"
        }));

        let changed = service
            .call("ozon_get_context", json!({}), CancellationToken::new())
            .await;
        assert!(!changed.error, "{}", changed.text.unwrap_or_default());
        let capabilities = changed.structured.unwrap()["data"]["capabilities"]
            .as_array()
            .unwrap()
            .clone();
        assert!(capabilities.iter().any(|capability| {
            capability["name"] == "image_content" && capability["status"] == "unverified"
        }));
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scripted_context_change_rejects_search_before_recording_success() {
        let raw =
            json!({"searchUrl":"https://www.ozon.ru/search/?text=x","items":[],"hasNext":false});
        let (_temp, service) = scripted_service(
            vec![fake_context("before"), fake_context("after")],
            vec![FakeAction::Value(raw)],
            vec![],
        )
        .await;
        let reply = service
            .call(
                "ozon_search",
                json!({"start":{"query":"x"}}),
                CancellationToken::new(),
            )
            .await;
        assert!(reply.error);
        assert_eq!(failure_code(&reply), "CONTEXT_CHANGED");
        let list = locked_store(&service.inner)
            .unwrap()
            .list(&json!({}))
            .unwrap();
        let rid = list["researches"][0]["researchId"].as_str().unwrap();
        let events = locked_store(&service.inner)
            .unwrap()
            .read(&json!({"researchId":rid,"section":"events"}))
            .unwrap();
        assert!(events["payload"].as_array().unwrap().is_empty());
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_records_terminal_event_after_gateway_cleanup() {
        let (_temp, service) = scripted_service(
            vec![fake_context("same")],
            vec![FakeAction::WaitForCancellation {
                cleanup: Duration::from_millis(5),
            }],
            vec![],
        )
        .await;
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let service = service.clone();
            let cancel = cancel.clone();
            async move {
                service
                    .call("ozon_search", json!({"start":{"query":"x"}}), cancel)
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
        let reply = task.await.unwrap();
        assert_eq!(failure_code(&reply), "CANCELLED");
        service.shutdown().await.unwrap();
        let list = locked_store(&service.inner)
            .unwrap()
            .list(&json!({}))
            .unwrap();
        let rid = list["researches"][0]["researchId"].as_str().unwrap();
        let events = locked_store(&service.inner)
            .unwrap()
            .read(&json!({"researchId":rid,"section":"events"}))
            .unwrap();
        assert_eq!(events["payload"][0]["kind"], "cancelled");
    }

    #[tokio::test]
    async fn batch_fails_live_items_when_cleanup_exhausts_confirmation_budget() {
        let context = fake_context("same");
        let (_temp, service) = scripted_service(
            vec![context.clone(), context],
            vec![],
            vec![
                FakeAction::Value(product_fixture("1")),
                FakeAction::WaitForCancellation {
                    cleanup: Duration::from_millis(20),
                },
            ],
        )
        .await;
        let reply = execute(
            service.inner.clone(),
            "ozon_get_products",
            json!({"products":[{"sku":"1"},{"sku":"2"}]}),
            CancellationToken::new(),
            Instant::now() + Duration::from_millis(10),
        )
        .await;
        assert!(!reply.error, "{}", reply.text.unwrap_or_default());
        let value = reply.structured.unwrap();
        assert_eq!(
            value["data"]["results"][0]["error"]["code"],
            "UPSTREAM_TIMEOUT"
        );
        assert_eq!(
            value["data"]["results"][1]["error"]["code"],
            "UPSTREAM_TIMEOUT"
        );
        contracts::validate_output("ozon_get_products", &value).unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn product_batch_uses_one_post_fetch_context_confirmation() {
        let context = fake_context("same");
        let products = (1..=8)
            .map(|sku| FakeAction::Value(product_fixture(&sku.to_string())))
            .collect();
        let (_temp, service) =
            scripted_service(vec![context.clone(), context], vec![], products).await;
        let reply = execute(
            service.inner.clone(),
            "ozon_get_products",
            json!({"products": (1..=8).map(|sku| json!({"sku":sku.to_string()})).collect::<Vec<_>>() }),
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .await;
        assert!(!reply.error, "{}", reply.text.unwrap_or_default());
        let value = reply.structured.unwrap();
        assert_eq!(
            value["data"]["results"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|result| result["status"] == "ok")
                .count(),
            8,
            "{value}"
        );
        match service.inner.gateway.lock().await.as_ref().unwrap() {
            GatewayBackend::Fake(gateway) => assert!(
                gateway.contexts.is_empty(),
                "the initial observation and one post-fetch confirmation must consume both scripted contexts"
            ),
            GatewayBackend::Real(_) => unreachable!(),
        }
        contracts::validate_output("ozon_get_products", &value).unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn product_batch_context_mismatch_fails_live_items_but_keeps_cached_items() {
        let context = fake_context("before");
        let (_temp, service) = scripted_service(
            vec![context.clone(), fake_context("after")],
            vec![],
            vec![FakeAction::Value(product_fixture("2"))],
        )
        .await;
        let bound_context = bind_context(&service.inner, &context).unwrap();
        let (research_id, cursor) = {
            let mut store = locked_store(&service.inner).unwrap();
            let research_id = store
                .create_research("cached and live products", &bound_context)
                .unwrap();
            let cursor = store
                .put_ref(
                    &research_id,
                    "product_cursor",
                    &json!({
                        "path":"/product/item-1/",
                        "include":["characteristics"],
                        "cachedRaw":product_fixture("1")
                    }),
                    Some(1800),
                )
                .unwrap();
            (research_id, cursor)
        };
        let reply = service
            .call(
                "ozon_get_products",
                json!({
                    "researchId":research_id,
                    "products":[{"cursor":cursor},{"sku":"2"}]
                }),
                CancellationToken::new(),
            )
            .await;
        assert!(!reply.error, "{}", reply.text.unwrap_or_default());
        let value = reply.structured.unwrap();
        assert_eq!(value["data"]["results"][0]["status"], "ok");
        assert_eq!(
            value["data"]["results"][1]["error"]["code"],
            "CONTEXT_CHANGED"
        );
        contracts::validate_output("ozon_get_products", &value).unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn product_batch_reports_invalid_reference_per_item_and_keeps_valid_sku() {
        let context = fake_context("same");
        let (_temp, service) = scripted_service(
            vec![context.clone(), context],
            vec![],
            vec![FakeAction::Value(product_fixture("2"))],
        )
        .await;
        let reply = service
            .call(
                "ozon_get_products",
                json!({"products":[{"productRef":"ref_missing"},{"sku":"2"}]}),
                CancellationToken::new(),
            )
            .await;
        assert!(!reply.error, "{}", reply.text.unwrap_or_default());
        let value = reply.structured.unwrap();
        assert_eq!(
            value["data"]["results"][0]["error"]["code"],
            "INVALID_REFERENCE"
        );
        assert_eq!(value["data"]["results"][1]["status"], "ok");
        contracts::validate_output("ozon_get_products", &value).unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_review_cursor_requires_restart_instead_of_resetting_coverage() {
        let source_context = fake_context("same");
        let (_temp, service) = scripted_service(vec![source_context.clone()], vec![], vec![]).await;
        let bound_context = bind_context(&service.inner, &source_context).unwrap();
        let (research_id, cursor) = {
            let mut store = locked_store(&service.inner).unwrap();
            let research_id = store
                .create_research("legacy reviews", &bound_context)
                .unwrap();
            let cursor = store
                .put_ref(
                    &research_id,
                    "review_cursor",
                    &json!({"path":"/product/item-123/reviews/?page=2","productRef":"ref_product","sku":"123","sourceUrl":"https://www.ozon.ru/product/item-123/reviews/"}),
                    Some(1800),
                )
                .unwrap();
            (research_id, cursor)
        };
        let reply = service
            .call(
                "ozon_get_reviews",
                json!({"start":{"cursor":cursor},"researchId":research_id,"limit":10}),
                CancellationToken::new(),
            )
            .await;
        assert!(reply.error);
        assert_eq!(failure_code(&reply), "INVALID_REFERENCE");
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn review_handler_drains_cached_source_page_before_upstream_continuation() {
        let source_context = fake_context("same");
        let (_temp, service) = scripted_service(
            vec![
                source_context.clone(),
                source_context.clone(),
                source_context.clone(),
                source_context.clone(),
                source_context.clone(),
                source_context.clone(),
                source_context.clone(),
            ],
            vec![],
            vec![],
        )
        .await;
        let bound_context = bind_context(&service.inner, &source_context).unwrap();
        let (rid, product_ref) = {
            let mut store = locked_store(&service.inner).unwrap();
            let rid = store.create_research("reviews", &bound_context).unwrap();
            let product_ref = store
                .put_ref(
                    &rid,
                    "product",
                    &json!({"sku":"123","url":"https://www.ozon.ru/product/item-123/"}),
                    None,
                )
                .unwrap();
            (rid, product_ref)
        };
        let source_reviews = (0..30)
            .map(|index| {
                let review_id = if index == 1 {
                    Value::Null
                } else if index == 3 {
                    json!("r0")
                } else {
                    json!(format!("r{index}"))
                };
                json!({"reviewId":review_id,"score":5.0,"comment":format!("review-{index}"),"pros":null,"cons":null,"date":null,"purchased":null,"variantLabel":null,"photos":[]})
            })
            .collect::<Vec<_>>();
        let raw = json!({"rating":5.0,"totalReviews":31,"reviews":source_reviews,"nextPath":"/product/item-123/reviews/?page=2","hasNext":true,"refinements":[],"aggregationScope":"specific_sku","warnings":[]});
        let upstream = json!({"rating":5.0,"totalReviews":31,"reviews":[
            {"reviewId":"r29","score":5.0,"comment":"upstream-duplicate","photos":[]},
            {"reviewId":"r30","score":5.0,"comment":"upstream-new","photos":[]}
        ],"nextPath":null,"hasNext":false,"refinements":[],"aggregationScope":"specific_sku","warnings":[]});
        {
            let mut gateway = service.inner.gateway.lock().await;
            let Some(GatewayBackend::Fake(gateway)) = gateway.as_mut() else {
                panic!("scripted gateway missing")
            };
            gateway.reviews.push_back(FakeAction::Value(raw));
            gateway.reviews.push_back(FakeAction::Value(upstream));
        }

        let mut start = json!({"productRef":product_ref});
        let mut emitted = vec![];
        let mut unique_seen = vec![];
        let mut observed_at = None;
        let mut first_cursor = None;
        for limit in [3, 5, 30, 30] {
            let reply = service
                .call(
                    "ozon_get_reviews",
                    json!({"start":start,"researchId":rid,"limit":limit}),
                    CancellationToken::new(),
                )
                .await;
            assert!(!reply.error, "{}", reply.text.unwrap_or_default());
            let value = reply.structured.unwrap();
            contracts::validate_output("ozon_get_reviews", &value).unwrap();
            if unique_seen.len() < 3 {
                assert_eq!(
                    observed_at.get_or_insert(value["observedAt"].clone()),
                    &value["observedAt"]
                );
            }
            emitted.extend(
                value["data"]["reviews"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|review| review["text"].as_str().unwrap().to_owned()),
            );
            unique_seen.push(value["data"]["coverage"]["uniqueSeen"].as_u64().unwrap());
            match value["data"]["nextCursor"].as_str() {
                Some(cursor) => {
                    if first_cursor.is_none() {
                        first_cursor = Some(cursor.to_owned());
                        let mismatch = service
                            .call(
                                "ozon_get_reviews",
                                json!({"start":{"cursor":cursor},"includeFacets":true}),
                                CancellationToken::new(),
                            )
                            .await;
                        assert!(mismatch.error);
                        assert_eq!(failure_code(&mismatch), "INVALID_ARGUMENT");
                        let inherited = presentation_args(
                            &service.inner,
                            "ozon_get_reviews",
                            json!({"start":{"cursor":cursor}}),
                        )
                        .unwrap();
                        assert_eq!(inherited["includeFacets"], false);
                    }
                    start = json!({"cursor":cursor});
                }
                None => break,
            }
        }
        assert_eq!(
            emitted,
            (0..30)
                .filter(|index| *index != 3)
                .map(|index| format!("review-{index}"))
                .chain(["upstream-new".into()])
                .collect::<Vec<_>>()
        );
        assert_eq!(unique_seen, vec![3, 8, 29, 30]);

        let replay = service
            .call(
                "ozon_get_reviews",
                json!({"start":{"cursor":first_cursor.unwrap()},"researchId":rid,"limit":5}),
                CancellationToken::new(),
            )
            .await;
        assert!(!replay.error, "{}", replay.text.unwrap_or_default());
        let replay = replay.structured.unwrap();
        assert_eq!(replay["data"]["coverage"]["uniqueSeen"], 8);
        assert_eq!(
            replay["data"]["reviews"]
                .as_array()
                .unwrap()
                .iter()
                .map(|review| review["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["review-4", "review-5", "review-6", "review-7", "review-8"]
        );
        service.shutdown().await.unwrap();
    }

    #[test]
    fn review_seen_checkpoint_bounds_loader_and_honors_cancellation() {
        let temp = tempfile::Builder::new()
            .prefix("service-review-seen-")
            .tempdir()
            .unwrap();
        std::fs::set_permissions(
            temp.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        let context = json!({"contextId":"c","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"unknown"});
        let research_id = store.create_research("late reviews", &context).unwrap();
        let checkpoint_keys = (0..100)
            .map(|index| format!("id:{index:064x}"))
            .collect::<Vec<_>>();
        let mut head = store
            .put_ref(
                &research_id,
                "review_seen",
                &json!({"kind":"checkpoint","keys":checkpoint_keys,"count":100,"depth":0}),
                None,
            )
            .unwrap();
        for offset in 0..255usize {
            let key = format!("id:{:064x}", offset + 100);
            head = store
                .put_ref(
                    &research_id,
                    "review_seen",
                    &json!({"kind":"delta","parent":head,"keys":[key],"count":101+offset,"depth":offset+1}),
                    None,
                )
                .unwrap();
        }

        let started = std::time::Instant::now();
        let loaded = load_review_seen(
            &store,
            Some(&head),
            &research_id,
            "c",
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(loaded.keys.len(), 355);
        assert_eq!(loaded.depth, 255);
        assert_eq!(loaded.nodes_read, 256);
        eprintln!(
            "review_seen_load nodes={} elapsed_ms={}",
            loaded.nodes_read,
            started.elapsed().as_millis()
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            error_code(
                &load_review_seen(
                    &store,
                    Some(&head),
                    &research_id,
                    "c",
                    &cancelled,
                    Instant::now() + Duration::from_secs(1),
                )
                .unwrap_err()
            ),
            "CANCELLED"
        );
        assert_eq!(
            error_code(
                &load_review_seen(
                    &store,
                    Some(&head),
                    &research_id,
                    "c",
                    &CancellationToken::new(),
                    Instant::now(),
                )
                .unwrap_err()
            ),
            "UPSTREAM_TIMEOUT"
        );
    }
}
