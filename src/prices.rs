//! Public API prices from the models.dev catalog, and the rules that apply them to requests.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexSet;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::model::{Api, BillingPart, Cost, Harness, PartKind, Request};
use crate::safe::{ScanError, iso, label, object};

const PRICE_URL: &str = "https://models.dev/api.json";

/// Dated provider rules used to interpret catalog fields.
pub const PRICE_SOURCES: [&str; 3] = [
    "https://developers.openai.com/api/docs/guides/prompt-caching (2026-11-07)",
    "https://developers.openai.com/api/docs/pricing (2026-11-07)",
    "https://platform.claude.com/docs/en/about-claude/pricing (2026-11-07)",
];

/// Dollars per million tokens; absent rates stay unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rates {
    input: Option<f64>,
    output: Option<f64>,
    read: Option<f64>,
    write: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Tier {
    size: f64,
    rates: Rates,
}

#[derive(Clone, Debug, PartialEq)]
struct ModelPrice {
    base: Rates,
    tiers: Vec<Tier>,
    legacy: Option<Rates>,
    invalid: bool,
}

/// Catalog stores only numeric pricing metadata, keyed by exact provider and model.
#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    models: HashMap<String, HashMap<String, ModelPrice>>,
    pub source: String,
    pub fetched_at: Option<String>,
    pub offline: bool,
}

impl Catalog {
    fn model(&self, provider: &str, model: &str) -> Option<&ModelPrice> {
        self.models.get(provider)?.get(model)
    }

    /// The ascending input-token sizes above which an exact provider/model uses a higher context tier.
    pub fn tier_sizes(&self, provider: &str, model: &str) -> impl Iterator<Item = f64> + '_ {
        self.model(provider, model)
            .into_iter()
            .flat_map(|m| m.tiers.iter().map(|t| t.size))
    }
}

fn rate(value: Option<&Value>) -> Result<Option<f64>, ScanError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.is_finite() && f >= 0.0 => Ok(Some(f)),
            _ => Err(ScanError::new("invalid price catalog rate")),
        },
        Some(_) => Err(ScanError::new("invalid price catalog rate")),
    }
}

fn rates(value: &Value) -> Result<Rates, ScanError> {
    let c = object(value);
    Ok(Rates {
        input: rate(c.get("input"))?,
        output: rate(c.get("output"))?,
        read: rate(c.get("cache_read"))?,
        write: rate(c.get("cache_write"))?,
    })
}

/// JavaScript truthiness, which decides whether a legacy tier field is present.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn model_price(value: &Value) -> Result<ModelPrice, ScanError> {
    let cost = object(value).get("cost").unwrap_or(&Value::Null);
    let fields = object(cost);
    let mut tiers = Vec::new();
    let mut invalid = false;
    if let Some(list) = fields.get("tiers") {
        let Value::Array(list) = list else {
            return Err(ScanError::new("invalid price catalog tiers"));
        };
        for entry in list {
            let tier = object(entry).get("tier").unwrap_or(&Value::Null);
            let size = rate(object(tier).get("size"))?;
            let context = object(tier).get("type").and_then(Value::as_str) == Some("context");
            match size {
                Some(size) if context => tiers.push(Tier {
                    size,
                    rates: rates(entry)?,
                }),
                _ => invalid = true,
            }
        }
    }
    tiers.sort_by(|a, b| a.size.partial_cmp(&b.size).expect("finite tier sizes"));
    let legacy = match fields.get("context_over_200k") {
        Some(value) if truthy(value) => Some(rates(value)?),
        _ => None,
    };
    Ok(ModelPrice {
        base: rates(cost)?,
        tiers,
        legacy,
        invalid,
    })
}

/// Why a catalog could not be read: a validation error or a stream failure owned by the caller.
enum Failure {
    Scan(ScanError),
    Io,
}

impl From<io::Error> for Failure {
    fn from(_: io::Error) -> Self {
        Failure::Io
    }
}

#[derive(Default)]
struct Parsed {
    models: HashMap<String, HashMap<String, ModelPrice>>,
    error: Option<ScanError>,
}

/// Where a visitor is in the `$.*.models.*` path.
enum Level {
    Root,
    Provider(String),
    Models(String),
}

/// Visits the catalog one model at a time, skipping everything outside `$.*.models.*`.
struct Walk<'s> {
    parsed: &'s mut Parsed,
    level: Level,
}

