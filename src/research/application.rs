//! Application boundary: typed retrieval, local normalization and validated atomic commit.
use crate::{
    contracts,
    error::{self, Code, fail},
    images,
    ozon::model::{
        ProductDetails, ProductReadOptions, ReviewPage, ReviewReadOptions, SearchResponse,
    },
    ozon::outcome::ContextObservation,
    ozon::search::SearchArgs,
    ozon::source::OzonSource,
    research::evidence::{self, envelope},
    research::journal::{Journal, JournalTxn, StoredRef},
    research::observations::{self, Normalized, ProductContinuation, SearchCriteria},
    research::response,
    runtime::config::Config,
    runtime::wire::{ImagePayload, ToolReply},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore, oneshot},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use std::collections::VecDeque;

const OPERATION_TIME: Duration = Duration::from_secs(44);
const REQUEST_TIME: Duration = Duration::from_secs(55);

#[derive(Clone)]
pub struct Application {
    inner: Arc<Inner>,
}
struct Inner {
    config: Config,
    journal: StdMutex<Journal>,
    source: Mutex<Option<SourceBackend>>,
    admission: Arc<Semaphore>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    shutdown: CancellationToken,
}
#[derive(Clone)]
struct Budget {
    cancel: CancellationToken,
    shutdown: CancellationToken,
    deadline: Instant,
}
impl Budget {
    fn check(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(fail(
                Code::UpstreamTimeout,
                "The operation deadline expired",
            ));
        }
        if self.cancel.is_cancelled() || self.shutdown.is_cancelled() {
            return Err(fail(Code::Cancelled, "The request was cancelled"));
        }
        Ok(())
    }
}

enum SourceBackend {
    Real(Box<OzonSource>),
    #[cfg(test)]
    Fake(Box<FakeSource>),
}
impl SourceBackend {
    async fn context(&mut self, cancel: &CancellationToken) -> Result<ContextObservation> {
        match self {
            Self::Real(g) => g.context(cancel).await,
            #[cfg(test)]
            Self::Fake(g) => g.context(cancel).await,
        }
    }
    async fn search(
        &mut self,
        args: SearchArgs,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse> {
        match self {
            Self::Real(g) => g.search(args, cancel).await,
            #[cfg(test)]
            Self::Fake(g) => {
                g.search_args.push(args);
                run_fake(g.searches.pop_front(), cancel, &g.entered).await
            }
        }
    }
    async fn product(
        &mut self,
        target: &str,
        options: &ProductReadOptions,
        cancel: &CancellationToken,
    ) -> Result<ProductDetails> {
        match self {
            Self::Real(g) => g.product_with_options(target, options, cancel).await,
            #[cfg(test)]
            Self::Fake(g) => {
                g.targets.push(target.into());
                g.product_options.push(options.clone());
                run_fake(g.products.pop_front(), cancel, &g.entered).await
            }
        }
    }
    async fn offers_page(
        &mut self,
        sku: &str,
        path: &str,
        cancel: &CancellationToken,
    ) -> Result<(
        crate::ozon::model::ProductOffers,
        crate::ozon::model::ReadUsage,
    )> {
        match self {
            Self::Real(g) => g.offers_page(sku, path, cancel).await,
            #[cfg(test)]
            Self::Fake(g) => {
                g.targets.push(format!("offers:{sku}:{path}"));
                run_fake(g.offer_pages.pop_front(), cancel, &g.entered).await
            }
        }
    }
    async fn reviews(
        &mut self,
        path: &str,
        options: &ReviewReadOptions,
        cancel: &CancellationToken,
    ) -> Result<ReviewPage> {
        match self {
            Self::Real(g) => g.reviews_with_options(path, options, cancel).await,
            #[cfg(test)]
            Self::Fake(g) => {
                g.review_options.push(options.clone());
                run_fake(g.reviews.pop_front(), cancel, &g.entered).await
            }
        }
    }
    async fn image(
        &mut self,
        url: &str,
        doh_enabled: bool,
        cancel: &CancellationToken,
    ) -> Result<images::FetchedImage> {
        match self {
            Self::Real(_) => images::fetch_image(url, doh_enabled, cancel).await,
            #[cfg(test)]
            Self::Fake(g) => run_fake(g.images.pop_front(), cancel, &g.entered).await,
        }
    }
    async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Real(g) => g.shutdown().await,
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
        }
    }
}

impl Application {
    pub async fn new(config: &Config) -> Result<Self> {
        let mut journal = Journal::open(&config.root).context("open journal")?;
        journal.maintain()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config: config.clone(),
                journal: StdMutex::new(journal),
                source: Mutex::new(None),
                admission: Arc::new(Semaphore::new(8)),
                tasks: Mutex::new(vec![]),
                shutdown: CancellationToken::new(),
            }),
        })
    }
    pub async fn call(&self, name: &str, mut args: Value, cancel: CancellationToken) -> ToolReply {
        if let Err(error) = contracts::normalize_input(name, &mut args) {
            return failure(&error, research_arg(&args));
        }
        let permit = match self.inner.admission.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return failure(
                    &fail(
                        Code::ServerBusy,
                        "Eight requests are active or completing cleanup",
                    ),
                    research_arg(&args),
                );
            }
        };
        let (tx, rx) = oneshot::channel();
        let inner = self.inner.clone();
        let name = name.to_owned();
        let caller = cancel.clone();
        let token = cancel.child_token();
        let rid = research_arg(&args).map(str::to_owned);
        let started = Instant::now();
        let mut tasks = self.inner.tasks.lock().await;
        if self.inner.shutdown.is_cancelled() {
            return failure(
                &fail(Code::Cancelled, "The server is shutting down"),
                rid.as_deref(),
            );
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio::spawn(async move {
            // This shield owns admission and execution through actual cleanup.
            let _permit = permit;
            let budget = Budget {
                cancel: token.clone(),
                shutdown: inner.shutdown.clone(),
                deadline: started + OPERATION_TIME,
            };
            let work = execute(&inner, &name, args, &budget);
            tokio::pin!(work);
            let mut sender = Some(tx);
            let mut operation_timer = true;
            loop {
                tokio::select! {
                    biased;
                    result = &mut work => {
                        if let Some(sender) = sender.take() {
                            let reply = result.unwrap_or_else(|error| failure(&error, rid.as_deref()));
                            let _ = sender.send(reply);
                        }
                        break;
                    }
                    _ = caller.cancelled(), if sender.is_some() => {
                        token.cancel();
                        if let Some(sender) = sender.take() {
                            let _ = sender.send(failure(&fail(Code::Cancelled, "The request was cancelled"), rid.as_deref()));
                        }
                    }
                    _ = inner.shutdown.cancelled(), if sender.is_some() => {
                        token.cancel();
                        if let Some(sender) = sender.take() {
                            let _ = sender.send(failure(&fail(Code::Cancelled, "The server is shutting down"), rid.as_deref()));
                        }
                    }
                    _ = tokio::time::sleep_until(started + OPERATION_TIME), if operation_timer => {
                        operation_timer = false;
                        token.cancel();
                    }
                    _ = tokio::time::sleep_until(started + REQUEST_TIME), if sender.is_some() => {
                        token.cancel();
                        if let Some(sender) = sender.take() {
                            let _ = sender.send(failure(&fail(Code::UpstreamTimeout, "The request deadline expired; cleanup continues"), rid.as_deref()));
                        }
                    }
                }
            }
        }));
        drop(tasks);
        rx.await.unwrap_or_else(|_| {
            failure(
                &fail(Code::Cancelled, "The request owner ended before replying"),
                None,
            )
        })
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.shutdown.cancel();
        let tasks = {
            let mut tasks = self.inner.tasks.lock().await;
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            let _ = task.await;
        }
        let mut source = self.inner.source.lock().await;
        match source.as_mut() {
            Some(source) => source.shutdown().await,
            None => Ok(()),
        }
    }
}
fn failure(error: &anyhow::Error, rid: Option<&str>) -> ToolReply {
    contracts::failure(error::code(error), &error::safe_message(error), rid)
}
fn journal(inner: &Inner) -> Result<std::sync::MutexGuard<'_, Journal>> {
    inner
        .journal
        .lock()
        .map_err(|_| fail(Code::SourceChanged, "The journal lock is unavailable"))
}
fn research_arg(args: &Value) -> Option<&str> {
    args.get("researchId").and_then(Value::as_str)
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| fail(Code::InvalidReference, format!("Missing {key} binding")))
}
fn context_id(context: &Value) -> Result<&str> {
    text(context, "contextId")
}
fn unknown_context() -> Value {
    json!({"contextId":"context-unverified","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"unknown"})
}
fn cached_context(journal: &Journal) -> Result<Value> {
    Ok(journal.meta("context")?.unwrap_or_else(unknown_context))
}
fn observed_context(raw: &ContextObservation) -> Value {
    let id = match raw.signature.as_deref() {
        Some(signature) => {
            let mut digest = Sha256::new();
            digest.update(signature.as_bytes());
            digest.update(raw.region_label.as_deref().unwrap_or("").as_bytes());
            digest.update([raw.region_verified as u8]);
            digest.update(raw.account_state.as_str().as_bytes());
            format!("context-{:x}", digest.finalize())
        }
        None => "context-unverified".into(),
    };
    json!({
        "contextId": id,
        "regionLabel": raw.region_label,
        "regionVerification": if raw.region_verified{
            "verified"
        }else{
            "unverified"
        },
        "accountState": raw.account_state,
        "accessState": raw.access_state
    })
}
fn confirm(before: &ContextObservation, after: &ContextObservation) -> Result<()> {
    if before.signature.is_none() || after.signature.is_none() {
        return Err(fail(
            Code::ContextUnverified,
            "The observable marketplace context could not be bound",
        ));
    }
    if before.signature != after.signature
        || before.region_label != after.region_label
        || before.region_verified != after.region_verified
        || before.account_state != after.account_state
    {
        return Err(fail(
            Code::ContextChanged,
            "Marketplace account or region changed during retrieval",
        ));
    }
    Ok(())
}
fn verify(stored: &StoredRef, rid: Option<&str>, context: &str) -> Result<()> {
    if rid.is_some_and(|rid| rid != stored.research_id) {
        return Err(fail(
            Code::InvalidReference,
            "Reference belongs to another research",
        ));
    }
    if stored.context_id != context {
        return Err(fail(
            Code::ContextChanged,
            "Reference belongs to another marketplace context",
        ));
    }
    Ok(())
}
fn pin(inner: &Inner, rid: Option<&str>) -> Result<()> {
    if let Some(rid) = rid {
        journal(inner)?.lease(rid)?;
    }
    Ok(())
}
fn ensure_or_create(
    txn: &mut JournalTxn<'_>,
    rid: Option<&str>,
    context: &Value,
    title: &str,
) -> Result<String> {
    match rid {
        Some(rid) => {
            txn.ensure_research(rid, context_id(context)?)?;
            Ok(rid.into())
        }
        None => txn.create_research(title, context),
    }
}
fn prepare_reply(
    txn: &mut JournalTxn<'_>,
    name: &str,
    mut structured: Value,
    images: Vec<ImagePayload>,
) -> Result<ToolReply> {
    txn.set_meta("context", &structured["context"])?;
    let unverified = structured["context"]["regionVerification"] != "verified";
    let warnings = structured
        .get_mut("warnings")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| fail(Code::SourceChanged, "Invalid response warnings"))?;
    if unverified {
        warnings.push(observations::warning(
            "REGION_UNVERIFIED",
            "Price and delivery observations use an unverified region.",
            &[],
        ));
    }
    *warnings = dedup_warnings(std::mem::take(warnings));
    response::success(name, structured, images)
}

fn validate_evidence(rid: &str, context: &Value, values: &[Value]) -> Result<()> {
    for value in values {
        response::success(
            "ozon_get_research",
            envelope(
                Some(rid),
                json!({"section":"evidence","payload":[value],"nextCursor":null}),
                context,
                vec![],
                value["observedAt"].as_str().unwrap_or(""),
            ),
            vec![],
        )?;
    }
    Ok(())
}
fn validate_candidates(rid: &str, context: &Value, items: &Value, observed: &str) -> Result<()> {
    for item in items
        .as_array()
        .ok_or_else(|| fail(Code::SourceChanged, "Invalid normalized candidates"))?
    {
        let mut snapshot = item.clone();
        snapshot["observedAt"] = json!(observed);
        snapshot["contextId"] = context["contextId"].clone();
        response::success(
            "ozon_get_research",
            envelope(
                Some(rid),
                json!({"section":"candidates","payload":[snapshot],"nextCursor":null}),
                context,
                vec![],
                observed,
            ),
            vec![],
        )?;
    }
    Ok(())
}
fn dedup_warnings(values: Vec<Value>) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v["code"].clone().to_string()))
        .take(30)
        .collect()
}
async fn source_lock<'a>(
    inner: &'a Inner,
    budget: &Budget,
) -> Result<tokio::sync::MutexGuard<'a, Option<SourceBackend>>> {
    budget.check()?;
    let guard = tokio::select! {biased;_ = budget.cancel.cancelled()=>return Err(fail(Code::Cancelled,"Cancelled while waiting for marketplace access")),guard=inner.source.lock()=>guard};
    budget.check()?;
    Ok(guard)
}
async fn source<'a>(
    inner: &'a Inner,
    budget: &Budget,
) -> Result<tokio::sync::MutexGuard<'a, Option<SourceBackend>>> {
    let mut guard = source_lock(inner, budget).await?;
    if guard.is_none() {
        *guard = Some(SourceBackend::Real(Box::new(
            OzonSource::new(&inner.config).await?,
        )));
    }
    budget.check()?;
    Ok(guard)
}

