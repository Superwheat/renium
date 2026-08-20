use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{CloudIdentity, execute_one};
use crate::automation::Failure;
use crate::system::files::absolutize_for_daemon as absolute_path;

#[derive(Subcommand)]
pub(crate) enum DeveloperProductCommand {
    List(DeveloperProductListArgs),
    Get(DeveloperProductGetArgs),
    Create(DeveloperProductCreateArgs),
    Update(DeveloperProductUpdateArgs),
}

#[derive(Args)]
pub(crate) struct DeveloperProductListArgs {
    #[arg(long, default_value_t = 50)]
    page_size: u32,
    #[arg(long)]
    page_token: Option<String>,
}

#[derive(Args)]
pub(crate) struct DeveloperProductGetArgs {
    product_id: u64,
}

#[derive(Args)]
pub(crate) struct DeveloperProductCreateArgs {
    name: String,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    price: Option<u32>,
    #[arg(long)]
    image: Option<PathBuf>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    for_sale: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    regional_pricing: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    managed_pricing: Option<bool>,
}

#[derive(Args)]
pub(crate) struct DeveloperProductUpdateArgs {
    product_id: u64,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    price: Option<u32>,
    #[arg(long)]
    image: Option<PathBuf>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    for_sale: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    regional_pricing: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    managed_pricing: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    store_page: Option<bool>,
}

pub(crate) fn run(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    command: DeveloperProductCommand,
) -> Result<Value> {
    match command {
        DeveloperProductCommand::List(args) => list(identity, key_env, oauth_env, args),
        DeveloperProductCommand::Get(args) => get(identity, key_env, oauth_env, args.product_id),
        DeveloperProductCommand::Create(args) => create(identity, key_env, oauth_env, args),
        DeveloperProductCommand::Update(args) => update(identity, key_env, oauth_env, args),
    }
}

fn list(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    args: DeveloperProductListArgs,
) -> Result<Value> {
    if args.page_size == 0 {
        bail!("--page-size must be greater than zero");
    }
    let mut query = Map::from_iter([("pageSize".to_string(), json!(args.page_size))]);
    if let Some(token) = args.page_token {
        query.insert("pageToken".to_string(), Value::String(token));
    }
    response_body(request(
        identity,
        key_env,
        oauth_env,
        json!({
            "method": "GET",
            "path": "/developer-products/v2/universes/{universe}/developer-products/creator",
            "query": query,
        }),
    )?)
}

fn get(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    product_id: u64,
) -> Result<Value> {
    response_body(request(
        identity,
        key_env,
        oauth_env,
        json!({
            "method": "GET",
            "path": format!("/developer-products/v2/universes/{{universe}}/developer-products/{product_id}/creator"),
        }),
    )?)
}

fn create(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    args: DeveloperProductCreateArgs,
) -> Result<Value> {
    validate_price(args.price)?;
    let mut form = Map::from_iter([("name".to_string(), Value::String(args.name))]);
    optional(&mut form, "description", args.description);
    optional(&mut form, "price", args.price);
    optional(&mut form, "isForSale", args.for_sale);
    optional(&mut form, "isRegionalPricingEnabled", args.regional_pricing);
    optional(&mut form, "isManagedPricingEnabled", args.managed_pricing);
    let created = response_body(request(
        identity,
        key_env,
        oauth_env,
        json!({
            "method": "POST",
            "path": "/developer-products/v2/universes/{universe}/developer-products",
            "form": form,
            "files": image(args.image)?,
        }),
    )?)?;
    let product_id = value_id(created.get("productId"))
        .context("Roblox created the product but did not return productId")?;
    Ok(json!({
        "created": true,
        "product": get(identity, key_env, oauth_env, product_id)?,
    }))
}

fn update(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    args: DeveloperProductUpdateArgs,
) -> Result<Value> {
    validate_price(args.price)?;
    let mut form = Map::new();
    optional(&mut form, "name", args.name);
    optional(&mut form, "description", args.description);
    optional(&mut form, "price", args.price);
    optional(&mut form, "isForSale", args.for_sale);
    optional(&mut form, "isRegionalPricingEnabled", args.regional_pricing);
    optional(&mut form, "isManagedPricingEnabled", args.managed_pricing);
    optional(&mut form, "storePageEnabled", args.store_page);
    let files = image(args.image)?;
    if form.is_empty() && files.is_empty() {
        bail!("Provide at least one developer product field to update");
    }
    request(
        identity,
        key_env,
        oauth_env,
        json!({
            "method": "PATCH",
            "path": format!("/developer-products/v2/universes/{{universe}}/developer-products/{}", args.product_id),
            "form": form,
            "files": files,
        }),
    )?;
    Ok(json!({
        "updated": true,
        "product": get(identity, key_env, oauth_env, args.product_id)?,
    }))
}

fn image(path: Option<PathBuf>) -> Result<Map<String, Value>> {
    let Some(path) = path else {
        return Ok(Map::new());
    };
    let path = absolute_path(&path);
    if !path.is_file() {
        bail!("Developer product image does not exist: {}", path.display());
    }
    Ok(Map::from_iter([(
        "imageFile".to_string(),
        Value::String(path.display().to_string()),
    )]))
}

fn validate_price(price: Option<u32>) -> Result<()> {
    if price.is_some_and(|price| !(1..=1_000_000_000).contains(&price)) {
        bail!("Developer product price must be from 1 through 1000000000 Robux");
    }
    Ok(())
}

fn optional<T: Serialize>(map: &mut Map<String, Value>, name: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(name.to_string(), json!(value));
    }
}

fn value_id(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn response_body(response: Value) -> Result<Value> {
    response
        .get("body")
        .cloned()
        .context("Open Cloud response did not contain a body")
}

fn request(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    request: Value,
) -> Result<Value> {
    execute_one(identity, key_env, oauth_env, false, request).map_err(cloud_error)
}

fn cloud_error(failure: Failure) -> anyhow::Error {
    match failure.0.d {
        Some(detail) => anyhow::anyhow!("{}\n{}", failure.0.m, detail),
        None => anyhow::anyhow!(failure.0.m),
    }
}