impl<'de> DeserializeSeed<'de> for Walk<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Walk<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            match &self.level {
                Level::Root => match label(&Value::String(key)) {
                    Some(provider) => map.next_value_seed(Walk {
                        parsed: &mut *self.parsed,
                        level: Level::Provider(provider),
                    })?,
                    None => {
                        map.next_value::<IgnoredAny>()?;
                    }
                },
                Level::Provider(provider) if key == "models" => map.next_value_seed(Walk {
                    parsed: &mut *self.parsed,
                    level: Level::Models(provider.clone()),
                })?,
                Level::Provider(_) => {
                    map.next_value::<IgnoredAny>()?;
                }
                Level::Models(provider) => match label(&Value::String(key)) {
                    Some(model) => {
                        let value = map.next_value::<Value>()?;
                        match model_price(&value) {
                            Ok(price) => {
                                self.parsed
                                    .models
                                    .entry(provider.clone())
                                    .or_default()
                                    .insert(model, price);
                            }
                            Err(error) => {
                                self.parsed.error = Some(error);
                                return Err(de::Error::custom("invalid model price"));
                            }
                        }
                    }
                    None => {
                        map.next_value::<IgnoredAny>()?;
                    }
                },
            }
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
}

/// Parse the public catalog one model at a time instead of buffering the whole document.
fn parse_catalog(reader: impl Read) -> Result<Catalog, Failure> {
    let mut parsed = Parsed::default();
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(reader));
    let result = Walk {
        parsed: &mut parsed,
        level: Level::Root,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    if let Some(error) = parsed.error {
        return Err(Failure::Scan(error));
    }
    match result {
        // Stream failures belong to the file/fetch diagnostic, not JSON validation.
        Err(error) if error.is_io() => return Err(Failure::Io),
        Err(_) => {
            return Err(Failure::Scan(ScanError::new(
                "invalid public price catalog",
            )));
        }
        Ok(()) => {}
    }
    if parsed.models.is_empty() {
        return Err(Failure::Scan(ScanError::new(
            "public price catalog contains no models",
        )));
    }
    Ok(Catalog {
        models: parsed.models,
        source: PRICE_URL.to_string(),
        fetched_at: None,
        offline: true,
    })
}

/// Explicit offline and fixture modes never make network requests.
#[derive(Clone, Debug, Default)]
pub struct PriceOptions {
    pub file: Option<PathBuf>,
    pub cache: PathBuf,
    pub offline: bool,
}

/// Copies every byte the parser reads into the cache file being written.
struct Tee<R> {
    body: R,
    file: File,
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.body.read(buf)?;
        self.file.write_all(&buf[..n])?;
        Ok(n)
    }
}

/// Removes this operation's partial download however it ends; other downloads own other names.
struct Discard<'a>(&'a Path);

impl Drop for Discard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

fn now_millis() -> i64 {
    millis(SystemTime::now()).expect("clock after the Unix epoch")
}

fn millis(time: SystemTime) -> Option<i64> {
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// A name no concurrent download in this or another process will pick.
fn unique() -> String {
    use std::hash::{BuildHasher, RandomState};
    let state = RandomState::new();
    format!(
        "{:016x}{:016x}",
        state.hash_one(std::process::id()),
        state.hash_one(SystemTime::now())
    )
}

fn read_file(file: &Path) -> Result<Catalog, Failure> {
    let mut catalog = parse_catalog(File::open(file)?)?;
    let modified = millis(fs::metadata(file)?.modified()?).ok_or(Failure::Io)?;
    catalog.fetched_at = Some(iso(modified));
    Ok(catalog)
}

fn download(cache: &Path, temporary: &Path) -> Result<Catalog, Failure> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent.get(PRICE_URL).call().map_err(|_| Failure::Io)?;
    // A redirect is a fetch failure, not an unsuccessful response.
    if response.status().is_redirection() {
        return Err(Failure::Io);
    }
    if !response.status().is_success() {
        return Err(Failure::Scan(ScanError::new(
            "public price fetch failed; use --offline with an existing cache",
        )));
    }
    if let Some(parent) = cache.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(temporary)?;
    let catalog = parse_catalog(Tee {
        body: response.body_mut().as_reader(),
        file,
    })?;
    fs::rename(temporary, cache)?;
    Ok(catalog)
}

/// Fetch only public prices, caching atomically after the complete catalog validates.
pub fn load_prices(options: &PriceOptions) -> Result<Catalog, ScanError> {
    if options.file.is_some() || options.offline {
        let file = options.file.as_deref().unwrap_or(&options.cache);
        return match read_file(file) {
            Ok(mut catalog) => {
                catalog.source = file.to_string_lossy().into_owned();
                Ok(catalog)
            }
            Err(Failure::Scan(error)) => Err(error),
            Err(Failure::Io) => Err(ScanError::new(
                "cannot read price file; fetch prices once before offline use",
            )),
        };
    }
    let mut temporary = options.cache.clone().into_os_string();
    temporary.push(format!(".{}.tmp", unique()));
    let temporary = PathBuf::from(temporary);
    let _discard = Discard(&temporary);
    match download(&options.cache, &temporary) {
        Ok(catalog) => Ok(Catalog {
            fetched_at: Some(iso(now_millis())),
            offline: false,
            ..catalog
        }),
        Err(Failure::Scan(error)) => Err(error),
        Err(Failure::Io) => Err(ScanError::new(
            "cannot fetch or cache public prices; use --offline with an existing cache",
        )),
    }
}