async fn execute(inner: &Inner, name: &str, args: Value, budget: &Budget) -> Result<ToolReply> {
    budget.check()?;
    match name {
        "ozon_get_context" => get_context(inner, budget).await,
        "ozon_search" => search(inner, &args, budget).await,
        "ozon_get_products" => products(inner, &args, budget).await,
        "ozon_get_reviews" => reviews(inner, &args, budget).await,
        "ozon_get_images" => get_images(inner, &args, budget).await,
        "ozon_list_research"
        | "ozon_get_research"
        | "ozon_append_research_note"
        | "ozon_compare_products" => local_operation(inner, name, &args, budget),
        _ => Err(fail(Code::InvalidArgument, "Unknown tool")),
    }
}
async fn get_context(inner: &Inner, budget: &Budget) -> Result<ToolReply> {
    let mut guard = source(inner, budget).await?;
    let raw = guard
        .as_mut()
        .expect("initialized source")
        .context(&budget.cancel)
        .await?;
    let context = observed_context(&raw);
    let observed = evidence::now();
    let normalized = observations::normalize_context(&context, &observed);
    journal(inner)?.commit_operation(
        None,
        || budget.check(),
        |txn| {
            prepare_reply(
                txn,
                "ozon_get_context",
                envelope(
                    None,
                    normalized.data,
                    &context,
                    normalized.warnings,
                    &observed,
                ),
                vec![],
            )
        },
    )
}
fn local_operation(inner: &Inner, name: &str, args: &Value, budget: &Budget) -> Result<ToolReply> {
    let rid = research_arg(args);
    pin(inner, rid)?;
    journal(inner)?.commit_operation(
        rid,
        || budget.check(),
        |txn| {
            let context = match rid {
                Some(rid) => txn.research_context(rid)?,
                None => txn.meta("context")?.unwrap_or_else(unknown_context),
            };
            let data = match name {
                "ozon_list_research" => txn.list(args)?,
                "ozon_get_research" => txn.read(args)?,
                "ozon_append_research_note" => txn.append_note(args)?,
                "ozon_compare_products" => {
                    let rid =
                        rid.ok_or_else(|| fail(Code::InvalidArgument, "missing researchId"))?;
                    let refs: Vec<String> = args["productRefs"]
                        .as_array()
                        .ok_or_else(|| fail(Code::InvalidArgument, "missing productRefs"))?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    let candidates = txn.selected_products(rid, &refs)?;
                    if let Some(quantities) = args["quantities"].as_array() {
                        for quantity in quantities {
                            let evidence: Vec<String> = quantity["evidenceRefs"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect();
                            txn.validate_evidence_refs(rid, &evidence)?;
                        }
                    }
                    crate::research::compare::compare(args, &candidates)?
                }
                _ => return Err(fail(Code::InvalidArgument, "Unknown journal operation")),
            };
            response::success(
                name,
                envelope(rid, data, &context, vec![], &evidence::now()),
                vec![],
            )
        },
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchBinding {
    criteria: SearchCriteria,
    price_override: Option<(Option<u64>, Option<u64>)>,
    search_url: Option<String>,
    limit: usize,
    include_facets: bool,
    refinement_limit: usize,
}
impl SearchBinding {
    fn input(args: &Value, mut criteria: SearchCriteria, search_url: Option<&str>) -> Self {
        if let Some(range) = args.pointer("/start/priceRange") {
            criteria.price_min = range["minMinor"].as_u64();
            criteria.price_max = range["maxMinor"].as_u64();
        }
        Self {
            price_override: args
                .pointer("/start/priceRange")
                .map(|range| (range["minMinor"].as_u64(), range["maxMinor"].as_u64())),
            criteria,
            search_url: search_url.map(str::to_owned),
            limit: args["limit"].as_u64().unwrap_or(12) as usize,
            include_facets: args["includeFacets"].as_bool().unwrap_or(false),
            refinement_limit: args["refinementLimit"].as_u64().unwrap_or(12) as usize,
        }
    }
    fn verify(&self, args: &Value) -> Result<()> {
        for (key, value) in [
            ("limit", json!(self.limit)),
            ("includeFacets", json!(self.include_facets)),
            ("refinementLimit", json!(self.refinement_limit)),
        ] {
            if args.get(key).is_some_and(|requested| requested != &value) {
                return Err(fail(
                    Code::InvalidReference,
                    "Search continuation settings cannot change",
                ));
            }
        }
        Ok(())
    }
    fn source(&self, path: Option<&str>) -> SearchArgs {
        SearchArgs {
            query: if path.is_some() || self.search_url.is_some() {
                None
            } else {
                self.criteria.query.clone()
            },
            search_url: path.map(str::to_owned).or_else(|| self.search_url.clone()),
            include_facets: Some(self.include_facets),
            sort: None,
            price_min: if path.is_some() {
                None
            } else {
                self.price_override.and_then(|(min, _)| min)
            },
            price_max: if path.is_some() {
                None
            } else {
                self.price_override.and_then(|(_, max)| max)
            },
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchContinuation {
    binding: SearchBinding,
    raw: Option<SearchResponse>,
    offset: usize,
    next_path: Option<String>,
    seen: Vec<String>,
    chain_complete: bool,
    paths: Vec<String>,
    no_progress: usize,
    page_progress: bool,
}
fn default_review_reads() -> u32 {
    4
}
async fn search(inner: &Inner, args: &Value, budget: &Budget) -> Result<ToolReply> {
    let start = &args["start"];
    if let Some(cursor) = start["refinementsCursor"].as_str() {
        let _source_guard = source_lock(inner, budget).await?;
        let stored = journal(inner)?.get_ref(cursor, "search_refinements")?;
        let rid = stored.research_id;
        pin(inner, Some(&rid))?;
        return journal(inner)?.commit_operation(
            Some(&rid),
            || budget.check(),
            |txn| {
                let context = txn.meta("context")?.unwrap_or_else(unknown_context);
                let (rid, data, observed) = crate::research::refinements::continue_page(
                    txn,
                    cursor,
                    research_arg(args),
                    context_id(&context)?,
                    args["refinementLimit"].as_u64().map(|n| n as usize),
                    args["limit"].as_u64().map(|n| n as usize),
                    args["includeFacets"].as_bool(),
                )?;
                response::success(
                    "ozon_search",
                    envelope(Some(&rid), data, &context, vec![], &observed),
                    vec![],
                )
            },
        );
    }
    let mut rid = research_arg(args).map(str::to_owned);
    let mut reference = None;
    let mut state = if let Some(cursor) = start["cursor"].as_str() {
        let stored = journal(inner)?.get_ref(cursor, "search_cursor")?;
        if rid.as_ref().is_some_and(|rid| rid != &stored.research_id) {
            return Err(fail(Code::InvalidReference, "Foreign search cursor"));
        }
        rid = Some(stored.research_id.clone());
        let state: SearchContinuation = serde_json::from_value(stored.value.clone())
            .map_err(|_| fail(Code::SourceChanged, "Invalid search cursor"))?;
        state.binding.verify(args)?;
        reference = Some(stored);
        state
    } else {
        let (criteria, path) = if let Some(query) = start["query"].as_str() {
            (SearchCriteria::for_query(query), None)
        } else {
            let stored = journal(inner)?.get_ref(text(start, "searchRef")?, "search_ref")?;
            if rid.as_ref().is_some_and(|rid| rid != &stored.research_id) {
                return Err(fail(Code::InvalidReference, "Foreign search refinement"));
            }
            rid = Some(stored.research_id.clone());
            let path = text(&stored.value, "searchUrl")?.to_owned();
            let criteria = serde_json::from_value(stored.value["criteria"].clone())
                .map_err(|_| fail(Code::SourceChanged, "Invalid refinement criteria"))?;
            reference = Some(stored);
            (criteria, Some(path))
        };
        SearchContinuation {
            binding: SearchBinding::input(args, criteria, path.as_deref()),
            raw: None,
            offset: 0,
            next_path: None,
            seen: vec![],
            chain_complete: true,
            paths: vec![],
            no_progress: 0,
            page_progress: false,
        }
    };
    pin(inner, rid.as_deref())?;
    let cached = state.raw.is_some();
    if cached {
        let _source_guard = source_lock(inner, budget).await?;
        let context = cached_context(&*journal(inner)?)?;
        if let Some(reference) = &reference {
            verify(reference, rid.as_deref(), context_id(&context)?)?;
        }
        return commit_search(inner, rid.as_deref(), &context, state, budget);
    }
    let mut guard = source(inner, budget).await?;
    let source = guard.as_mut().expect("initialized source");
    let before = source.context(&budget.cancel).await?;
    let context = observed_context(&before);
    if let Some(reference) = &reference {
        verify(reference, rid.as_deref(), context_id(&context)?)?;
    }
    if let Some(rid) = &rid {
        journal(inner)?.ensure_research(rid, context_id(&context)?)?;
    }
    let raw = source
        .search(
            state.binding.source(state.next_path.as_deref()),
            &budget.cancel,
        )
        .await?;
    let after = source.context(&budget.cancel).await?;
    confirm(&before, &after)?;
    budget.check()?;
    state.raw = Some(raw);
    state.offset = 0;
    commit_search(inner, rid.as_deref(), &context, state, budget)
}
fn commit_search(
    inner: &Inner,
    rid: Option<&str>,
    context: &Value,
    mut state: SearchContinuation,
    budget: &Budget,
) -> Result<ToolReply> {
    let mut raw = state
        .raw
        .take()
        .ok_or_else(|| fail(Code::SourceChanged, "Search cursor has no captured page"))?;
    if state.binding.criteria.price_min.is_some() || state.binding.criteria.price_max.is_some() {
        for item in &mut raw.items {
            item.matches_price_range = item.price_minor.map(|price| {
                state
                    .binding
                    .criteria
                    .price_min
                    .is_none_or(|min| price >= min)
                    && state
                        .binding
                        .criteria
                        .price_max
                        .is_none_or(|max| price <= max)
            });
        }
    }
    let observed = raw
        .acquisition
        .as_ref()
        .map(|a| a.observed_at.clone())
        .unwrap_or_else(evidence::now);
    journal(inner)?.commit_operation(
        rid,
        || budget.check(),
        |txn| {
            let rid = ensure_or_create(
                txn,
                rid,
                context,
                state
                    .binding
                    .criteria
                    .query
                    .as_deref()
                    .unwrap_or("Refined product search"),
            )?;
            let end = (state.offset + state.binding.limit).min(raw.items.len());
            if state.offset > raw.items.len() {
                return Err(fail(
                    Code::SourceChanged,
                    "Invalid captured search position",
                ));
            }
            let mut seen = state.seen.iter().cloned().collect::<BTreeSet<_>>();
            let previous = seen.len();
            for item in &raw.items[state.offset..end] {
                seen.insert(item.sku.clone());
            }
            if seen.len() > 30_000 {
                return Err(fail(
                    Code::ResultTooLarge,
                    "Search chain identity limit reached",
                ));
            }
            state.chain_complete &= raw.source.complete();
            state.page_progress |= previous != seen.len();
            if end == raw.items.len() {
                if !state.page_progress {
                    state.no_progress += 1;
                } else {
                    state.no_progress = 0;
                }
                if !state.paths.contains(&raw.search_url) {
                    state.paths.push(raw.search_url.clone());
                }
            }
            let chain_limit = state.paths.len() >= 1000;
            let stalled = chain_limit
                || state.no_progress >= 3
                || raw
                    .next_path
                    .as_ref()
                    .is_some_and(|path| state.paths.contains(path));
            let has_remainder = end < raw.items.len();
            let has_next = !stalled && raw.next_path.is_some();
            let next = if has_remainder || has_next {
                let next_state = SearchContinuation {
                    binding: state.binding.clone(),
                    raw: has_remainder.then(|| raw.clone()),
                    offset: if has_remainder { end } else { 0 },
                    next_path: if has_remainder {
                        None
                    } else {
                        raw.next_path.clone()
                    },
                    seen: seen.iter().cloned().collect(),
                    chain_complete: state.chain_complete,
                    paths: state.paths.clone(),
                    no_progress: state.no_progress,
                    page_progress: if has_remainder {
                        state.page_progress
                    } else {
                        false
                    },
                };
                Some(txn.put_ref(
                    &rid,
                    "search_cursor",
                    &serde_json::to_value(next_state)?,
                    Some(1800),
                )?)
            } else {
                None
            };
            let mut normalized = observations::normalize_search(
                &raw,
                txn,
                &rid,
                observations::SearchBatch {
                    range: state.offset..end,
                    context_id: context_id(context)?,
                    unique_seen: seen.len(),
                    chain_complete: state.chain_complete,
                    include_facets: state.binding.include_facets,
                    next_cursor: next.as_deref(),
                    criteria: &state.binding.criteria,
                },
            )?;
            if state.binding.criteria.price_min.is_some()
                || state.binding.criteria.price_max.is_some()
            {
                normalized.data["priceRangeAssessment"] =
                    json!({"basis":"displayed_search_price","sourceFilterGuaranteesMatch":false});
            }
            if stalled {
                normalized.data["coverage"]["completeness"] = json!("partial");
                normalized.warnings.push(observations::warning(
                    if chain_limit {
                        "CONTINUATION_LIMIT_REACHED"
                    } else {
                        "CONTINUATION_STALLED"
                    },
                    "The captured paginator reached its continuation boundary.",
                    &[],
                ));
            }
            if state.binding.include_facets {
                crate::research::refinements::first_page(
                    txn,
                    &rid,
                    &mut normalized.data,
                    &observed,
                    state.binding.refinement_limit,
                    state.binding.limit,
                )?;
            }
            validate_evidence(&rid, context, &normalized.evidence)?;
            validate_candidates(&rid, context, &normalized.data["items"], &observed)?;
            txn.record_search(
                &rid,
                &state.binding.criteria.summary(),
                &normalized.product_refs,
                &normalized.evidence,
                &mut normalized.data["items"],
            )?;
            prepare_reply(
                txn,
                "ozon_search",
                envelope(
                    Some(&rid),
                    normalized.data,
                    context,
                    normalized.warnings,
                    &observed,
                ),
                vec![],
            )
        },
    )
}

struct ResolvedProduct {
    target: String,
    research_id: Option<String>,
    context_id: Option<String>,
    continuation: Option<ProductContinuation>,
}
fn resolve_product(inner: &Inner, selector: &Value) -> Result<ResolvedProduct> {
    if let Some(cursor) = selector["cursor"].as_str() {
        let stored = journal(inner)?.get_ref(cursor, "product_cursor")?;
        let continuation: ProductContinuation = serde_json::from_value(stored.value)
            .map_err(|_| fail(Code::SourceChanged, "Invalid product cursor"))?;
        let target = continuation
            .raw
            .sku
            .clone()
            .ok_or_else(|| fail(Code::SourceChanged, "Product continuation has no SKU"))?;
        return Ok(ResolvedProduct {
            target,
            research_id: Some(stored.research_id),
            context_id: Some(stored.context_id),
            continuation: Some(continuation),
        });
    }
    if let Some(reference) = selector["productRef"].as_str() {
        let stored = journal(inner)?.get_ref(reference, "product")?;
        let target = stored.value["sku"]
            .as_str()
            .or_else(|| stored.value["url"].as_str())
            .ok_or_else(|| fail(Code::InvalidReference, "Product reference has no identity"))?
            .to_owned();
        return Ok(ResolvedProduct {
            target,
            research_id: Some(stored.research_id),
            context_id: Some(stored.context_id),
            continuation: None,
        });
    }
    let target = selector["sku"]
        .as_str()
        .or_else(|| selector["url"].as_str())
        .ok_or_else(|| fail(Code::InvalidArgument, "Missing product selector"))?
        .to_owned();
    Ok(ResolvedProduct {
        target,
        research_id: None,
        context_id: None,
        continuation: None,
    })
}
fn product_cache_key(target: &str) -> String {
    if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
        return target.to_owned();
    }
    if let Some(url) = evidence::canonical_source_url(target)
        && let Ok(url) = url::Url::parse(&url)
        && let Some(segment) = url
            .path()
            .strip_prefix("/product/")
            .and_then(|p| p.strip_suffix('/'))
        && !segment.contains('/')
    {
        let sku = segment.rsplit('-').next().unwrap_or("");
        if !sku.is_empty() && sku.chars().all(|c| c.is_ascii_digit()) {
            return sku.to_owned();
        }
    }
    target.to_owned()
}
async fn products(inner: &Inner, args: &Value, budget: &Budget) -> Result<ToolReply> {
    let selectors = args["products"]
        .as_array()
        .expect("validated product array");
    let mut rid = research_arg(args).map(str::to_owned);
    let mut resolved = vec![];
    for selector in selectors {
        let result = resolve_product(inner, selector).and_then(|product| {
            if let Some(bound) = &product.research_id {
                if rid.as_ref().is_some_and(|rid| rid != bound) {
                    return Err(fail(Code::InvalidReference, "Mixed-research product batch"));
                }
                rid = Some(bound.clone());
            }
            Ok(product)
        });
        resolved.push(result);
    }
    pin(inner, rid.as_deref())?;
    let include = args["include"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["characteristics".into()]);
    let names = args
        .get("characteristicNames")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
    for result in &mut resolved {
        if let Ok(item) = result
            && let Some(cursor) = &item.continuation
        {
            let pinned_include = if cursor.include.is_empty() {
                vec![cursor.section.clone()]
            } else {
                cursor.include.clone()
            };
            if names
                .as_ref()
                .is_some_and(|n| Some(n) != cursor.characteristic_names.as_ref())
                || args
                    .get("include")
                    .is_some_and(|_| include != pinned_include)
                || args
                    .get("maxSourceReads")
                    .is_some_and(|v| v.as_u64().map(|n| n as u32) != cursor.max_source_reads)
            {
                *result = Err(fail(
                    Code::InvalidReference,
                    "Product continuation settings cannot change",
                ));
            }
        }
    }
    let live = resolved.iter().any(|item| {
        item.as_ref().is_ok_and(|item| {
            item.continuation
                .as_ref()
                .is_none_or(|c| c.offers_path.is_some())
        })
    });
    let _local_guard = if !live {
        Some(source_lock(inner, budget).await?)
    } else {
        None
    };
    let mut source_guard = if live {
        Some(source(inner, budget).await?)
    } else {
        None
    };
    let before = match source_guard.as_mut() {
        Some(guard) => Some(
            guard
                .as_mut()
                .expect("initialized source")
                .context(&budget.cancel)
                .await?,
        ),
        None => None,
    };
    let context = match &before {
        Some(before) => observed_context(before),
        None => cached_context(&*journal(inner)?)?,
    };
    if let Some(rid) = &rid {
        journal(inner)?.ensure_research(rid, context_id(&context)?)?;
    }
    let read_limit = args["maxSourceReads"]
        .as_u64()
        .unwrap_or(if selectors.len() == 1 { 8 } else { 32 }) as u32;
    let mut source_reads = 0u32;
    let mut exhausted = false;
    let mut cache = std::collections::BTreeMap::<String, ProductDetails>::new();
    let mut fetched = vec![];
    let fetch_deadline = budget.deadline - Duration::from_secs(5);
    for result in resolved {
        let product = match result {
            Ok(product) => product,
            Err(error) => {
                fetched.push(Err(error));
                continue;
            }
        };
        if product
            .context_id
            .as_deref()
            .is_some_and(|bound| Some(bound) != context["contextId"].as_str())
        {
            fetched.push(Err(fail(
                Code::ContextChanged,
                "Product selector context changed",
            )));
            continue;
        }
        if let Some(mut continuation) = product.continuation {
            if let Some(path) = continuation.offers_path.take() {
                if source_reads >= read_limit {
                    exhausted = true;
                    fetched.push(Err(fail(
                        Code::SourceChanged,
                        "Product source-read budget exhausted",
                    )));
                    continue;
                }
                let source = source_guard
                    .as_mut()
                    .expect("live source")
                    .as_mut()
                    .expect("initialized source");
                let sku = continuation
                    .raw
                    .sku
                    .clone()
                    .ok_or_else(|| fail(Code::SourceChanged, "Offer cursor has no SKU"))?;
                if Instant::now() >= fetch_deadline {
                    fetched.push(Err(fail(
                        Code::UpstreamTimeout,
                        "Offer retrieval budget exhausted",
                    )));
                    continue;
                }
                let child = budget.cancel.child_token();
                let retrieve = source.offers_page(&sku, &path, &child);
                tokio::pin!(retrieve);
                let outcome = tokio::select! { biased; result = &mut retrieve => result, _ = tokio::time::sleep_until(fetch_deadline) => { child.cancel(); let outcome = retrieve.await; let usage = match outcome { Ok((_,usage)) => usage, Err(error) => error.downcast_ref::<crate::ozon::model::ReadUsage>().cloned().unwrap_or_default() }; Err(fail(Code::UpstreamTimeout, "Offer retrieval budget exhausted").context(usage)) } };
                match outcome {
                    Ok((offers, usage)) => {
                        source_reads += usage.source_reads;
                        exhausted |= usage.budget_exhausted;
                        continuation.raw.offers = Some(offers);
                        continuation.offers_paths.push(path.clone());
                        continuation.offset = 0;
                        fetched.push(Ok((
                            continuation.raw.clone(),
                            vec![continuation.section.clone()],
                            Some(continuation),
                            true,
                        )));
                    }
                    Err(error) => {
                        source_reads += error
                            .downcast_ref::<crate::ozon::model::ReadUsage>()
                            .map_or(1, |u| u.source_reads);
                        fetched.push(Err(error));
                    }
                }
            } else {
                fetched.push(Ok((
                    continuation.raw.clone(),
                    vec![continuation.section.clone()],
                    Some(continuation),
                    false,
                )));
            }
            continue;
        }
        if let Some(raw) = cache.get(&product_cache_key(&product.target)) {
            fetched.push(Ok((raw.clone(), include.clone(), None, true)));
            continue;
        }
        if source_reads >= read_limit {
            exhausted = true;
            fetched.push(Err(fail(
                Code::SourceChanged,
                "Product source-read budget exhausted",
            )));
            continue;
        }
        if Instant::now() >= fetch_deadline {
            fetched.push(Err(fail(
                Code::UpstreamTimeout,
                "Product batch retrieval budget exhausted",
            )));
            continue;
        }
        budget.check()?;
        let child = budget.cancel.child_token();
        let source = source_guard
            .as_mut()
            .expect("live source")
            .as_mut()
            .expect("initialized source");
        let options = ProductReadOptions {
            description: include.iter().any(|s| s == "description"),
            full_characteristics: include.iter().any(|s| s == "fullCharacteristics"),
            offers: include.iter().any(|s| s == "offers"),
            duty: include.iter().any(|s| s == "customsDuty"),
            max_source_reads: read_limit - source_reads,
        };
        let retrieve = source.product(&product.target, &options, &child);
        tokio::pin!(retrieve);
        let result = tokio::select! {biased; result=&mut retrieve=>result,_=tokio::time::sleep_until(fetch_deadline)=>{child.cancel();let outcome=retrieve.await; let usage=match outcome { Ok(raw)=>raw.read_usage, Err(error)=>error.downcast_ref::<crate::ozon::model::ReadUsage>().cloned().unwrap_or_default() }; Err(fail(Code::UpstreamTimeout,"Product retrieval budget exhausted").context(usage))}};
        if let Ok(raw) = &result {
            source_reads += raw.read_usage.source_reads;
            exhausted |= raw.read_usage.budget_exhausted;
            cache.insert(product_cache_key(&product.target), raw.clone());
        } else if let Err(error) = &result {
            let used = error
                .downcast_ref::<crate::ozon::model::ReadUsage>()
                .map(|u| u.source_reads)
                .unwrap_or(1);
            source_reads = source_reads.saturating_add(used).min(read_limit);
        }
        fetched.push(result.map(|raw| (raw, include.clone(), None, true)));
    }
    let confirmation = match (&before, source_guard.as_mut()) {
        (Some(before), Some(guard)) => match guard
            .as_mut()
            .expect("initialized source")
            .context(&budget.cancel)
            .await
        {
            Ok(after) => confirm(before, &after),
            Err(error) => Err(error),
        },
        _ => Ok(()),
    };
    budget.check()?;
    journal(inner)?.commit_operation(
        rid.as_deref(),
        || budget.check(),
        |txn| {
            let rid = ensure_or_create(txn, rid.as_deref(), &context, "Product research")?;
            let mut normalized = Normalized::default();
            let mut rows = vec![];
            for (selector, result) in selectors.iter().zip(fetched) {
                let (raw, include, continuation, live) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        rows.push(observations::item_error(selector, &error));
                        continue;
                    }
                };
                if live && let Err(error) = &confirmation {
                    rows.push(observations::item_error(selector, error));
                    continue;
                }
                let result = txn.stage(|txn| {
                    let selected_names = continuation.as_ref().and_then(|c| c.characteristic_names.as_deref()).or(names.as_deref());
                    let pinned_include = continuation.as_ref().map(|c| c.include.as_slice()).filter(|i| !i.is_empty()).unwrap_or(&include);
                    let pinned_limit = continuation.as_ref().and_then(|c| c.max_source_reads).or(args["maxSourceReads"].as_u64().map(|n| n as u32));
                    let mut value = observations::normalize_product(&raw, selector, txn, &rid, context_id(&context)?, &include, selected_names, pinned_include, pinned_limit, continuation.as_ref())?;
                    validate_evidence(&rid, &context, &value.evidence)?;
                    let check = |value: &Normalized| {
                        let mut all_rows = rows.clone(); all_rows.push(value.data.clone());
                        let mut warnings = normalized.warnings.clone(); warnings.extend(value.warnings.clone());
                        response::success("ozon_get_products", envelope(Some(&rid), json!({"results":all_rows,"usage":{"sourceReads":source_reads,"sourceReadLimit":read_limit,"budgetExhausted":exhausted,"unit":"logical_source_reads"}}), &context, warnings, &evidence::now()), vec![])
                    };
                    if let Err(error) = check(&value) {
                        if error::code(&error) != "RESULT_TOO_LARGE" || continuation.is_some() { return Err(error); }
                        observations::compact_product_sections(&mut value, &raw, txn, &rid, &include, selected_names, pinned_include, pinned_limit, continuation.as_ref())?;
                        check(&value)?;
                    }
                    Ok(value)
                });
                match result {
                    Ok(value) => {
                        rows.push(value.data);
                        normalized.evidence.extend(value.evidence);
                        normalized.product_refs.extend(value.product_refs);
                        normalized.warnings.extend(value.warnings);
                    }
                    Err(error) => {
                        if error::code(&error) == "STORAGE_FULL" {
                            return Err(error);
                        }
                        rows.push(observations::item_error(selector, &error));
                    }
                }
            }
            normalized.product_refs.sort();
            normalized.product_refs.dedup();
            txn.record(
                &rid,
                "products",
                "Captured ordered product outcomes",
                &normalized.product_refs,
                &normalized.evidence,
            )?;
            let observed = normalized
                .evidence
                .iter()
                .filter_map(|ev| ev["observedAt"].as_str())
                .min()
                .map(str::to_owned)
                .unwrap_or_else(evidence::now);
            prepare_reply(
                txn,
                "ozon_get_products",
                envelope(
                    Some(&rid),
                    json!({"results":rows,"usage":{"sourceReads":source_reads,"sourceReadLimit":read_limit,"budgetExhausted":exhausted,"unit":"logical_source_reads"}}),
                    &context,
                    normalized.warnings,
                    &observed,
                ),
                vec![],
            )
        },
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewContinuation {
    product_ref: String,
    sku: String,
    path: String,
    raw: Option<ReviewPage>,
    offset: usize,
    limit: usize,
    include_facets: bool,
    seen_ref: Option<String>,
    seen_depth: usize,
    chain_complete: bool,
    paths: Vec<String>,
    no_progress: usize,
    page_progress: bool,
    #[serde(default)]
    include_details: bool,
    #[serde(default)]
    include_seller_replies: bool,
    #[serde(default)]
    text_limit: Option<usize>,
    #[serde(default = "default_review_reads")]
    max_source_reads: u32,
}
async fn reviews(inner: &Inner, args: &Value, budget: &Budget) -> Result<ToolReply> {
    let start = &args["start"];
    let stored = if let Some(cursor) = start["cursor"].as_str() {
        journal(inner)?.get_ref(cursor, "review_cursor")?
    } else if let Some(reference) = start["reviewSearchRef"].as_str() {
        journal(inner)?.get_ref(reference, "review_search")?
    } else {
        journal(inner)?.get_ref(text(start, "productRef")?, "product")?
    };
    let rid = stored.research_id.clone();
    if research_arg(args).is_some_and(|rid| rid != stored.research_id) {
        return Err(fail(Code::InvalidReference, "Foreign review selector"));
    }
    pin(inner, Some(&rid))?;
    let mut state = if start["cursor"].is_string() {
        let state: ReviewContinuation = serde_json::from_value(stored.value.clone())
            .map_err(|_| fail(Code::SourceChanged, "Invalid review cursor"))?;
        if args
            .get("limit")
            .is_some_and(|limit| limit.as_u64() != Some(state.limit as u64))
            || args
                .get("includeFacets")
                .is_some_and(|include| include.as_bool() != Some(state.include_facets))
            || args
                .get("includeDetails")
                .is_some_and(|v| v.as_bool() != Some(state.include_details))
            || args
                .get("includeSellerReplies")
                .is_some_and(|v| v.as_bool() != Some(state.include_seller_replies))
            || args
                .get("textLimit")
                .is_some_and(|v| v.as_u64().map(|n| n as usize) != state.text_limit)
            || args
                .get("maxSourceReads")
                .is_some_and(|v| v.as_u64() != Some(state.max_source_reads as u64))
        {
            return Err(fail(
                Code::InvalidReference,
                "Review continuation settings cannot change",
            ));
        }
        state
    } else {
        let sku = text(&stored.value, "sku")?.to_owned();
        let product_ref = start["productRef"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| stored.value["productRef"].as_str().map(str::to_owned))
            .ok_or_else(|| {
                fail(
                    Code::InvalidReference,
                    "Review selector has no product binding",
                )
            })?;
        let path = stored.value["path"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("/product/{sku}/?tab=reviews"));
        ReviewContinuation {
            product_ref,
            sku,
            path,
            raw: None,
            offset: 0,
            limit: args["limit"].as_u64().unwrap_or(10) as usize,
            include_facets: args["includeFacets"].as_bool().unwrap_or(false),
            seen_ref: None,
            seen_depth: 0,
            chain_complete: true,
            paths: vec![],
            no_progress: 0,
            page_progress: false,
            include_details: args["includeDetails"].as_bool().unwrap_or(false),
            include_seller_replies: args["includeSellerReplies"].as_bool().unwrap_or(false),
            text_limit: args["textLimit"].as_u64().map(|n| n as usize),
            max_source_reads: args["maxSourceReads"].as_u64().unwrap_or(4) as u32,
        }
    };
    if state.raw.is_some() {
        let _source_guard = source_lock(inner, budget).await?;
        let context = cached_context(&*journal(inner)?)?;
        verify(&stored, Some(&rid), context_id(&context)?)?;
        return commit_reviews(inner, &rid, &context, state, budget, false);
    }
    let mut guard = source(inner, budget).await?;
    let source = guard.as_mut().expect("initialized source");
    let before = source.context(&budget.cancel).await?;
    let context = observed_context(&before);
    verify(&stored, Some(&rid), context_id(&context)?)?;
    let options = ReviewReadOptions {
        include_details: state.include_details,
        include_seller_replies: state.include_seller_replies,
        max_source_reads: state.max_source_reads,
    };
    let raw = source
        .reviews(&state.path, &options, &budget.cancel)
        .await?;
    let after = source.context(&budget.cancel).await?;
    confirm(&before, &after)?;
    budget.check()?;
    state.raw = Some(raw);
    state.offset = 0;
    commit_reviews(inner, &rid, &context, state, budget, true)
}
fn commit_reviews(
    inner: &Inner,
    rid: &str,
    context: &Value,
    mut state: ReviewContinuation,
    budget: &Budget,
    live: bool,
) -> Result<ToolReply> {
    let raw = state
        .raw
        .take()
        .ok_or_else(|| fail(Code::SourceChanged, "Review cursor has no captured page"))?;
    let identity_url = raw
        .acquisition
        .as_ref()
        .map(|a| a.source_url.clone())
        .or_else(|| raw.source_url.clone())
        .unwrap_or_else(|| format!("https://www.ozon.ru{}", state.path));
    let observed = raw
        .acquisition
        .as_ref()
        .map(|a| a.observed_at.clone())
        .or_else(|| raw.observed_at.clone())
        .unwrap_or_else(evidence::now);
    journal(inner)?.commit_operation(
        Some(rid),
        || budget.check(),
        |txn| {
            txn.ensure_research(rid, context_id(context)?)?;
            let mut seen = observations::load_review_seen(
                txn,
                rid,
                context_id(context)?,
                state.seen_ref.as_deref(),
            )?;
            let mut new_keys = vec![];
            let mut indices = vec![];
            let mut offset = state.offset;
            if offset > raw.reviews.len() {
                return Err(fail(
                    Code::SourceChanged,
                    "Invalid captured review position",
                ));
            }
            let mut text_units = 0usize;
            while offset < raw.reviews.len() && indices.len() < state.limit {
                let item = &raw.reviews[offset];
                let units: usize = [&item.comment, &item.pros, &item.cons].iter().filter_map(|s| s.as_ref()).map(|s| s.encode_utf16().count()).sum();
                let reply_units = if state.include_seller_replies { 4000 } else { 0 };
                let shown = units.min(state.text_limit.unwrap_or(12000).min(12000) * 2) + reply_units;
                if !indices.is_empty() && text_units + shown > 30000 { break; }
                text_units += shown;
                let key =
                    observations::review_identity(&raw.reviews[offset], &identity_url, offset)?;
                if seen.insert(key.clone()) {
                    new_keys.push(key);
                    indices.push(offset);
                }
                offset += 1;
            }
            let (seen_ref, seen_depth) = observations::save_review_seen(
                txn,
                rid,
                state.seen_ref.as_deref(),
                state.seen_depth,
                &seen,
                new_keys,
            )?;
            state.chain_complete &= raw.source.complete();
            state.page_progress |= !indices.is_empty();
            if offset == raw.reviews.len() {
                if !state.page_progress {
                    state.no_progress += 1;
                } else {
                    state.no_progress = 0;
                }
                if !state.paths.contains(&state.path) {
                    state.paths.push(state.path.clone());
                }
            }
            let chain_limit = state.paths.len() >= 1000;
            let stalled = chain_limit
                || state.no_progress >= observations::MAX_REVIEW_NO_PROGRESS_PAGES
                || raw
                    .next_path
                    .as_ref()
                    .is_some_and(|path| state.paths.contains(path));
            let remainder = offset < raw.reviews.len();
            let next = if remainder || (!stalled && raw.next_path.is_some()) {
                let next = ReviewContinuation {
                    product_ref: state.product_ref.clone(),
                    sku: state.sku.clone(),
                    path: if remainder {
                        state.path.clone()
                    } else {
                        raw.next_path.clone().expect("observed next path")
                    },
                    raw: remainder.then(|| raw.clone()),
                    offset: if remainder { offset } else { 0 },
                    limit: state.limit,
                    include_facets: state.include_facets,
                    seen_ref,
                    seen_depth,
                    chain_complete: state.chain_complete,
                    paths: state.paths.clone(),
                    no_progress: state.no_progress,
                    include_details: state.include_details,
                    include_seller_replies: state.include_seller_replies,
                    text_limit: state.text_limit,
                    max_source_reads: state.max_source_reads,
                    page_progress: if remainder {
                        state.page_progress
                    } else {
                        false
                    },
                };
                Some(txn.put_ref(
                    rid,
                    "review_cursor",
                    &serde_json::to_value(next)?,
                    Some(1800),
                )?)
            } else {
                None
            };
            let mut normalized = observations::normalize_reviews(
                &raw,
                txn,
                rid,
                observations::ReviewBatch {
                    indices: &indices,
                    context_id: context_id(context)?,
                    product_ref: &state.product_ref,
                    sku: &state.sku,
                    source_url: &identity_url,
                    unique_seen: seen.len(),
                    chain_complete: state.chain_complete,
                    include_facets: state.include_facets,
                    next_cursor: next.as_deref(),
                    text_limit: state.text_limit,
                    include_details: state.include_details,
                    include_seller_replies: state.include_seller_replies,
                },
            )?;
            normalized.data["usage"] = json!({"sourceReads":if live {raw.read_usage.source_reads} else {0},"sourceReadLimit":state.max_source_reads,"budgetExhausted":live && raw.read_usage.budget_exhausted,"unit":"logical_source_reads"});
            if stalled {
                normalized.data["coverage"]["completeness"] = json!("partial");
                normalized.warnings.push(observations::warning(
                    if chain_limit {
                        "CONTINUATION_LIMIT_REACHED"
                    } else {
                        "CONTINUATION_STALLED"
                    },
                    "The review paginator reached its continuation boundary.",
                    &[],
                ));
            }
            validate_evidence(rid, context, &normalized.evidence)?;
            txn.record(
                rid,
                "reviews",
                "Captured review sample",
                &normalized.product_refs,
                &normalized.evidence,
            )?;
            prepare_reply(
                txn,
                "ozon_get_reviews",
                envelope(
                    Some(rid),
                    normalized.data,
                    context,
                    normalized.warnings,
                    &observed,
                ),
                vec![],
            )
        },
    )
}

async fn get_images(inner: &Inner, args: &Value, budget: &Budget) -> Result<ToolReply> {
    let refs = args["imageRefs"].as_array().expect("validated image refs");
    let mut rid = research_arg(args).map(str::to_owned);
    let mut resolved = vec![];
    for reference in refs {
        let result = journal(inner)?
            .get_ref(
                reference.as_str().expect("validated image reference"),
                "image",
            )
            .and_then(|stored| {
                if rid.as_ref().is_some_and(|rid| rid != &stored.research_id) {
                    return Err(fail(Code::InvalidReference, "Mixed-research image batch"));
                }
                rid = Some(stored.research_id.clone());
                Ok(stored)
            });
        resolved.push(result);
    }
    pin(inner, rid.as_deref())?;
    let mut guard = source(inner, budget).await?;
    let source = guard.as_mut().expect("initialized source");
    let before = source.context(&budget.cancel).await?;
    let context = observed_context(&before);
    let mut fetched = vec![];
    for result in resolved {
        let stored = match result {
            Ok(stored) => stored,
            Err(error) => {
                fetched.push(Err(error));
                continue;
            }
        };
        let result = verify(&stored, rid.as_deref(), context_id(&context)?)
            .and_then(|_| text(&stored.value, "url").map(str::to_owned));
        let url = match result {
            Ok(url) => url,
            Err(error) => {
                fetched.push(Err(error));
                continue;
            }
        };
        budget.check()?;
        let result = source
            .image(&url, inner.config.image_doh_fallback, &budget.cancel)
            .await;
        fetched.push(result.map(|image| (stored, image)));
    }
    let confirmation = match source.context(&budget.cancel).await {
        Ok(after) => confirm(&before, &after),
        Err(error) => Err(error),
    };
    confirmation?;
    budget.check()?;
    journal(inner)?.commit_operation(
        rid.as_deref(),
        || budget.check(),
        |txn| {
            let rid = ensure_or_create(txn, rid.as_deref(), &context, "Image research")?;
            let mut results = vec![];
            let mut payloads = vec![];
            let mut all_evidence = vec![];
            for (reference, result) in refs.iter().zip(fetched) {
                let (stored, image) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        results.push(json!({
                            "imageRef": reference,
                            "status": "error",
                            "error": observations::error_value(&error),
                        }));
                        continue;
                    }
                };
                let kind = text(&stored.value, "sourceKind")?;
                let source_ref = text(&stored.value, "sourceRef")?;
                let original = text(&stored.value, "evidenceRef")?;
                let ev = json!({
                    "evidenceRef": evidence::unique("evidence"),
                    "sourceKind": "local_journal",
                    "sourceUrl": null,
                    "observedAt": image.retrieved_at,
                    "contextId": context_id(&context)?,
                    "sku": stored.value["sku"],
                    "facts": [
                        evidence::fact("/sha256", json!(image.sha256)),
                        evidence::fact("/mimeType", json!(image.mime_type)),
                        evidence::fact("/width", json!(image.width)),
                        evidence::fact("/height", json!(image.height)),
                        evidence::fact("/retrievedAt", json!(image.retrieved_at)),
                        evidence::fact("/sourceUrl", stored.value["sourceUrl"].clone()),
                        evidence::fact("/sourceObservedAt", stored.value["observedAt"].clone()),
                        evidence::fact("/sourceFieldPath", stored.value["fieldPath"].clone()),
                        evidence::fact("/sourceRef", json!(source_ref)),
                    ],
                });
                let eid = text(&ev, "evidenceRef")?.to_owned();
                let content_index = payloads.len() + 1;
                results.push(json!({
                    "imageRef": reference,
                    "status": "ok",
                    "sourceKind": kind,
                    "sourceRef": source_ref,
                    "evidenceRefs": [original, eid],
                    "mimeType": image.mime_type,
                    "width": image.width,
                    "height": image.height,
                    "sha256": image.sha256,
                    "retrievedAt": image.retrieved_at,
                    "contentIndex": content_index,
                }));
                payloads.push(ImagePayload {
                    data: image.data,
                    mime_type: image.mime_type,
                });
                all_evidence.push(ev);
            }
            validate_evidence(&rid, &context, &all_evidence)?;
            txn.record(
                &rid,
                "images",
                "Fetched bound raster images",
                &[],
                &all_evidence,
            )?;
            prepare_reply(
                txn,
                "ozon_get_images",
                envelope(
                    Some(&rid),
                    json!({"results": results}),
                    &context,
                    vec![],
                    &evidence::now(),
                ),
                payloads,
            )
        },
    )
}

#[cfg(test)]
enum FakeAction<T> {
    Observation(T),
    Failure(Code),
    Cancelled {
        cleanup_entered: Arc<tokio::sync::Notify>,
        cleanup_release: Arc<tokio::sync::Notify>,
    },
}
#[cfg(test)]
struct FakeSource {
    contexts: VecDeque<ContextObservation>,
    last_context: ContextObservation,
    searches: VecDeque<FakeAction<SearchResponse>>,
    search_args: Vec<SearchArgs>,
    products: VecDeque<FakeAction<ProductDetails>>,
    reviews: VecDeque<FakeAction<ReviewPage>>,
    offer_pages: VecDeque<
        FakeAction<(
            crate::ozon::model::ProductOffers,
            crate::ozon::model::ReadUsage,
        )>,
    >,
    images: VecDeque<FakeAction<images::FetchedImage>>,
    entered: Arc<tokio::sync::Notify>,
    targets: Vec<String>,
    product_options: Vec<ProductReadOptions>,
    review_options: Vec<ReviewReadOptions>,
}
#[cfg(test)]
impl FakeSource {
    fn new() -> Self {
        Self {
            contexts: VecDeque::new(),
            last_context: ContextObservation {
                region_label: Some("Москва".into()),
                region_verified: true,
                region_source_url: None,
                account_state: crate::ozon::outcome::AccountState::Anonymous,
                access_state: crate::ozon::outcome::AccessState::Available,
                signature: Some("public-moscow".into()),
            },
            searches: VecDeque::new(),
            search_args: vec![],
            products: VecDeque::new(),
            reviews: VecDeque::new(),
            offer_pages: VecDeque::new(),
            images: VecDeque::new(),
            entered: Arc::new(tokio::sync::Notify::new()),
            targets: vec![],
            product_options: vec![],
            review_options: vec![],
        }
    }
    async fn context(&mut self, cancel: &CancellationToken) -> Result<ContextObservation> {
        if cancel.is_cancelled() {
            return Err(fail(Code::Cancelled, "Fixture context cancelled"));
        }
        if let Some(context) = self.contexts.pop_front() {
            self.last_context = context;
        }
        Ok(self.last_context.clone())
    }
}
#[cfg(test)]
async fn run_fake<T>(
    action: Option<FakeAction<T>>,
    cancel: &CancellationToken,
    entered: &tokio::sync::Notify,
) -> Result<T> {
    entered.notify_one();
    match action {
        Some(FakeAction::Observation(value)) => Ok(value),
        Some(FakeAction::Failure(code)) => Err(fail(code, "Fixture retrieval failed")),
        Some(FakeAction::Cancelled {
            cleanup_entered,
            cleanup_release,
        }) => {
            cancel.cancelled().await;
            cleanup_entered.notify_one();
            cleanup_release.notified().await;
            Err(fail(Code::Cancelled, "Fixture cleanup completed"))
        }
        None => Err(fail(
            Code::SourceChanged,
            "Fixture retrieval script exhausted",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ozon::model::*;
    use std::os::unix::fs::PermissionsExt;
    fn coverage(rows: usize) -> SourceCoverage {
        SourceCoverage {
            present: true,
            raw_rows: rows,
            parsed_rows: rows,
            malformed_rows: 0,
            source_truncated: false,
            local_truncated: false,
        }
    }
    fn acquisition(url: &str) -> Option<Acquisition> {
        Some(Acquisition {
            source_url: url.into(),
            observed_at: "2026-09-30T00:00:00Z".into(),
            method: CaptureMethod::Composer,
        })
    }
    fn search_page(skus: &[&str], next: Option<&str>) -> SearchResponse {
        SearchResponse {
            acquisition: acquisition("https://www.ozon.ru/search/?text=fixture"),
            items: skus
                .iter()
                .enumerate()
                .map(|(index, sku)| SearchItem {
                    source_index: index,
                    source_locator: format!("/widgetStates/tileGridDesktop-fixture/items/{index}"),
                    sku: (*sku).into(),
                    name: Some(format!("Product {sku}")),
                    price_minor: Some(123450),
                    price_type: PriceType::Unknown,
                    price_label: Some("Displayed price".into()),
                    delivery_label: Some("Завтра".into()),
                    seller: Some("Seller".into()),
                    rating: Some(4.5),
                    reviews: Some(12),
                    url: Some(format!("https://www.ozon.ru/product/{sku}/")),
                    image: Some("https://ir.ozone.ru/s3/multimedia-test/a.jpg".into()),
                    matches_price_range: None,
                })
                .collect(),
            search_url: "https://www.ozon.ru/search/?text=fixture".into(),
            next_path: next.map(str::to_owned),
            source: coverage(skus.len()),
            refinements_source_truncated: false,
            has_next: Some(next.is_some()),
            total: Some(skus.len() as u64),
            warnings: vec![],
            facets: None,
            sort_options: None,
        }
    }
    fn product(sku: &str) -> ProductDetails {
        ProductDetails {
            acquisition: acquisition(&format!("https://www.ozon.ru/product/{sku}/")),
            sku: Some(sku.into()),
            name: Some(format!("Product {sku}")),
            url: Some(format!("https://www.ozon.ru/product/{sku}/")),
            displayed_price_minor: Some(120000),
            card_price_minor: Some(110000),
            regular_price_minor: Some(120000),
            duty: None,
            available: Some(true),
            rating: Some(4.5),
            reviews: Some(12),
            seller: None,
            delivery_label: Some("Завтра".into()),
            image_origins: vec![SourceImage {
                source_locator: Some("/widgetStates/webGallery-fixture/images/3".into()),
                url: "https://ir.ozone.ru/a.jpg".into(),
                section: "gallery/images".into(),
                index: 3,
            }],
            gallery_source: coverage(1),
            supplement: None,
            characteristics: vec![
                Characteristic {
                    source_index: 0,
                    source_locator:
                        "/widgetStates/webShortCharacteristics-fixture/characteristics/0".into(),
                    label: "Размер".into(),
                    value: "XL".into(),
                },
                Characteristic {
                    source_index: 1,
                    source_locator:
                        "/widgetStates/webShortCharacteristics-fixture/characteristics/1".into(),
                    label: "Размер".into(),
                    value: "52".into(),
                },
            ],
            characteristics_complete: true,
            description: Description {
                acquisition: None,
                text: "Описание".into(),
                source: coverage(0),
                image_origins: vec![],
            },
            variants: ProductVariants {
                status: SourceSectionStatus::Available,
                items: vec![ProductVariant {
                    sku: "202".into(),
                    title: Some("Синий".into()),
                    url: None,
                    displayed_price_minor: None,
                    available: None,
                }],
                has_next: Some(false),
                next_path: None,
            },
            warnings: vec![],
            full_characteristics: None,
            offers: None,
            read_usage: Default::default(),
        }
    }
    fn review(id: Option<&str>, comment: &str) -> Review {
        Review {
            source_index: 0,
            source_locator: "/widgetStates/webListReviews-fixture/reviews/0".into(),
            photo_origins: vec![],
            review_id: id.map(str::to_owned),
            author: None,
            score: Some(5.0),
            comment: Some(comment.into()),
            pros: None,
            cons: None,
            date: Some("2026-09-30".into()),
            purchased: Some(true),
            variant_label: Some("XL".into()),
            details: None,
        }
    }
    fn review_page(mut reviews: Vec<Review>, path: &str, next: Option<&str>) -> ReviewPage {
        for (index, review) in reviews.iter_mut().enumerate() {
            review.source_index = index;
            review.source_locator = format!("/widgetStates/webListReviews-fixture/reviews/{index}");
        }
        let count = reviews.len();
        ReviewPage {
            acquisition: acquisition(&format!("https://www.ozon.ru{path}")),
            source: coverage(count),
            source_url: Some(format!("https://www.ozon.ru{path}")),
            observed_at: Some("2026-09-30T00:00:00Z".into()),
            rating: Some(4.5),
            total_reviews: Some(100),
            reviews,
            next_path: next.map(str::to_owned),
            has_next: Some(next.is_some()),
            refinements: vec![],
            aggregation_scope: ReviewAggregationScope::MultipleVariants,
            warnings: vec![],
            star_counts: None,
            read_usage: Default::default(),
        }
    }
    fn fetched_image() -> images::FetchedImage {
        images::FetchedImage {
            data: "AA==".into(),
            mime_type: "image/png".into(),
            width: 1,
            height: 1,
            sha256: "a".repeat(64),
            retrieved_at: "2026-09-30T00:01:00Z".into(),
        }
    }
    async fn fixture(fake: FakeSource) -> (Application, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("oz-app-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(dir.path().to_owned()).unwrap();
        let application = Application::new(&config).await.unwrap();
        *application.inner.source.lock().await = Some(SourceBackend::Fake(Box::new(fake)));
        (application, dir)
    }
    async fn call(application: &Application, name: &str, args: Value) -> Value {
        let reply = application.call(name, args, CancellationToken::new()).await;
        assert!(!reply.is_error(), "{:?}", reply.failure());
        let value = reply.structured().unwrap().clone();
        contracts::validate_output(name, &value).unwrap();
        assert_eq!(value["evidence"], json!([]));
        value
    }
    async fn failed(application: &Application, name: &str, args: Value, code: &str) {
        let reply = application.call(name, args, CancellationToken::new()).await;
        assert_eq!(reply.failure().unwrap()["error"]["code"], code);
    }

    #[tokio::test]
    async fn all_eight_public_tools_share_validated_refs_and_local_expansion() {
        let mut fake = FakeSource::new();
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["101"], None)));
        fake.products
            .push_back(FakeAction::Observation(product("101")));
        fake.reviews.push_back(FakeAction::Observation(review_page(
            vec![review(Some("r1"), "Полный отзыв")],
            "/product/101/?tab=reviews",
            None,
        )));
        fake.images
            .push_back(FakeAction::Observation(fetched_image()));
        let (application, _dir) = fixture(fake).await;
        let context = call(&application, "ozon_get_context", json!({})).await;
        assert!(context["researchId"].is_null());
        assert_eq!(context["data"]["region"]["evidenceRefs"], json!([]));
        assert_eq!(
            context["data"]["capabilities"].as_array().unwrap().len(),
            17
        );
        assert_eq!(context["data"]["capabilities"][8]["status"], "supported");
        let search = call(
            &application,
            "ozon_search",
            json!({"start":{"query":"fixture"}}),
        )
        .await;
        let rid = search["researchId"].as_str().unwrap();
        let candidate = &search["data"]["items"][0];
        let pref = candidate["productRef"].as_str().unwrap();
        let eid = candidate["evidenceRefs"][0].as_str().unwrap();
        let image_ref = candidate["imageRefs"][0].as_str().unwrap();
        let details=call(&application,"ozon_get_products",json!({"researchId":rid,"products":[{"productRef":pref}],"include":["characteristics","description","variants","images"]})).await;
        assert_eq!(
            details["data"]["results"][0]["product"]["characteristics"]["items"][0]["name"],
            "Размер"
        );
        assert_eq!(
            details["data"]["results"][0]["product"]["characteristics"]["items"][1]["value"],
            "52"
        );
        assert_eq!(
            details["data"]["results"][0]["product"]["prices"][0]["type"],
            "ozon_card"
        );
        let reviews = call(
            &application,
            "ozon_get_reviews",
            json!({"start":{"productRef":pref}}),
        )
        .await;
        assert_eq!(reviews["data"]["reviews"][0]["text"], "Полный отзыв");
        assert_eq!(reviews["data"]["aggregationScope"], "multiple_variants");
        let images = application
            .call(
                "ozon_get_images",
                json!({"imageRefs":["missing",image_ref]}),
                CancellationToken::new(),
            )
            .await;
        let value = images.structured().unwrap();
        contracts::validate_output("ozon_get_images", value).unwrap();
        assert_eq!(value["data"]["results"][0]["status"], "error");
        assert_eq!(value["data"]["results"][1]["contentIndex"], 1);
        let mcp = images.into_mcp().unwrap();
        assert_eq!(mcp.content.len(), 2);
        let note_args = json!({"researchId":rid,"operationId":"op-1","kind":"assessment","text":"Проверить размер","productRefs":[pref],"evidenceRefs":[eid]});
        let note = call(&application, "ozon_append_research_note", note_args.clone()).await;
        let retry = call(&application, "ozon_append_research_note", note_args.clone()).await;
        assert_eq!(note["data"], retry["data"]);
        let mut changed = note_args;
        changed["text"] = json!("Другое");
        failed(
            &application,
            "ozon_append_research_note",
            changed,
            "CONFLICT",
        )
        .await;
        let listing = call(&application, "ozon_list_research", json!({})).await;
        assert_eq!(listing["data"]["researches"].as_array().unwrap().len(), 1);
        let evidence = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"evidence","evidenceRefs":[eid]}),
        )
        .await;
        assert!(
            !evidence["data"]["payload"][0]["facts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let snapshot = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"candidates","productRefs":[pref]}),
        )
        .await;
        assert_eq!(snapshot["data"]["payload"][0]["title"], "Product 101");
        assert_eq!(
            snapshot["data"]["payload"][0]["observedAt"],
            "2026-09-30T00:00:00.000Z"
        );
        let notes = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"notes","noteIds":[note["data"]["noteId"]]}),
        )
        .await;
        assert_eq!(notes["data"]["payload"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn captured_search_remainder_is_local_bound_and_replayable() {
        let mut fake = FakeSource::new();
        fake.searches.push_back(FakeAction::Observation(search_page(
            &["1", "2", "3", "4"],
            None,
        )));
        let (application, _dir) = fixture(fake).await;
        let first = call(
            &application,
            "ozon_search",
            serde_json::from_str(r#"{"start":{"query":"fixture","priceRange":{"minMinor":1e5,"maxMinor":123000.0}},"limit":2.0}"#).unwrap(),
        )
        .await;
        assert_eq!(first["data"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(
            first["data"]["items"][0]["matchesDisplayedPriceRange"],
            false
        );
        let events = call(
            &application,
            "ozon_get_research",
            json!({"researchId":first["researchId"],"section":"events"}),
        )
        .await;
        let summary = events["data"]["payload"][0]["summary"].as_str().unwrap();
        assert!(summary.contains("fixture") && summary.contains("100000..123000"));
        let cursor = first["data"]["nextCursor"].as_str().unwrap();
        failed(
            &application,
            "ozon_search",
            json!({"start":{"cursor":cursor},"limit":3}),
            "INVALID_REFERENCE",
        )
        .await;
        failed(
            &application,
            "ozon_search",
            json!({"researchId":"foreign","start":{"cursor":cursor}}),
            "INVALID_REFERENCE",
        )
        .await;
        let next = call(
            &application,
            "ozon_search",
            json!({"start":{"cursor":cursor}}),
        )
        .await;
        assert_eq!(next["data"]["items"][0]["sku"], "3");
        assert_eq!(next["data"]["coverage"]["uniqueSeen"], 4);
        assert_eq!(next["data"]["coverage"]["completeness"], "complete");
        assert!(next["data"]["nextCursor"].is_null());
        let replay = call(
            &application,
            "ozon_search",
            json!({"start":{"cursor":cursor}}),
        )
        .await;
        assert_eq!(replay["data"]["items"][1]["sku"], "4");
        assert_eq!(replay["data"]["coverage"], next["data"]["coverage"]);
    }

    #[tokio::test]
    async fn observed_refinement_keeps_its_state_and_durable_search_criteria() {
        let mut fake = FakeSource::new();
        let mut first = search_page(&["101"], None);
        first.sort_options = Some(vec![SortOption {
            label: Some("Sort without bounds".into()),
            selected: false,
            search_url: Some("https://www.ozon.ru/search/?text=fixture&sorting=price".into()),
        }]);
        fake.searches.push_back(FakeAction::Observation(first));
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["202"], None)));
        let (application, _dir) = fixture(fake).await;
        for input in [
            r#"{"start":{"query":"fixture","priceRange":{"minMinor":9007199254740992}}}"#,
            r#"{"start":{"query":"fixture","priceRange":{"minMinor":2.5}}}"#,
            r#"{"start":{"query":"fixture","priceRange":{"minMinor":200.0,"maxMinor":1e2}}}"#,
        ] {
            failed(
                &application,
                "ozon_search",
                serde_json::from_str(input).unwrap(),
                "INVALID_ARGUMENT",
            )
            .await;
        }
        let first = call(&application,"ozon_search",serde_json::from_str(r#"{"start":{"query":"fixture","priceRange":{"minMinor":1e5}},"includeFacets":true,"refinementLimit":2.0}"#).unwrap()).await;
        let rid = first["researchId"].as_str().unwrap();
        let search_ref = first["data"]["refinements"][0]["searchRef"].clone();
        let next = call(
            &application,
            "ozon_search",
            json!({"start":{"searchRef":search_ref}}),
        )
        .await;
        assert!(next["data"].get("priceRangeAssessment").is_none());
        {
            let guard = application.inner.source.lock().await;
            let Some(SourceBackend::Fake(fake)) = guard.as_ref() else {
                panic!("fake source")
            };
            assert_eq!(fake.search_args.len(), 2);
            let args = &fake.search_args[1];
            assert_eq!(
                args.search_url.as_deref(),
                Some("https://www.ozon.ru/search/?text=fixture&sorting=price")
            );
            assert!(args.price_min.is_none() && args.price_max.is_none());
        }
        let events = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"events"}),
        )
        .await;
        let summary = events["data"]["payload"][1]["summary"].as_str().unwrap();
        assert!(summary.contains("fixture") && summary.contains("Sort without bounds"));
        assert!(!summary.contains("minor-unit bounds"));
    }

    #[tokio::test]
    async fn whitespace_description_stays_unknown_and_keeps_independent_images() {
        for annotation in ["   ", " <img src=\"https://ir.ozone.ru/a.jpg\"> "] {
            let base = json!({
                "seo": {"link": [{"href": "/product/example-901/"}]},
                "widgetStates": {"webProductHeading-a": {"title": "Example"},"webDescription-a": {"richAnnotation": annotation}}
            });
            let supplement = json!({
                "seo": {"link": [{"href": "/product/example-901/"}]},
                "widgetStates": {"webDescription-b": {"richAnnotation": "   "}}
            });
            crate::ozon::decode::validate_product_identity(&base, "901", true).unwrap();
            crate::ozon::decode::validate_product_identity(&supplement, "901", false).unwrap();
            let mut raw = crate::ozon::decode::parse_details(&base, Some(&supplement));
            raw.acquisition = acquisition("https://www.ozon.ru/product/example-901/");
            let expected_images = raw
                .image_origins
                .iter()
                .map(|image| &image.url)
                .collect::<BTreeSet<_>>()
                .len();
            let mut fake = FakeSource::new();
            fake.products.push_back(FakeAction::Observation(raw));
            let (application, _dir) = fixture(fake).await;
            let result = call(
                &application,
                "ozon_get_products",
                json!({"products":[{"sku":"901"}],"include":["description","images"]}),
            )
            .await;
            let product = &result["data"]["results"][0]["product"];
            assert_eq!(product["description"]["status"], "unknown");
            assert!(product["description"]["text"].is_null());
            assert!(product["description"]["hasNext"].is_null());
            assert_eq!(
                product["images"]["items"].as_array().unwrap().len(),
                expected_images
            );
            if expected_images > 0 {
                let image = journal(&application.inner)
                    .unwrap()
                    .get_ref(
                        product["images"]["items"][0]["imageRef"].as_str().unwrap(),
                        "image",
                    )
                    .unwrap();
                assert!(
                    image.value["fieldPath"]
                        .as_str()
                        .unwrap()
                        .contains("webDescription-a/richAnnotation")
                );
            }
        }
    }

    #[tokio::test]
    async fn mixed_product_errors_preserve_order_and_url_null_variant_uses_sku() {
        let mut fake = FakeSource::new();
        let mut populated = product("101");
        populated.variants.items[0].displayed_price_minor = Some(99000);
        populated.variants.items[0].available = Some(true);
        fake.products.push_back(FakeAction::Observation(populated));
        let mut malformed = product("666");
        malformed.name = Some("x".repeat(1001));
        fake.products.push_back(FakeAction::Observation(malformed));
        fake.products
            .push_back(FakeAction::Observation(product("202")));
        let (application, _dir) = fixture(fake).await;
        let initial=call(&application,"ozon_get_products",json!({"products":[{"productRef":"missing"},{"sku":"101"},{"sku":"666"}],"include":["variants"]})).await;
        let rows = initial["data"]["results"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r["status"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["error", "ok", "error"]
        );
        let rid = initial["researchId"].as_str().unwrap();
        let variant_row = &rows[1]["product"]["variants"]["items"][0];
        assert_eq!(variant_row["prices"][0]["amountMinor"], 99000);
        assert_eq!(variant_row["prices"][0]["type"], "unknown");
        assert_eq!(variant_row["availability"], "available");
        let variant = rows[1]["product"]["variants"]["items"][0]["productRef"]
            .as_str()
            .unwrap();
        let note=call(&application,"ozon_append_research_note",json!({"researchId":rid,"operationId":"variant-note","kind":"assessment","text":"Variant ref is authoritative","productRefs":[variant]})).await;
        assert!(note["data"]["noteId"].is_string());
        let lookup = call(
            &application,
            "ozon_get_products",
            json!({"products":[{"productRef":variant}],"include":[]}),
        )
        .await;
        assert_eq!(lookup["data"]["results"][0]["product"]["sku"], "202");
        let guard = application.inner.source.lock().await;
        if let Some(SourceBackend::Fake(fake)) = guard.as_ref() {
            assert_eq!(fake.targets, vec!["101", "666", "202"]);
        } else {
            panic!("fixture source");
        }
        drop(guard);
        let summary = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"summary"}),
        )
        .await;
        assert_eq!(summary["data"]["payload"]["productCount"], 2);
    }

    #[tokio::test]
    async fn review_remainders_identity_branching_and_source_loss_stay_truthful() {
        let mut fake = FakeSource::new();
        fake.products
            .push_back(FakeAction::Observation(product("101")));
        fake.reviews.push_back(FakeAction::Observation(review_page(
            vec![
                review(Some("a"), "A"),
                review(Some("b"), "B"),
                review(None, "same"),
                review(None, "same"),
            ],
            "/product/101/?tab=reviews",
            Some("/product/101/?tab=reviews&page=2"),
        )));
        let mut last = review_page(
            vec![review(Some("b"), "B"), review(Some("c"), "C")],
            "/product/101/?tab=reviews&page=2",
            None,
        );
        last.source.malformed_rows = 1;
        fake.reviews.push_back(FakeAction::Observation(last));
        let (application, _dir) = fixture(fake).await;
        let product = call(
            &application,
            "ozon_get_products",
            json!({"products":[{"sku":"101"}],"include":[]}),
        )
        .await;
        let pref = product["data"]["results"][0]["product"]["productRef"]
            .as_str()
            .unwrap();
        let first = call(
            &application,
            "ozon_get_reviews",
            json!({"start":{"productRef":pref},"limit":2}),
        )
        .await;
        let cursor = first["data"]["nextCursor"].as_str().unwrap();
        failed(
            &application,
            "ozon_get_reviews",
            json!({"start":{"cursor":cursor},"limit":1}),
            "INVALID_REFERENCE",
        )
        .await;
        let remainder = call(
            &application,
            "ozon_get_reviews",
            json!({"start":{"cursor":cursor}}),
        )
        .await;
        assert_eq!(remainder["data"]["reviews"].as_array().unwrap().len(), 2);
        assert_eq!(remainder["data"]["coverage"]["uniqueSeen"], 4);
        let replay = call(
            &application,
            "ozon_get_reviews",
            json!({"start":{"cursor":cursor}}),
        )
        .await;
        assert_eq!(replay["data"]["coverage"], remainder["data"]["coverage"]);
        let final_page = call(
            &application,
            "ozon_get_reviews",
            json!({"start":{"cursor":remainder["data"]["nextCursor"]}}),
        )
        .await;
        assert_eq!(final_page["data"]["reviews"].as_array().unwrap().len(), 1);
        assert_eq!(final_page["data"]["coverage"]["uniqueSeen"], 5);
        assert_eq!(final_page["data"]["coverage"]["completeness"], "partial");
    }

    #[tokio::test]
    async fn output_budget_and_context_change_roll_back_entire_new_research() {
        let mut fake = FakeSource::new();
        let skus = (1..=36).map(|n| n.to_string()).collect::<Vec<_>>();
        let references = skus.iter().map(String::as_str).collect::<Vec<_>>();
        let mut huge = search_page(&references, None);
        for item in &mut huge.items {
            item.name = Some("x".repeat(1000));
            item.delivery_label = Some("y".repeat(1000));
        }
        fake.searches.push_back(FakeAction::Observation(huge));
        let (application, _dir) = fixture(fake).await;
        failed(
            &application,
            "ozon_search",
            json!({"start":{"query":"fixture"},"limit":36}),
            "RESULT_TOO_LARGE",
        )
        .await;
        let list = call(&application, "ozon_list_research", json!({})).await;
        assert_eq!(list["data"]["researches"], json!([]));
        let mut fake = FakeSource::new();
        let before = fake.last_context.clone();
        let mut after = before.clone();
        after.region_label = Some("Казань".into());
        after.signature = Some("public-kazan".into());
        fake.contexts = VecDeque::from([before, after]);
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["1"], None)));
        let (application, _dir) = fixture(fake).await;
        failed(
            &application,
            "ozon_search",
            json!({"start":{"query":"fixture"}}),
            "CONTEXT_CHANGED",
        )
        .await;
        assert_eq!(
            call(&application, "ozon_list_research", json!({})).await["data"]["researches"],
            json!([])
        );
    }

    #[tokio::test]
    async fn cancellation_keeps_admission_and_source_owned_until_cleanup() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let cleanup_entered = Arc::new(tokio::sync::Notify::new());
        let cleanup_release = Arc::new(tokio::sync::Notify::new());
        let mut fake = FakeSource::new();
        fake.entered = entered.clone();
        fake.searches.push_back(FakeAction::Cancelled {
            cleanup_entered: cleanup_entered.clone(),
            cleanup_release: cleanup_release.clone(),
        });
        let (application, _dir) = fixture(fake).await;
        let cancel = CancellationToken::new();
        let task = {
            let application = application.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                application
                    .call("ozon_search", json!({"start":{"query":"fixture"}}), cancel)
                    .await
            })
        };
        entered.notified().await;
        cancel.cancel();
        let reply = task.await.unwrap();
        assert_eq!(reply.failure().unwrap()["error"]["code"], "CANCELLED");
        cleanup_entered.notified().await;
        assert_eq!(application.inner.admission.available_permits(), 7);
        assert!(application.inner.source.try_lock().is_err());
        assert_eq!(
            journal(&application.inner)
                .unwrap()
                .list(&json!({}))
                .unwrap()["researches"],
            json!([])
        );
        cleanup_release.notify_one();
        application.shutdown().await.unwrap();
        assert_eq!(application.inner.admission.available_permits(), 8);
    }

    #[tokio::test]
    async fn large_product_batch_uses_section_cursors_without_losing_rows() {
        let mut fake = FakeSource::new();
        let mut selectors = Vec::new();
        for sku in 101..=108 {
            let mut card = product(&sku.to_string());
            card.description.text = "т".repeat(12000);
            card.read_usage.source_reads = 1;
            fake.products.push_back(FakeAction::Observation(card));
            selectors.push(json!({"sku":sku.to_string()}));
        }
        let (application, _dir) = fixture(fake).await;
        let result = call(
            &application,
            "ozon_get_products",
            json!({"products":selectors,"include":["description"]}),
        )
        .await;
        let rid = result["researchId"].as_str().unwrap();
        let rows = result["data"]["results"].as_array().unwrap();
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|r| r["status"] == "ok"));
        let cursor = rows
            .iter()
            .find_map(|r| r["product"]["description"]["nextCursor"].as_str())
            .expect("large batch has a continuation");
        let next = call(
            &application,
            "ozon_get_products",
            json!({"researchId":rid,"products":[{"cursor":cursor}]}),
        )
        .await;
        assert_eq!(
            next["data"]["results"][0]["product"]["description"]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            6000
        );
        application.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn large_characteristics_cursor_makes_progress_and_preserves_source_truncation() {
        let mut fake = FakeSource::new();
        let mut card = product("991");
        let rows = (0..40)
            .map(|i| Characteristic {
                source_index: i,
                source_locator: format!("/full/{i}"),
                label: format!("Field {i}"),
                value: "x".repeat(2000),
            })
            .collect();
        let mut source = coverage(40);
        source.local_truncated = true;
        card.full_characteristics = Some(FullCharacteristics {
            items: rows,
            source,
            acquisition: acquisition("https://www.ozon.ru/product/991/"),
        });
        card.read_usage.source_reads = 1;
        fake.products.push_back(FakeAction::Observation(card));
        let (app, _dir) = fixture(fake).await;
        let first = call(
            &app,
            "ozon_get_products",
            json!({"products":[{"sku":"991"}],"include":["fullCharacteristics"]}),
        )
        .await;
        let rid = first["researchId"].as_str().unwrap();
        let mut section = first["data"]["results"][0]["product"]["fullCharacteristics"].clone();
        let mut count = 0;
        loop {
            count += section["items"].as_array().unwrap().len();
            assert_eq!(section["truncated"], true);
            let Some(cursor) = section["nextCursor"].as_str() else {
                break;
            };
            let next = call(
                &app,
                "ozon_get_products",
                json!({"researchId":rid,"products":[{"cursor":cursor}]}),
            )
            .await;
            section = next["data"]["results"][0]["product"]["fullCharacteristics"].clone();
            assert!(!section["items"].as_array().unwrap().is_empty());
            assert!(count < 40);
        }
        assert_eq!(count, 40);
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn large_offer_rows_page_by_measured_size() {
        let mut fake = FakeSource::new();
        let mut card = product("994");
        let offers = (0..20)
            .map(|i| ProductOffer {
                sku: (2000 + i).to_string(),
                seller: Seller {
                    name: "😀".repeat(500),
                    rating: None,
                    url: None,
                },
                card_price_minor: None,
                regular_price_minor: Some(100),
                available: None,
                delivery_label: Some("😀".repeat(1000)),
                url: None,
            })
            .collect();
        card.offers = Some(ProductOffers {
            items: offers,
            source: coverage(20),
            acquisition: acquisition("https://www.ozon.ru/product/994/"),
            has_next: Some(false),
            next_path: None,
        });
        fake.products.push_back(FakeAction::Observation(card));
        let (app, _dir) = fixture(fake).await;
        let first = call(
            &app,
            "ozon_get_products",
            json!({"products":[{"sku":"994"}],"include":["offers"]}),
        )
        .await;
        let rid = first["researchId"].as_str().unwrap();
        assert_eq!(
            first["data"]["results"][0]["status"], "ok",
            "{}",
            first["data"]["results"][0]
        );
        let mut section = first["data"]["results"][0]["product"]["offers"].clone();
        let mut total = 0;
        loop {
            let count = section["items"].as_array().unwrap().len();
            assert!(count > 0);
            total += count;
            let Some(cursor) = section["nextCursor"].as_str() else {
                break;
            };
            let next = call(
                &app,
                "ozon_get_products",
                json!({"researchId":rid,"products":[{"cursor":cursor}]}),
            )
            .await;
            section = next["data"]["results"][0]["product"]["offers"].clone();
            assert!(total < 20);
        }
        assert_eq!(total, 20);
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reply_metadata_is_included_in_page_budget() {
        let mut fake = FakeSource::new();
        fake.products
            .push_back(FakeAction::Observation(product("995")));
        let reviews = (0..7)
            .map(|i| {
                let mut item = review(Some(&format!("{i}")), "x");
                item.details = Some(ReviewDetails {
                    helpful: None,
                    unhelpful: None,
                    item_sku: None,
                    seller_replies: (0..9)
                        .map(|_| SellerReply {
                            text: "x".into(),
                            published_at: None,
                            author_label: Some("a".repeat(1000)),
                        })
                        .collect(),
                    replies_status: SourceSectionStatus::Available,
                    replies_acquisition: acquisition(
                        "https://www.ozon.ru/api/composer-api.bx/_action/rpGetCommentsByReviewUuid",
                    ),
                });
                item
            })
            .collect();
        let mut page = review_page(reviews, "/product/995/?tab=reviews", None);
        page.star_counts = Some(vec![StarCount {
            stars: 5,
            count: 42,
        }]);
        fake.reviews.push_back(FakeAction::Observation(page));
        let (app, _dir) = fixture(fake).await;
        let card = call(
            &app,
            "ozon_get_products",
            json!({"products":[{"sku":"995"}]}),
        )
        .await;
        let rid = card["researchId"].as_str().unwrap();
        let pref = card["data"]["results"][0]["product"]["productRef"]
            .as_str()
            .unwrap();
        let result = call(&app, "ozon_get_reviews", json!({"researchId":rid,"start":{"productRef":pref},"includeSellerReplies":true,"maxSourceReads":8,"textLimit":80})).await;
        assert_eq!(result["data"]["reviews"].as_array().unwrap().len(), 7);
        assert_eq!(result["data"]["aggregate"]["starCounts"][0]["count"], 42);
        assert!(
            result["data"]["reviews"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["sellerReplies"]["truncated"] == true)
        );
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn later_search_candidate_keeps_unselected_requirement_for_local_comparison() {
        let mut fake = FakeSource::new();
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["941"], None)));
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["942"], None)));
        let mut first = product("941");
        first.characteristics[0].label = "Материал".into();
        first.characteristics[0].value = "пластик".into();
        first.characteristics.truncate(1);
        let mut second = product("942");
        second.characteristics[0].label = "Материал".into();
        second.characteristics[0].value = "сталь".into();
        second.characteristics.truncate(1);
        second.card_price_minor = Some(90000);
        fake.products.push_back(FakeAction::Observation(first));
        fake.products.push_back(FakeAction::Observation(second));
        let (app, _dir) = fixture(fake).await;
        let search1 = call(&app, "ozon_search", json!({"start":{"query":"first"}})).await;
        let rid = search1["researchId"].as_str().unwrap();
        let search2 = call(
            &app,
            "ozon_search",
            json!({"researchId":rid,"start":{"query":"later"}}),
        )
        .await;
        let a = search1["data"]["items"][0]["productRef"].as_str().unwrap();
        let b = search2["data"]["items"][0]["productRef"].as_str().unwrap();
        let cards = call(&app, "ozon_get_products", json!({"researchId":rid,"products":[{"productRef":a},{"productRef":b}],"include":["characteristics"],"characteristicNames":["Другой"]})).await;
        let a = cards["data"]["results"][0]["product"]["productRef"]
            .as_str()
            .unwrap();
        let b = cards["data"]["results"][1]["product"]["productRef"]
            .as_str()
            .unwrap();
        let result = call(&app, "ozon_compare_products", json!({"researchId":rid,"productRefs":[a,b],"sortBy":"price","requirements":[{"name":"Материал","equals":"сталь"},{"name":"Отсутствует","equals":"да"}]})).await;
        let rows = result["data"]["rows"].as_array().unwrap();
        assert_eq!(rows[0]["sku"], "942");
        assert_eq!(rows[0]["requirements"][0]["status"], "matches");
        assert_eq!(rows[1]["requirements"][0]["status"], "does_not_match");
        assert_eq!(rows[0]["requirements"][1]["status"], "unknown");
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observed_offers_continuation_reads_once_and_stops_on_cycle() {
        let path = "/modal/otherOffersFromSellers?product_id=993&sort=price&page=2";
        let mut fake = FakeSource::new();
        let mut card = product("993");
        let offers = ProductOffers {
            items: vec![],
            source: coverage(0),
            acquisition: acquisition("https://www.ozon.ru/product/993/"),
            has_next: Some(true),
            next_path: Some(path.into()),
        };
        card.offers = Some(offers.clone());
        card.read_usage.source_reads = 1;
        fake.products.push_back(FakeAction::Observation(card));
        fake.offer_pages.push_back(FakeAction::Observation((
            offers,
            ReadUsage {
                source_reads: 1,
                budget_exhausted: false,
            },
        )));
        let (app, _dir) = fixture(fake).await;
        let first = call(
            &app,
            "ozon_get_products",
            json!({"products":[{"sku":"993"}],"include":["offers"]}),
        )
        .await;
        let rid = first["researchId"].as_str().unwrap();
        let cursor = first["data"]["results"][0]["product"]["offers"]["nextCursor"]
            .as_str()
            .unwrap();
        let next = call(
            &app,
            "ozon_get_products",
            json!({"researchId":rid,"products":[{"cursor":cursor}]}),
        )
        .await;
        assert_eq!(next["data"]["usage"]["sourceReads"], 1);
        assert!(next["data"]["results"][0]["product"]["offers"]["nextCursor"].is_null());
        assert!(
            next["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["code"] == "CONTINUATION_STALLED")
        );
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn long_seller_replies_are_explicitly_partial_and_review_stays_available() {
        let mut fake = FakeSource::new();
        fake.products
            .push_back(FakeAction::Observation(product("992")));
        let mut item = review(Some("id"), "review");
        item.details = Some(ReviewDetails {
            helpful: Some(2),
            unhelpful: None,
            item_sku: Some("992".into()),
            seller_replies: (0..9)
                .map(|_| SellerReply {
                    text: "x".repeat(7000),
                    published_at: None,
                    author_label: Some("Seller".into()),
                })
                .collect(),
            replies_status: SourceSectionStatus::Available,
            replies_acquisition: acquisition(
                "https://www.ozon.ru/api/composer-api.bx/_action/rpGetCommentsByReviewUuid",
            ),
        });
        fake.reviews.push_back(FakeAction::Observation(review_page(
            vec![item],
            "/product/992/?tab=reviews",
            None,
        )));
        let (app, _dir) = fixture(fake).await;
        let card = call(
            &app,
            "ozon_get_products",
            json!({"products":[{"sku":"992"}]}),
        )
        .await;
        let rid = card["researchId"].as_str().unwrap();
        let pref = card["data"]["results"][0]["product"]["productRef"]
            .as_str()
            .unwrap();
        let result = call(&app, "ozon_get_reviews", json!({"researchId":rid,"start":{"productRef":pref},"includeDetails":true,"includeSellerReplies":true,"textLimit":80})).await;
        let row = &result["data"]["reviews"][0];
        assert_eq!(row["text"], "review");
        assert_eq!(row["sellerReplies"]["truncated"], true);
        assert_eq!(row["sellerReplies"]["status"], "partial");
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn long_review_expands_locally_and_product_snapshot_preserves_unfiltered_characteristics()
    {
        let mut fake = FakeSource::new();
        let mut card = product("901");
        card.read_usage.source_reads = 1;
        fake.products.push_back(FakeAction::Observation(card));
        let long_text = "аб😀".repeat(9000);
        let mut page = review_page(
            vec![review(Some("long"), &long_text)],
            "/product/901/?tab=reviews",
            None,
        );
        page.read_usage.source_reads = 1;
        fake.reviews.push_back(FakeAction::Observation(page));
        let (application, _dir) = fixture(fake).await;
        let started = std::time::Instant::now();
        let products = call(&application, "ozon_get_products", json!({"products":[{"sku":"901"},{"url":"https://www.ozon.ru/product/901/"}],"include":["characteristics"],"characteristicNames":["Размер"],"maxSourceReads":1})).await;
        assert_eq!(products["data"]["usage"]["sourceReads"], 1);
        let rid = products["researchId"].as_str().unwrap();
        let product_ref = products["data"]["results"][0]["product"]["productRef"]
            .as_str()
            .unwrap();
        assert_eq!(products["data"]["results"][1]["status"], "ok");
        let second_ref = products["data"]["results"][1]["product"]["productRef"]
            .as_str()
            .unwrap();
        let before_compare = {
            let guard = application.inner.source.lock().await;
            match guard.as_ref().unwrap() {
                SourceBackend::Fake(fake) => fake.targets.len(),
                _ => unreachable!(),
            }
        };
        let compared = call(
            &application,
            "ozon_compare_products",
            json!({"researchId":rid,"productRefs":[product_ref,second_ref]}),
        )
        .await;
        let after_compare = {
            let guard = application.inner.source.lock().await;
            match guard.as_ref().unwrap() {
                SourceBackend::Fake(fake) => fake.targets.len(),
                _ => unreachable!(),
            }
        };
        assert_eq!(before_compare, after_compare);
        assert_eq!(compared["data"]["historical"], true);
        let snapshot = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"products","productRefs":[product_ref]}),
        )
        .await;
        assert_eq!(
            snapshot["data"]["payload"][0]["characteristics"]["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let reviews = call(
            &application,
            "ozon_get_reviews",
            json!({"researchId":rid,"start":{"productRef":product_ref},"textLimit":80}),
        )
        .await;
        let row = &reviews["data"]["reviews"][0];
        assert_eq!(row["textTruncated"], true);
        let reference = row["reviewRef"].as_str().unwrap();
        let mut restored = String::new();
        let mut offset = 0;
        loop {
            let result = call(&application, "ozon_get_research", json!({"researchId":rid,"section":"reviews","reviewRefs":[reference],"textOffset":offset,"textLimit":12000})).await;
            let part = &result["data"]["payload"][0];
            restored.push_str(part["text"].as_str().unwrap());
            match part["nextTextOffset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(restored, long_text);
        {
            let guard = application.inner.source.lock().await;
            let SourceBackend::Fake(fake) = guard.as_ref().unwrap() else {
                unreachable!()
            };
            assert_eq!(fake.targets.len(), 1);
            assert_eq!(fake.review_options.len(), 1);
            assert!(!fake.product_options[0].description && !fake.product_options[0].offers);
        }
        eprintln!(
            "FIXTURE duplicate_product_calls={before_compare} compare_extra_product_calls={} review_calls=1 product_response_utf16={} product_response_bytes={} review_response_utf16={} review_response_bytes={} elapsed_ms={}",
            after_compare - before_compare,
            response::serialized_utf16_len(&products).unwrap(),
            serde_json::to_vec(&products).unwrap().len(),
            response::serialized_utf16_len(&reviews).unwrap(),
            serde_json::to_vec(&reviews).unwrap().len(),
            started.elapsed().as_millis()
        );
        application.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn source_projection_fixture_reaches_public_reviews_without_loss_or_private_fields() {
        let golden: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/domain/source-projection.json"
        ))
        .unwrap();
        let mut raw = crate::ozon::decode::parse_reviews(&golden["projected"], 30);
        let expected: Vec<Review> = serde_json::from_value(golden["reviews"].clone()).unwrap();
        assert_eq!(raw.reviews, expected);
        raw.acquisition = acquisition("https://www.ozon.ru/product/901/reviews/");
        let mut fake = FakeSource::new();
        fake.products
            .push_back(FakeAction::Observation(product("901")));
        fake.reviews.push_back(FakeAction::Observation(raw));
        let (application, _dir) = fixture(fake).await;
        let product = call(
            &application,
            "ozon_get_products",
            json!({"products":[{"sku":"901"}],"include":[]}),
        )
        .await;
        let pref = &product["data"]["results"][0]["product"]["productRef"];
        let response = call(
            &application,
            "ozon_get_reviews",
            json!({"start":{"productRef":pref},"limit":30}),
        )
        .await;
        assert_eq!(response["data"]["reviews"].as_array().unwrap().len(), 3);
        assert_eq!(response["data"]["reviews"][0]["text"], "Works");
        assert_eq!(response["data"]["reviews"][1]["rating"], 5);
        assert_eq!(response["data"]["reviews"][2]["variantLabel"], "Синий");
        assert_eq!(
            response["data"]["reviews"][2]["imageRefs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(response["data"]["coverage"]["completeness"], "partial");
        assert!(!response.to_string().contains("PRIVATE_MARKER"));
    }

    #[tokio::test]
    async fn local_product_section_and_refinement_pages_keep_original_source_indices() {
        let mut fake = FakeSource::new();
        let mut detail = product("101");
        detail.characteristics = (0..51)
            .map(|index| Characteristic {
                source_index: index,
                source_locator: format!(
                    "/widgetStates/webShortCharacteristics-fixture/characteristics/{index}"
                ),
                label: "Повторяющееся имя".into(),
                value: index.to_string(),
            })
            .collect();
        fake.products.push_back(FakeAction::Observation(detail));
        let mut search = search_page(&["101"], None);
        search.sort_options = Some(
            (0..5)
                .map(|index| SortOption {
                    label: Some(format!("Sort {index}")),
                    selected: false,
                    search_url: Some(format!(
                        "https://www.ozon.ru/search/?text=fixture&sorting={index}"
                    )),
                })
                .collect(),
        );
        fake.searches.push_back(FakeAction::Observation(search));
        let (application, _dir) = fixture(fake).await;
        let initial = call(
            &application,
            "ozon_get_products",
            json!({"products":[{"sku":"101"}]}),
        )
        .await;
        let rid = initial["researchId"].as_str().unwrap();
        let cursor = initial["data"]["results"][0]["product"]["characteristics"]["nextCursor"]
            .as_str()
            .unwrap();
        assert_eq!(
            initial["data"]["results"][0]["product"]["characteristics"]["items"]
                .as_array()
                .unwrap()
                .len(),
            50
        );
        failed(
            &application,
            "ozon_get_products",
            json!({"products":[{"cursor":cursor}],"include":["characteristics"]}),
            "INVALID_ARGUMENT",
        )
        .await;
        let tail = call(
            &application,
            "ozon_get_products",
            json!({"products":[{"cursor":cursor}]}),
        )
        .await;
        let item = &tail["data"]["results"][0]["product"]["characteristics"]["items"][0];
        assert_eq!(item["value"], "50");
        let ev = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"evidence","evidenceRefs":[item["evidenceRefs"][0]]}),
        )
        .await;
        assert_eq!(
            ev["data"]["payload"][0]["facts"][0]["fieldPath"],
            "/widgetStates/webShortCharacteristics-fixture/characteristics/50/title"
        );
        let search=call(&application,"ozon_search",json!({"researchId":rid,"start":{"query":"fixture"},"includeFacets":true,"refinementLimit":2})).await;
        assert_eq!(search["data"]["refinements"].as_array().unwrap().len(), 2);
        let facets = call(
            &application,
            "ozon_search",
            json!({"start":{"refinementsCursor":search["data"]["refinementsNextCursor"]}}),
        )
        .await;
        assert_eq!(facets["data"]["items"], json!([]));
        assert_eq!(facets["data"]["refinements"][0]["label"], "Sort 2");
    }

    #[tokio::test]
    async fn image_confirmation_and_full_wire_budget_fail_before_any_observation_commit() {
        let mut fake = FakeSource::new();
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["101"], None)));
        fake.images
            .push_back(FakeAction::Observation(fetched_image()));
        let (application, _dir) = fixture(fake).await;
        let search = call(
            &application,
            "ozon_search",
            json!({"start":{"query":"fixture"}}),
        )
        .await;
        let rid = search["researchId"].as_str().unwrap();
        let reference = search["data"]["items"][0]["imageRefs"][0].clone();
        let summary = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"summary"}),
        )
        .await;
        {
            let mut guard = application.inner.source.lock().await;
            if let Some(SourceBackend::Fake(fake)) = guard.as_mut() {
                let before = fake.last_context.clone();
                let mut after = before.clone();
                after.signature = Some("changed-account".into());
                fake.contexts = VecDeque::from([before, after]);
            }
        }
        failed(
            &application,
            "ozon_get_images",
            json!({"imageRefs":[reference.clone()]}),
            "CONTEXT_CHANGED",
        )
        .await;
        let after = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"summary"}),
        )
        .await;
        assert_eq!(after["data"], summary["data"]);
        {
            let mut guard = application.inner.source.lock().await;
            if let Some(SourceBackend::Fake(fake)) = guard.as_mut() {
                fake.last_context.signature = Some("public-moscow".into());
                let mut image = fetched_image();
                image.data = "x".repeat(1_398_105);
                fake.images.push_back(FakeAction::Observation(image));
            }
        }
        failed(
            &application,
            "ozon_get_images",
            json!({"imageRefs":[reference]}),
            "RESULT_TOO_LARGE",
        )
        .await;
        let after = call(
            &application,
            "ozon_get_research",
            json!({"researchId":rid,"section":"summary"}),
        )
        .await;
        assert_eq!(after["data"], summary["data"]);
    }

    #[tokio::test]
    async fn source_failures_remain_ordered_and_eight_admitted_calls_include_waiters() {
        let mut fake = FakeSource::new();
        fake.products
            .push_back(FakeAction::Failure(Code::SourceBlocked));
        fake.products
            .push_back(FakeAction::Observation(product("102")));
        let (application, _dir) = fixture(fake).await;
        let response = call(
            &application,
            "ozon_get_products",
            json!({"products":[{"sku":"101"},{"sku":"102"}],"include":[]}),
        )
        .await;
        assert_eq!(
            response["data"]["results"][0]["error"]["code"],
            "SOURCE_BLOCKED"
        );
        assert_eq!(response["data"]["results"][1]["product"]["sku"], "102");
        let held = application.inner.source.lock().await;
        let mut cancellations = vec![];
        let mut calls = vec![];
        for _ in 0..8 {
            let cancel = CancellationToken::new();
            cancellations.push(cancel.clone());
            let application = application.clone();
            calls.push(tokio::spawn(async move {
                application
                    .call("ozon_get_context", json!({}), cancel)
                    .await
            }));
        }
        for _ in 0..100 {
            if application.inner.admission.available_permits() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(application.inner.admission.available_permits(), 0);
        failed(&application, "ozon_get_context", json!({}), "SERVER_BUSY").await;
        for cancel in cancellations {
            cancel.cancel();
        }
        for call in calls {
            assert_eq!(
                call.await.unwrap().failure().unwrap()["error"]["code"],
                "CANCELLED"
            );
        }
        drop(held);
        application.shutdown().await.unwrap();
        assert_eq!(application.inner.admission.available_permits(), 8);
    }

    #[tokio::test]
    async fn storage_quota_rejection_leaves_no_refs_research_or_events() {
        let mut fake = FakeSource::new();
        fake.searches
            .push_back(FakeAction::Observation(search_page(&["101"], None)));
        let (application, _dir) = fixture(fake).await;
        journal(&application.inner).unwrap().set_cap_bytes(1);
        failed(
            &application,
            "ozon_search",
            json!({"start":{"query":"fixture"}}),
            "STORAGE_FULL",
        )
        .await;
        journal(&application.inner)
            .unwrap()
            .set_cap_bytes(512 * 1024 * 1024);
        let listing = call(&application, "ozon_list_research", json!({})).await;
        assert_eq!(listing["data"]["researches"], json!([]));
    }

    #[test]
    fn review_checkpoints_retain_23091_identities_and_branch_without_mutation() {
        let started = std::time::Instant::now();
        let dir = tempfile::Builder::new()
            .prefix("oz-scale-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let context = json!({"contextId":"scale","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"unknown"});
        let rid = journal.create_research("Scale", &context).unwrap();
        let mut seen = BTreeSet::new();
        let mut reference = None;
        let mut depth = 0;
        let mut first_branch = None;
        for start in (0..23_091).step_by(90) {
            let end = (start + 90).min(23_091);
            let keys = (start..end)
                .map(|i| format!("id:review-{i}"))
                .collect::<Vec<_>>();
            seen.extend(keys.clone());
            let prior = reference.clone();
            let result = journal
                .commit_operation(
                    Some(&rid),
                    || Ok(()),
                    |txn| {
                        let loaded =
                            observations::load_review_seen(txn, &rid, "scale", prior.as_deref())?;
                        assert_eq!(loaded.len(), start);
                        observations::save_review_seen(
                            txn,
                            &rid,
                            prior.as_deref(),
                            depth,
                            &seen,
                            keys,
                        )
                    },
                )
                .unwrap();
            reference = result.0;
            depth = result.1;
            if first_branch.is_none() {
                first_branch = reference.clone();
            }
        }
        journal
            .commit_operation(
                Some(&rid),
                || Ok(()),
                |txn| {
                    let loaded =
                        observations::load_review_seen(txn, &rid, "scale", reference.as_deref())?;
                    assert_eq!(loaded.len(), 23_091);
                    let branch = observations::load_review_seen(
                        txn,
                        &rid,
                        "scale",
                        first_branch.as_deref(),
                    )?;
                    assert_eq!(branch.len(), 90);
                    Ok(())
                },
            )
            .unwrap();
        assert!(depth <= observations::REVIEW_SEEN_CHECKPOINT_INTERVAL);
        eprintln!("review identity scale elapsed {:?}", started.elapsed());
    }
}
