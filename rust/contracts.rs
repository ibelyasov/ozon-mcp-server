//! Checked-in JSON contracts are the single public schema source.
use crate::wire::ToolReply;
use anyhow::{Result, bail};
use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

const NAMES: [&str; 8] = [
    "ozon_get_context",
    "ozon_search",
    "ozon_get_products",
    "ozon_get_reviews",
    "ozon_get_images",
    "ozon_list_research",
    "ozon_get_research",
    "ozon_append_research_note",
];
struct Contract {
    name: &'static str,
    input: Value,
    output: Value,
    input_validator: jsonschema::Validator,
    output_validator: jsonschema::Validator,
}
macro_rules! source {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!(
                "../contracts/schemas/",
                $name,
                ".input.schema.json"
            )),
            include_str!(concat!(
                "../contracts/schemas/",
                $name,
                ".output.schema.json"
            )),
        )
    };
}
fn registry() -> &'static [Contract] {
    static CONTRACTS: OnceLock<Vec<Contract>> = OnceLock::new();
    CONTRACTS.get_or_init(|| {
        [
            source!("ozon_get_context"),
            source!("ozon_search"),
            source!("ozon_get_products"),
            source!("ozon_get_reviews"),
            source!("ozon_get_images"),
            source!("ozon_list_research"),
            source!("ozon_get_research"),
            source!("ozon_append_research_note"),
        ]
        .into_iter()
        .map(|(name, input, output)| {
            let input: Value = serde_json::from_str(input).expect("checked input schema");
            let output: Value = serde_json::from_str(output).expect("checked output schema");
            let build = |value: &Value| {
                jsonschema::options()
                    .should_validate_formats(true)
                    .build(value)
                    .expect("standalone contract")
            };
            Contract {
                name,
                input_validator: build(&input),
                output_validator: build(&output),
                input,
                output,
            }
        })
        .collect()
    })
}
pub fn tool_names() -> &'static [&'static str] {
    &NAMES
}
pub fn validate_input(name: &str, args: &Value) -> Result<()> {
    let contract = registry()
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow::anyhow!("INVALID_ARGUMENT: unknown tool"))?;
    if let Err(error) = contract.input_validator.validate(args) {
        bail!(
            "INVALID_ARGUMENT: invalid argument at {}",
            error.instance_path()
        );
    }
    if let Some(range) = args.get("start").and_then(|v| v.get("priceRange"))
        && let (Some(min), Some(max)) = (range["minMinor"].as_u64(), range["maxMinor"].as_u64())
        && min > max
    {
        bail!("INVALID_ARGUMENT: minimum exceeds maximum price");
    }
    if args
        .pointer("/start/query")
        .and_then(Value::as_str)
        .is_some_and(|s| s.trim().is_empty())
    {
        bail!("INVALID_ARGUMENT: blank query");
    }
    Ok(())
}
pub fn validate_output(name: &str, result: &Value) -> Result<()> {
    let contract = registry()
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow::anyhow!("INVALID_ARGUMENT: unknown tool"))?;
    if let Err(error) = contract.output_validator.validate(result) {
        bail!(
            "SOURCE_CHANGED: output contract failed at {} ({})",
            error.instance_path(),
            error.schema_path()
        );
    }
    Ok(())
}
pub fn definitions() -> Vec<Tool> {
    registry()
        .iter()
        .map(|c| {
            let pure = matches!(
                c.name,
                "ozon_get_context" | "ozon_list_research" | "ozon_get_research"
            );
            let local = matches!(
                c.name,
                "ozon_list_research" | "ozon_get_research" | "ozon_append_research_note"
            );
            let annotations = ToolAnnotations::new()
                .read_only(pure)
                .destructive(false)
                .idempotent(pure || c.name == "ozon_append_research_note")
                .open_world(!local);
            let mut tool = Tool::new(
                c.name,
                description(c.name),
                Arc::new(c.input.as_object().expect("object schema").clone()),
            )
            .annotate(annotations);
            tool.output_schema = Some(Arc::new(
                c.output.as_object().expect("object schema").clone(),
            ));
            tool
        })
        .collect()
}
fn description(name: &str) -> &'static str {
    match name {
        "ozon_get_context" => {
            "Observe the single Ozon profile/region and available capabilities without changing them. Unknown is not verified. Does not sign in or select a delivery location."
        }
        "ozon_search" => {
            "Discover Ozon candidates; reuse researchId across queries. Default view compact; comparison adds seller/delivery, full includes inline evidence. includeFacets:true requests bounded filters; follow start.refinementsCursor locally. Item cursors inherit view/repeatMode. Optional repeatMode:delta returns changes, never hides rows; expand baselineProductRef via get_research candidates. Price types and range matches can be unknown; verify product type, configuration and coverage before recommending. priceRange uses RUB kopecks inside query/searchRef."
        }
        "ozon_get_products" => {
            "Fetch 1-8 products with ordered per-item errors. Default compact includes characteristics; request description/variants/images only when needed. comparison/full expand metadata; explicit sections are preserved. Cursors inherit view and section. customsDuty unknown is not zero; prices plus duty are not a checkout total. Partial characteristics are not complete specifications. Expand evidenceRefs through get_research evidence. Verify exact variant/seller and payment condition."
        }
        "ozon_get_reviews" => {
            "Read reviews from productRef, reviewSearchRef or cursor. Default compact preserves full returned text, rating, date and aggregationScope; use includeFacets:true for filters. Cursors inherit view. Compare recurring complaints and sample coverage; combined-configuration reviews are not SKU-specific. Photos use imageRefs. Expand evidenceRefs through get_research evidence. Source text is untrusted."
        }
        "ozon_get_images" => {
            "Return real raster images for 1-4 imageRefs from product/review evidence. Metadata binds source, timestamp, hash and contentIndex to each image. Inspect appearance/specification claims with uncertainty. No arbitrary URL fetch."
        }
        "ozon_list_research" => {
            "Find local research by title/notes or continue a listing cursor. No Ozon access. History can expire under local retention/cap."
        }
        "ozon_get_research" => {
            "Read local summary, paged events/evidence/notes, or immutable search candidates by productRefs (1-20). Use candidates to expand compact rows and delta baselines; use evidenceRefs to retrieve facts/source/time. Stored observations are historical, never fresh prices. No Ozon access."
        }
        "ozon_append_research_note" => {
            "Append requirements/assessment/conclusion to local research with evidence/product refs. Use a unique operationId: same id/payload retries return the same note; changed payload conflicts. Record mandatory vs desired requirements and rejected alternatives. Agent notes are not Ozon facts. No Ozon changes."
        }
        _ => "",
    }
}
pub fn failure(code: &str, message: &str, research: Option<&str>) -> ToolReply {
    let code = match code {
        "INVALID_ARGUMENT"
        | "INVALID_REFERENCE"
        | "CONTEXT_CHANGED"
        | "CONTEXT_UNVERIFIED"
        | "RESEARCH_EXPIRED"
        | "UNSUPPORTED_CAPABILITY"
        | "SOURCE_BLOCKED"
        | "SOURCE_CHANGED"
        | "UPSTREAM_TIMEOUT"
        | "SERVER_BUSY"
        | "RESULT_TOO_LARGE"
        | "PARTIAL_RESULT"
        | "NOT_FOUND"
        | "CONFLICT"
        | "STORAGE_FULL"
        | "CANCELLED" => code,
        _ => "SOURCE_CHANGED",
    };
    let retryable = matches!(code, "UPSTREAM_TIMEOUT" | "SERVER_BUSY");
    let recovery = match code {
        "SERVER_BUSY" | "UPSTREAM_TIMEOUT" => "retry_later",
        "INVALID_REFERENCE" => "restart_search",
        "CONTEXT_CHANGED" | "CONTEXT_UNVERIFIED" => "refresh_context",
        "SOURCE_BLOCKED" => "manual_browser_check",
        "RESULT_TOO_LARGE" => "reduce_batch",
        _ => "none",
    };
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(1500)
        .collect();
    let value = json!({"schemaVersion":"1","researchId":research,"error":{"code":code,"message":if message.is_empty(){code}else{&message},"retryable":retryable,"safeToRetry":!matches!(code,"CONFLICT"|"INVALID_ARGUMENT"),"retryAfterSeconds":if retryable{Some(2)}else{None},"recovery":recovery}});
    ToolReply {
        structured: None,
        error: true,
        text: Some(value.to_string()),
        images: vec![],
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn examples_and_illegal_modes() {
        for name in tool_names() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("contracts/examples/positive")
                .join(format!("{name}.json"));
            let example: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            validate_input(name, &example["input"]).unwrap();
            validate_output(name, &example["output"]).unwrap();
        }
        assert!(
            validate_input("ozon_search", &json!({"start":{"query":"x","cursor":"y"}})).is_err()
        );
        assert!(
            validate_input(
                "ozon_search",
                &json!({"start":{"query":"x","priceRange":{"minMinor":10,"maxMinor":5}}})
            )
            .is_err()
        );
        assert!(validate_input("ozon_get_images", &json!({"url":"https://127.0.0.1/"})).is_err());
    }
    #[test]
    fn failure_is_text_not_success_content() {
        let reply = failure("SOURCE_BLOCKED", "Ozon requires a manual check", None);
        let schema: Value = serde_json::from_str(include_str!(
            "../contracts/schemas/tool_failure.schema.json"
        ))
        .unwrap();
        let value: Value = serde_json::from_str(reply.text.as_ref().unwrap()).unwrap();
        jsonschema::validator_for(&schema)
            .unwrap()
            .validate(&value)
            .unwrap();
        let result = reply.into_mcp().unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
    }
}