/// A known zero with no missing components.
pub fn zero_cost() -> Cost {
    Cost {
        known_usd: 0.0,
        total_usd: Some(0.0),
        unknown: vec![],
        assumptions: vec![],
    }
}

/// Keep partial known charges separate from an unknown total.
pub fn add_costs<'a>(costs: impl IntoIterator<Item = &'a Cost>) -> Cost {
    let mut known = 0.0;
    let mut total = Some(0.0);
    let mut unknown = IndexSet::new();
    let mut assumptions = IndexSet::new();
    for cost in costs {
        known += cost.known_usd;
        total = match (total, cost.total_usd) {
            (Some(sum), Some(value)) => Some(sum + value),
            _ => None,
        };
        unknown.extend(cost.unknown.iter().map(String::as_str));
        assumptions.extend(cost.assumptions.iter().map(String::as_str));
    }
    Cost {
        known_usd: known,
        total_usd: total,
        unknown: unknown.into_iter().map(str::to_string).collect(),
        assumptions: assumptions.into_iter().map(str::to_string).collect(),
    }
}

/// Baseline-minus-alternative savings over the items both scenarios price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairedSavings {
    pub usd: f64,
    pub priced: u64,
    pub total: u64,
    /// True when the unpriced items cannot flip the sign, each bounded by the largest priced difference.
    pub decided: bool,
}

/// Compare two scenarios item by item as (baseline, alternative), so a few unpriced requests
/// cannot hide a clear winner. `unknown` counts further items that could differ but left no
/// evidence, such as unreadable transcript lines.
pub fn paired_savings<'a>(
    pairs: impl IntoIterator<Item = (&'a Cost, &'a Cost)>,
    unknown: u64,
) -> PairedSavings {
    let mut usd = 0.0;
    let mut priced = 0;
    let mut count = 0;
    let mut largest: f64 = 0.0;
    for (baseline, alternative) in pairs {
        count += 1;
        let (Some(baseline), Some(alternative)) = (baseline.total_usd, alternative.total_usd)
        else {
            continue;
        };
        let delta = baseline - alternative;
        usd += delta;
        priced += 1;
        largest = largest.max(delta.abs());
    }
    let unpriced = count - priced + unknown;
    PairedSavings {
        usd,
        priced,
        total: count + unknown,
        decided: unpriced == 0 || (priced > 0 && usd.abs() > unpriced as f64 * largest),
    }
}

/// Represent missing pricing without substituting a different model.
pub fn unknown_cost(reason: impl Into<String>) -> Cost {
    Cost {
        known_usd: 0.0,
        total_usd: None,
        unknown: vec![reason.into()],
        assumptions: vec![],
    }
}

/// Split a leading run of ASCII digits from the rest of `text`.
fn digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// Whether `text` is empty or continues with a hyphen.
fn ends_or_hyphen(text: &str) -> bool {
    text.is_empty() || text.starts_with('-')
}

/// Whether `model` is one of `names`, optionally followed by a `-YYYY-MM-DD` snapshot date.
fn dated(model: &str, names: &[&str]) -> bool {
    names.iter().any(|name| {
        let Some(rest) = model.strip_prefix(name) else {
            return false;
        };
        let b = rest.as_bytes();
        rest.is_empty()
            || (b.len() == 11
                && b[0] == b'-'
                && b[1..5].iter().all(u8::is_ascii_digit)
                && b[5] == b'-'
                && b[6..8].iter().all(u8::is_ascii_digit)
                && b[8] == b'-'
                && b[9..11].iter().all(u8::is_ascii_digit))
    })
}

/// Whether the documented GPT generation bills cache writes separately from ordinary input.
pub fn write_billing(model: &str) -> Option<bool> {
    let (major, rest) = digits(model.strip_prefix("gpt-")?)?;
    let (minor, rest) = match rest.strip_prefix('.').and_then(digits) {
        Some((minor, rest)) => (Some(minor), rest),
        None => (None, rest),
    };
    if !ends_or_hyphen(rest) {
        return None;
    }
    let major: f64 = major.parse().ok()?;
    let minor: f64 = minor.map_or(Some(0.0), |m| m.parse().ok())?;
    Some(major >= 6.0 || (major == 5.0 && minor >= 6.0))
}

fn threshold(provider: &str, model: &str) -> Option<i64> {
    // The legacy field is named over_200k, but these models bill above 272k.
    if provider == "openai"
        && let Some(rest) = model.strip_prefix("gpt-")
    {
        let five = ["5.4", "5.5", "5.6"]
            .iter()
            .any(|v| rest.strip_prefix(v).is_some_and(ends_or_hyphen));
        let six = rest
            .strip_prefix('6')
            .is_some_and(|r| r.is_empty() || r.starts_with(['.', '-']));
        if five || six {
            return Some(272_000);
        }
    }
    if provider == "anthropic"
        && model
            .strip_prefix("claude-haiku-5-5")
            .is_some_and(ends_or_hyphen)
    {
        return Some(100_000);
    }
    None
}

/// Whether US inference is documented for this Claude generation (4.6 and later).
fn us_generation(model: &str) -> bool {
    let Some(rest) = ["opus-", "sonnet-", "haiku-", "fable-", "mythos-"]
        .iter()
        .find_map(|family| model.strip_prefix("claude-")?.strip_prefix(family))
    else {
        return false;
    };
    let Some((major, rest)) = digits(rest) else {
        return false;
    };
    let major: f64 = major.parse().unwrap_or(f64::NAN);
    let minor = rest
        .strip_prefix('-')
        .and_then(digits)
        .map_or(f64::NAN, |(minor, _)| minor.parse().unwrap_or(f64::NAN));
    major > 4.0 || (major == 4.0 && minor >= 6.0)
}

/// Whether the documented fast price applies to this Claude Opus model.
fn fast_opus(model: &str) -> bool {
    model.strip_prefix("claude-opus-").is_some_and(|rest| {
        ["5-5", "5", "4-8"]
            .iter()
            .any(|v| rest.strip_prefix(v).is_some_and(ends_or_hyphen))
    })
}

/// The documented OpenAI premium-tier multiplier, or None when unverified for this model/context.
fn premium_multiplier(tier: &str, model: &str, input: Option<i64>) -> Option<f64> {
    let within = input.is_some_and(|i| i <= 272_000);
    if tier == "ultrafast" {
        return dated(model, &["gpt-6-astra", "gpt-6.1-sol"]).then_some(6.0);
    }
    if dated(
        model,
        &[
            "gpt-6-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "gpt-6.1-terra",
            "gpt-5.6",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ],
    ) {
        Some(2.0)
    } else if dated(model, &["gpt-5.5"]) && within {
        Some(2.5)
    } else if dated(model, &["gpt-5.4", "gpt-5.4-mini"]) && within {
        Some(2.0)
    } else {
        None
    }
}

struct Selected {
    rates: Rates,
    assumptions: Vec<String>,
}

fn selected_rates(
    catalog: &Catalog,
    r: &Request,
    p: &BillingPart,
) -> Result<Selected, &'static str> {
    let (Some(provider), Some(model_name)) = (
        r.provider.as_deref().filter(|s| !s.is_empty()),
        p.model.as_deref().filter(|s| !s.is_empty()),
    ) else {
        return Err("model/provider attribution is unknown");
    };
    let Some(model) = catalog.model(provider, model_name) else {
        return Err("exact model/provider has no public catalog price");
    };
    if model.invalid {
        return Err("unsupported catalog tier dimensions");
    }
    if p.kind == PartKind::Unknown {
        return Err("unknown sampling iteration semantics");
    }
    let mut assumptions = Vec::new();
    let mut selected = model.base;
    let documented = threshold(provider, model_name);
    if model.legacy.is_some() || !model.tiers.is_empty() {
        let Some(input) = p.usage.input else {
            return Err("context tier requires total input");
        };
        if model.legacy.is_some() && documented.is_none() {
            return Err("legacy context threshold is undocumented for this model");
        }
        if let Some(documented) = documented
            && model.tiers.iter().any(|t| t.size != documented as f64)
        {
            return Err("catalog context threshold contradicts dated provider documentation");
        }
        if !model.tiers.is_empty() {
            for tier in &model.tiers {
                if input as f64 > tier.size {
                    selected = tier.rates;
                }
            }
        } else if let (Some(legacy), Some(documented)) = (model.legacy, documented)
            && input > documented
        {
            selected = legacy;
        }
        if model.legacy.is_some() {
            assumptions
                .push("legacy context_over_200k resolved at documented 272000 input tokens".into());
        }
    }
    let mut factor = 1.0;
    let Some(tier) = p.tier.as_deref() else {
        return Err("effective service tier is not recorded");
    };
    if provider == "openai" && ["fast", "priority", "ultrafast"].contains(&tier) {
        let Some(multiplier) = premium_multiplier(tier, model_name, p.usage.input) else {
            return Err("premium tier lacks verified model/context rates");
        };
        factor *= multiplier;
        assumptions.push(format!(
            "documented {tier} API token rates are {multiplier}x standard, preserving context tiers and cache categories (2026-11-07)"
        ));
    } else if !["standard", "default"].contains(&tier) {
        return Err("effective service tier has no supported catalog rate");
    }
    if provider == "anthropic" {
        match p.speed.as_deref() {
            Some("fast") => {
                if !fast_opus(model_name) {
                    return Err("fast speed lacks verified model/context rates");
                }
                factor *= 2.0;
                assumptions.push(
                    "documented fast price is twice standard across the full context window".into(),
                );
            }
            Some("standard") => {}
            Some(_) => return Err("unknown response speed"),
            None => assumptions.push("unrecorded response speed assumed standard".into()),
        }
        match p.geo.as_deref() {
            Some("us") => {
                if !us_generation(model_name) {
                    return Err("US inference is unsupported for this model generation");
                }
                factor *= 1.1;
                assumptions.push("documented US inference multiplier 1.1".into());
            }
            Some("global") => {}
            None | Some("not_available") => {
                assumptions.push("unrecorded geography assumed global API equivalent".into())
            }
            Some(_) => return Err("regional rate is unsupported"),
        }
    }
    let scale = |rate: Option<f64>| rate.map(|r| r * factor);
    Ok(Selected {
        rates: Rates {
            input: scale(selected.input),
            output: scale(selected.output),
            read: scale(selected.read),
            write: scale(selected.write),
        },
        assumptions,
    })
}

/// Claude per-token prices for one part's cache reads, writes and ordinary input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnitRates {
    pub read: f64,
    pub write5m: f64,
    pub write1h: f64,
    pub ordinary: f64,
}

/// Claude per-token prices for one part's cache reads, writes and ordinary input; None when unpriced.
pub fn unit_rates(catalog: &Catalog, r: &Request, p: &BillingPart) -> Option<UnitRates> {
    if r.harness != Harness::Claude {
        return None;
    }
    let rates = selected_rates(catalog, r, p).ok()?.rates;
    let (input, read, write) = (rates.input?, rates.read?, rates.write?);
    Some(UnitRates {
        read: read / 1e6,
        write5m: write / 1e6,
        write1h: (input * 2.0) / 1e6,
        ordinary: input / 1e6,
    })
}

/// One part's cache, ordinary input and output charges, priced at the whole request's context tier.
#[derive(Clone, Debug, PartialEq)]
pub struct Components {
    pub cache: Cost,
    pub ordinary: Cost,
    pub output: Cost,
}

impl Components {
    /// All three charges together.
    pub fn total(&self) -> Cost {
        add_costs([&self.cache, &self.ordinary, &self.output])
    }
}

/// Add `tokens` at a per-million `rate`, or record `component` as unknown.
fn charge(cost: &mut Cost, tokens: Option<i64>, rate: Option<f64>, component: &str) {
    match tokens {
        Some(t) if t <= 0 || rate.is_some() => {
            cost.known_usd += (t as f64 * rate.unwrap_or(0.0)) / 1e6;
        }
        _ => cost.unknown.push(component.to_string()),
    }
}

/// Price ordinary input, which is assigned rather than accumulated.
fn ordinary_charge(cost: &mut Cost, tokens: Option<i64>, rate: Option<f64>) {
    match tokens {
        Some(t) if t <= 0 || rate.is_some() => {
            cost.known_usd = (t as f64 * rate.unwrap_or(0.0)) / 1e6;
        }
        _ => cost.unknown.push("ordinary input unknown".to_string()),
    }
}

/// Price cache, ordinary input and output at the whole request's context tier.
pub fn price_components(catalog: &Catalog, r: &Request, p: &BillingPart) -> Components {
    let u = &p.usage;
    let empty_cache = p.kind != PartKind::Unknown && u.read == Some(0) && u.write == Some(0);
    let selected =
        if u.input.is_none() && u.output.is_none() && u.read.is_none() && u.write.is_none() {
            Err("request usage is not recorded")
        } else {
            selected_rates(catalog, r, p)
        };
    let selected = match selected {
        Ok(selected) => selected,
        Err(reason) => {
            let cost = unknown_cost(reason);
            return Components {
                cache: if empty_cache {
                    zero_cost()
                } else {
                    cost.clone()
                },
                ordinary: cost.clone(),
                output: cost,
            };
        }
    };
    let start = || Cost {
        assumptions: selected.assumptions.clone(),
        ..zero_cost()
    };
    let mut result = start();
    let mut ordinary_cost = start();
    let mut output = start();
    let rates = selected.rates;
    match u.output {
        Some(o) if o <= 0 || rates.output.is_some() => {
            output.known_usd = (o as f64 * rates.output.unwrap_or(0.0)) / 1e6;
        }
        _ => output
            .unknown
            .push("output tokens/rate unknown".to_string()),
    }
    charge(&mut result, u.read, rates.read, "cache reads/rate unknown");
    if r.harness == Harness::Claude {
        let ordinary = match (u.input, u.read, u.write) {
            (Some(input), Some(read), Some(write)) => Some(input - read - write),
            _ => None,
        };
        ordinary_charge(&mut ordinary_cost, ordinary, rates.input);
        let empty_writes = u.write == Some(0);
        charge(
            &mut result,
            if empty_writes { Some(0) } else { u.write5m },
            rates.write,
            "5m write split/rate unknown",
        );
        charge(
            &mut result,
            if empty_writes { Some(0) } else { u.write1h },
            rates.input.map(|i| i * 2.0),
            "1h write split/rate unknown",
        );
        if !empty_writes && u.write1h.unwrap_or(0) > 0 {
            result.assumptions.push(
                "1h total write price derived as 2x ordinary input from provider documentation"
                    .to_string(),
            );
        }
    } else {
        let billing = p
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
            .and_then(write_billing);
        match (billing, u.write) {
            (None, _) => result
                .unknown
                .push("model cache-write billing rules unknown".to_string()),
            (Some(false), _) => {
                let ordinary = match (u.input, u.read) {
                    (Some(input), Some(read)) => Some(input - read),
                    _ => None,
                };
                ordinary_charge(&mut ordinary_cost, ordinary, rates.input);
            }
            (Some(true), None) => {
                ordinary_charge(&mut ordinary_cost, None, rates.input);
                result.unknown.push(
                    "cache writes missing on a model with billed writes; uncached input is not evidence of writes"
                        .to_string(),
                );
            }
            (Some(true), Some(write)) => {
                let ordinary = match (u.input, u.read) {
                    (Some(input), Some(read)) => Some(input - read - write),
                    _ => None,
                };
                ordinary_charge(&mut ordinary_cost, ordinary, rates.input);
                let (mut short, mut long) = (u.write30m, u.write12h);
                if write == 0 {
                    short = Some(0);
                    long = Some(0);
                } else if short.is_none() && long.is_none() {
                    short = Some(write);
                    long = Some(0);
                    result.assumptions.push(
                        "unsplit recorded writes assumed default 30m lifetime; 12h allocation unknown"
                            .to_string(),
                    );
                }
                if r.api != Api::Responses && long.unwrap_or(0) > 0 {
                    result
                        .unknown
                        .push("12h writes require the Responses API".to_string());
                }
                charge(
                    &mut result,
                    short,
                    rates.input.map(|i| i * 1.25),
                    "30m write split/rate unknown",
                );
                charge(
                    &mut result,
                    long,
                    rates.input.map(|i| i * 2.5),
                    "12h write split/rate unknown",
                );
                result.assumptions.push(
                    "write counts replace ordinary input at total 1.25x/2.5x rates; reasoning is already in output"
                        .to_string(),
                );
            }
        }
    }
    for cost in [&mut result, &mut ordinary_cost, &mut output] {
        cost.total_usd = cost.unknown.is_empty().then_some(cost.known_usd);
    }
    Components {
        cache: result,
        ordinary: ordinary_cost,
        output,
    }
}

/// Price the whole request again for every counterfactual, including context-tier changes.
pub fn price_part(catalog: &Catalog, r: &Request, p: &BillingPart) -> Cost {
    price_components(catalog, r, p).total()
}

/// Price non-token server fees separately from client tools and token iterations.
pub fn server_cost(r: &Request) -> Cost {
    if r.harness != Harness::Claude {
        return zero_cost();
    }
    let Some(tools) = &r.server_tools else {
        return unknown_cost("server-tool fee coverage is not recorded");
    };
    let mut result = zero_cost();
    for (name, &requests) in tools {
        if name == "web_search_requests" && r.provider.as_deref() == Some("anthropic") {
            result.known_usd += requests as f64 * 0.01;
        } else if name != "web_fetch_requests" && requests > 0 {
            result.unknown.push(
                "server-tool charge requires execution time, free allowance, tool version, or provider rate"
                    .to_string(),
            );
        }
    }
    result.total_usd = result.unknown.is_empty().then_some(result.known_usd);
    result
}

/// Price non-overlapping iterations once, adding only separately billed server fees.
pub fn price_request(catalog: &Catalog, r: &Request) -> Cost {
    let parts: Vec<Cost> = r
        .parts
        .iter()
        .map(|p| price_part(catalog, r, p))
        .chain([server_cost(r)])
        .collect();
    let mut result = add_costs(&parts);
    let mut assumptions: IndexSet<String> = result.assumptions.drain(..).collect();
    assumptions.extend(r.issues.iter().cloned());
    result.assumptions = assumptions.into_iter().collect();
    if r.issues.iter().any(|issue| {
        issue.contains("no verified request delta")
            || issue.contains("usage is absent")
            || issue.contains("no linked assistant usage")
    }) {
        result.total_usd = None;
        result.unknown.extend(r.issues.iter().cloned());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;
    use crate::test_support::{ClaudeUsage, claude_usage, make_request, prices};

    fn codex_request() -> Request {
        Request {
            harness: Harness::Codex,
            provider: Some("openai".into()),
            api: Api::Responses,
            ..make_request()
        }
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        (a.unwrap() - b).abs() < 1e-10
    }

    #[test]
    fn openai_cache_reads_and_writes_replace_ordinary_input_and_reasoning_stays_a_subset() {
        let catalog = prices();
        let r = codex_request();
        let p = BillingPart {
            model: Some("gpt-6.1-sol".into()),
            usage: Usage {
                input: Some(12000),
                output: Some(100),
                read: Some(4000),
                write: Some(7000),
                write30m: Some(3000),
                write12h: Some(4000),
                reasoning: Some(20),
                ..Usage::default()
            },
            ..r.parts[0].clone()
        };
        let result = price_part(&catalog, &r, &p);
        assert!(close(
            result.total_usd,
            (1000.0 * 2.0 + 4000.0 * 0.1 + 3000.0 * 2.5 + 4000.0 * 5.0 + 100.0 * 10.0) / 1e6
        ));
        let missing = price_part(
            &catalog,
            &r,
            &BillingPart {
                usage: Usage {
                    input: Some(12000),
                    output: Some(100),
                    read: Some(4000),
                    ..Usage::default()
                },
                ..p.clone()
            },
        );
        assert_eq!(missing.total_usd, None);
        assert!(missing.unknown.join(" ").contains("cache writes missing"));
        let unknown_api = Request {
            api: Api::Unknown,
            ..r.clone()
        };
        assert_eq!(price_part(&catalog, &unknown_api, &p).total_usd, None);
    }

    #[test]
    fn earlier_openai_models_have_no_additional_write_bill_and_unknown_internal_models_are_never_substituted()
     {
        let c = prices();
        let r = codex_request();
        let p = BillingPart {
            model: Some("gpt-5.4".into()),
            usage: Usage {
                input: Some(1000),
                output: Some(100),
                read: Some(200),
                ..Usage::default()
            },
            ..r.parts[0].clone()
        };
        assert_eq!(
            price_part(&c, &r, &p).total_usd,
            Some((800.0 * 2.5 + 200.0 * 0.25 + 100.0 * 15.0) / 1e6)
        );
        let internal = BillingPart {
            model: Some("codex-auto-review".into()),
            ..p
        };
        assert_eq!(price_part(&c, &r, &internal).total_usd, None);
    }

    #[test]
    fn documented_context_thresholds_reprice_the_full_request_not_just_removed_tokens() {
        let c = prices();
        let r = codex_request();
        let p = BillingPart {
            model: Some("gpt-6.1-sol".into()),
            usage: Usage {
                input: Some(250000),
                output: Some(100),
                read: Some(0),
                write: Some(0),
                ..Usage::default()
            },
            ..r.parts[0].clone()
        };
        let with_input = |input| BillingPart {
            usage: Usage {
                input: Some(input),
                ..p.usage
            },
            ..p.clone()
        };
        assert_eq!(price_part(&c, &r, &p).total_usd, Some(0.501));
        assert_eq!(
            price_part(&c, &r, &with_input(272000)).total_usd,
            Some(0.545)
        );
        assert_eq!(
            price_part(&c, &r, &with_input(272001)).total_usd,
            Some(1.089504)
        );
        let mut h = make_request();
        h.parts[0].model = Some("claude-haiku-5-5".into());
        h.parts[0].usage = claude_usage(ClaudeUsage {
            input: 100001,
            write: 0,
            output: 0,
            ..ClaudeUsage::default()
        });
        assert_eq!(price_part(&c, &h, &h.parts[0]).total_usd, Some(0.0500005));
    }

    #[test]
    fn claude_ttl_region_speed_server_fees_and_missing_prices_remain_explicit() {
        let c = prices();
        let r = make_request();
        let p = BillingPart {
            usage: claude_usage(ClaudeUsage {
                write: 1000,
                read: 200,
                output: 100,
                ttl: crate::model::Ttl::OneHour,
                ..ClaudeUsage::default()
            }),
            geo: Some("us".into()),
            speed: Some("fast".into()),
            ..r.parts[0].clone()
        };
        let cost = price_part(&c, &r, &p);
        assert!(close(
            cost.total_usd,
            ((1000.0 * 8.0 + 200.0 * 0.2 + 100.0 * 20.0) * 2.0 * 1.1) / 1e6
        ));
        let priority = BillingPart {
            tier: Some("priority".into()),
            ..p.clone()
        };
        assert_eq!(price_part(&c, &r, &priority).total_usd, None);
        let custom = Request {
            provider: Some("custom".into()),
            ..r.clone()
        };
        assert_eq!(price_part(&c, &custom, &p).total_usd, None);
        let tools = |entries: &[(&str, i64)]| Request {
            server_tools: Some(entries.iter().map(|&(k, v)| (k.to_string(), v)).collect()),
            ..r.clone()
        };
        assert_eq!(
            server_cost(&tools(&[
                ("web_search_requests", 2),
                ("web_fetch_requests", 3)
            ]))
            .total_usd,
            Some(0.02)
        );
        assert_eq!(
            server_cost(&tools(&[("code_execution_requests", 1)])).total_usd,
            None
        );
        let unrecorded = Request {
            parts: vec![p],
            server_tools: None,
            ..r
        };
        assert_eq!(price_request(&c, &unrecorded).total_usd, None);
    }

    #[test]
    fn unavailable_claude_geography_uses_the_labelled_global_api_equivalent() {
        let r = make_request();
        let c = prices();
        let with_geo = |geo: Option<&str>| BillingPart {
            geo: geo.map(str::to_string),
            ..r.parts[0].clone()
        };
        let result = price_part(&c, &r, &with_geo(Some("not_available")));
        assert_eq!(
            result.total_usd,
            price_part(&c, &r, &with_geo(None)).total_usd
        );
        assert!(
            result
                .assumptions
                .join(" ")
                .contains("unrecorded geography assumed global")
        );
        assert_eq!(
            price_part(&c, &r, &with_geo(Some("unexpected_region"))).total_usd,
            None
        );
    }

    #[test]
    fn paired_savings_decide_only_when_unpriced_parts_cannot_flip_the_sign() {
        let known = |usd: f64| Cost {
            known_usd: usd,
            total_usd: Some(usd),
            ..Cost::default()
        };
        let missing = unknown_cost("missing");
        let (three, two, one) = (known(3.0), known(2.0), known(1.0));
        let a = (&three, &one);
        let b = (&two, &one);
        let gap = (&missing, &missing);
        assert_eq!(
            paired_savings([a, b], 0),
            PairedSavings {
                usd: 3.0,
                priced: 2,
                total: 2,
                decided: true
            }
        );
        // Each unpriced part may differ by up to the largest priced difference (2).
        assert!(paired_savings([a, b, gap], 0).decided);
        assert!(!paired_savings([a, b, gap, gap], 0).decided);
        assert!(!paired_savings([gap], 0).decided);
        assert!(paired_savings([], 0).decided);
    }

    #[test]
    fn model_name_rules_follow_the_documented_patterns() {
        assert_eq!(write_billing("gpt-5.6"), Some(true));
        assert_eq!(write_billing("gpt-5.6-sol"), Some(true));
        assert_eq!(write_billing("gpt-6"), Some(true));
        assert_eq!(write_billing("gpt-5.5-2026-01-01"), Some(false));
        assert_eq!(write_billing("gpt-5"), Some(false));
        assert_eq!(write_billing("gpt-5.6.1"), None);
        assert_eq!(write_billing("gpt-5."), None);
        assert_eq!(write_billing("codex-auto-review"), None);
        assert_eq!(threshold("openai", "gpt-6.1-sol"), Some(272_000));
        assert_eq!(threshold("openai", "gpt-5.40"), None);
        assert_eq!(threshold("openai", "gpt-60"), None);
        assert_eq!(
            threshold("anthropic", "claude-haiku-5-5-20260101"),
            Some(100_000)
        );
        assert!(fast_opus("claude-opus-5-6"));
        assert!(!fast_opus("claude-opus-4-7"));
        assert!(us_generation("claude-sonnet-4-6"));
        assert!(us_generation("claude-opus-4-20250514"));
        assert!(!us_generation("claude-opus-4"));
        assert!(dated("gpt-6.1-sol-2026-03-04", &["gpt-6.1-sol"]));
        assert!(!dated("gpt-6.1-sol-2026-03", &["gpt-6.1-sol"]));
    }

    #[test]
    fn catalogs_validate_rates_and_skip_unlabelled_paths() {
        let parse = |text: &str| match parse_catalog(text.as_bytes()) {
            Ok(catalog) => Ok(catalog),
            Err(Failure::Scan(error)) => Err(error.message),
            Err(Failure::Io) => Err("io".into()),
        };
        let catalog = parse(
            r#"{"p":{"models":{"m":{"cost":{"input":1,"tiers":[{"tier":{"type":"other","size":5}}]}},"bad key":{}}},"x y":5}"#,
        )
        .unwrap();
        assert!(catalog.model("p", "m").unwrap().invalid);
        assert!(catalog.model("p", "bad key").is_none());
        assert_eq!(
            parse(r#"{"p":{"models":{"m":{"cost":{"input":-1}}}}}"#).unwrap_err(),
            "invalid price catalog rate"
        );
        assert_eq!(
            parse(r#"{"p":{"models":{"m":{"cost":{"tiers":null}}}}}"#).unwrap_err(),
            "invalid price catalog tiers"
        );
        assert_eq!(
            parse(r#"{"p":{"models":{"m":{}}}"#).unwrap_err(),
            "invalid public price catalog"
        );
        assert_eq!(
            parse(r#"[{"models":{"m":{}}}]"#).unwrap_err(),
            "public price catalog contains no models"
        );
    }
}
