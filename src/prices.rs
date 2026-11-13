//! Public API prices from the models.dev catalog, and the rules that apply them to requests.

use crate::collections::{HashMap, IndexSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Api, BillingPart, Cost, Harness, PartKind, Request, TierScenario, Usd};
use crate::safe::{ScanError, iso, label, object};

const PRICE_URL: &str = "https://models.dev/api.json";

/// Dated provider rules used to interpret catalog fields.
pub const PRICE_SOURCES: [&str; 7] = [
    "https://developers.openai.com/api/docs/guides/prompt-caching (2026-11-07)",
    "https://developers.openai.com/api/docs/pricing (2026-11-07)",
    "https://platform.claude.com/docs/en/about-claude/pricing (2026-11-07)",
    "https://developers.openai.com/api/reference/python/resources/responses (2026-11-08)",
    "https://developers.openai.com/api/docs/guides/fast-mode (2026-11-08)",
    "https://learn.chatgpt.com/docs/agent-configuration/speed (2026-11-08)",
    "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-08)",
];

/// Why a conditional tier estimate cannot decide a cost comparison.
pub const TIER_SENSITIVITY: &str = "effective service tier is absent or conflicting; project defaults and served tiers can differ from requests; tier sensitivity has no verified bounds, so affected cost-based recommendations are withheld";

/// Whether this Codex part lacks consistent effective-tier evidence.
pub fn conditional_tier(r: &Request, p: &BillingPart) -> bool {
    r.harness == Harness::Codex
        && (p.tier.is_none()
            || r.issues
                .iter()
                .any(|s| &**s == "conflicting effective service tiers"))
}

/// Whether this cost depends on unresolved Codex service-tier evidence.
pub fn tier_sensitive(cost: &Cost) -> bool {
    cost.assumptions.iter().any(|s| s == TIER_SENSITIVITY)
}

/// Normalize documented names for the same pricing tier.
pub(crate) fn tier_name(tier: &str) -> &str {
    match tier {
        "standard" => "default",
        "priority" => "fast",
        other => other,
    }
}

/// Choose a pricing scenario without promoting requested controls into billing evidence.
fn service_tier<'a>(r: &'a Request, p: &'a BillingPart) -> Result<&'a str, &'static str> {
    if r.issues
        .iter()
        .any(|s| &**s == "conflicting effective service tiers")
    {
        return Err("conflicting effective service tiers");
    }
    if let Some(tier) = p.tier.as_deref() {
        return Ok(tier);
    }
    if r.harness != Harness::Codex
        || r.provider.as_deref() != Some("openai")
        || r.api != Api::Responses
    {
        return Err("effective service tier is not recorded");
    }
    if r.issues
        .iter()
        .any(|s| &**s == "conflicting requested service tiers")
    {
        return Err("conflicting requested service tiers");
    }
    if p.speed.is_some() {
        return Err("recorded response speed without an effective service tier is unsupported");
    }
    if p.geo.as_deref().is_some_and(|geo| geo != "global") {
        return Err("regional rate is unsupported");
    }
    let requested = r.requested_speed.as_ref();
    let tier = requested.and_then(|s| s.tier.as_deref());
    let speed = requested.and_then(|s| s.speed.as_deref());
    if let (Some(tier), Some(speed)) = (tier, speed)
        && tier != "auto"
        && tier_name(tier) != tier_name(speed)
    {
        return Err("contradictory requested service tier and speed");
    }
    Ok(match tier.filter(|t| *t != "auto").or(speed) {
        None | Some("auto") => "default",
        Some(tier) => tier,
    })
}

/// Dollars per million tokens; absent rates stay unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Rates {
    input: Option<f64>,
    output: Option<f64>,
    read: Option<f64>,
    write: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Tier {
    size: f64,
    rates: Rates,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ModelPrice {
    base: Rates,
    tiers: Vec<Tier>,
    legacy: Option<Rates>,
    invalid: bool,
    /// The catalog's model family, such as `claude-haiku`.
    family: Option<String>,
    /// The ISO release date, which orders a family's models.
    release: Option<String>,
    /// Not deprecated, replies in text and calls tools, so it could run an agent.
    agentic: bool,
}

/// Catalog stores only numeric pricing metadata, keyed by exact provider and model.
///
/// SSH destinations receive it whole, so every host is priced from one catalog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

    /// The provider's cheapest current model that can run an agent: of the newest
    /// non-deprecated, tool-calling text model in each family, the one with the lowest
    /// output then input rate.
    pub fn cheapest(&self, provider: &str) -> Option<&str> {
        let mut newest: HashMap<&str, (&String, &ModelPrice)> = HashMap::default();
        for (name, price) in self.models.get(provider)? {
            let rates = price.base;
            if !price.agentic
                || price.invalid
                || rates.input.is_none()
                || rates.output.is_none()
                || rates.read.is_none()
            {
                continue;
            }
            let family = price.family.as_deref().unwrap_or(name);
            let newer = newest
                .get(family)
                .is_none_or(|(n, p)| (&price.release, name) > (&p.release, *n));
            if newer {
                newest.insert(family, (name, price));
            }
        }
        newest
            .into_values()
            .min_by(|(a, x), (b, y)| {
                let key = |p: &ModelPrice| (p.base.output, p.base.input);
                key(x)
                    .partial_cmp(&key(y))
                    .expect("finite rates")
                    .then(a.cmp(b))
            })
            .map(|(name, _)| name.as_str())
    }
}

/// Catalog edits for tests of missing public prices.
#[cfg(test)]
impl Catalog {
    pub(crate) fn clear_models(&mut self) {
        self.models.clear();
    }

    pub(crate) fn remove_model(&mut self, provider: &str, model: &str) {
        if let Some(models) = self.models.get_mut(provider) {
            models.remove(model);
        }
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

/// Dollars per million tokens no real model charges, so a larger rate is a malformed catalog
/// rather than a price; it keeps every charge far inside [`Usd`]'s range.
const MAX_RATE: f64 = 1e6;

fn rates(value: &Value) -> Result<Rates, ScanError> {
    let c = object(value);
    let price = |key| match rate(c.get(key))? {
        Some(r) if r > MAX_RATE => Err(ScanError::new("price catalog rate out of range")),
        r => Ok(r),
    };
    Ok(Rates {
        input: price("input")?,
        output: price("output")?,
        read: price("cache_read")?,
        write: price("cache_write")?,
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
    let model = object(value);
    let text_output = object(model.get("modalities").unwrap_or(&Value::Null))
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| output.iter().any(|m| m == "text"));
    Ok(ModelPrice {
        base: rates(cost)?,
        tiers,
        legacy,
        invalid,
        family: model.get("family").and_then(label),
        release: model.get("release_date").and_then(label),
        agentic: text_output
            && model.get("tool_call") == Some(&Value::Bool(true))
            && model.get("status").and_then(Value::as_str) != Some("deprecated"),
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

/// Partial downloads that a cancelled process must delete itself, because it exits without
/// waiting for a download that a stalled response may block indefinitely.
struct Downloads {
    cancelled: bool,
    files: Vec<PathBuf>,
}

static DOWNLOADS: Mutex<Downloads> = Mutex::new(Downloads {
    cancelled: false,
    files: Vec::new(),
});

fn downloads() -> MutexGuard<'static, Downloads> {
    DOWNLOADS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Delete every partial price download and refuse new ones, before a cancelled process exits.
pub fn cancel_downloads() {
    let mut downloads = downloads();
    downloads.cancelled = true;
    for file in downloads.files.drain(..) {
        let _ = fs::remove_file(file);
    }
}

/// Create a partial download, registered so cancellation can delete it.
fn create_temporary(temporary: &Path) -> io::Result<File> {
    let mut downloads = downloads();
    if downloads.cancelled {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // Cancellation deletes the file while the download still holds it open, which
    // Windows allows only when every handle shares delete access.
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 0x7);
    let file = options.open(temporary)?;
    downloads.files.push(temporary.to_path_buf());
    Ok(file)
}

/// Removes this operation's partial download however it ends; other downloads own other names.
struct Discard<'a>(&'a Path);

impl Drop for Discard<'_> {
    fn drop(&mut self) {
        let mut downloads = downloads();
        downloads.files.retain(|file| file != self.0);
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
    let file = create_temporary(temporary)?;
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
        known_usd: Usd::ZERO,
        total_usd: Some(Usd::ZERO),
        unknown: vec![],
        assumptions: vec![],
    }
}

/// Keep partial known charges separate from an unknown total.
pub fn add_costs<'a>(costs: impl IntoIterator<Item = &'a Cost>) -> Cost {
    let mut known = Usd::ZERO;
    let mut total = Some(Usd::ZERO);
    let mut unknown = IndexSet::default();
    let mut assumptions = IndexSet::default();
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

/// A running [`add_costs`], so charges need not be kept to be totaled.
#[derive(Clone, Debug)]
pub struct Sum {
    known: Usd,
    total: Option<Usd>,
    unknown: IndexSet<String>,
    assumptions: IndexSet<String>,
}

impl Default for Sum {
    fn default() -> Self {
        Self::new()
    }
}

impl Sum {
    /// The total of no costs.
    pub fn new() -> Self {
        Self {
            known: Usd::ZERO,
            total: Some(Usd::ZERO),
            unknown: IndexSet::default(),
            assumptions: IndexSet::default(),
        }
    }

    /// Add one cost, as the next item of [`add_costs`] would.
    pub fn add(&mut self, cost: &Cost) {
        self.add_scaled(cost, 1.0);
    }

    /// Add `scale(cost, weight)` without building it.
    pub fn add_scaled(&mut self, cost: &Cost, weight: f64) {
        if weight == 0.0 {
            return;
        }
        self.known += cost.known_usd.scale(weight);
        self.total = match (self.total, cost.total_usd) {
            (Some(sum), Some(value)) => Some(sum + value.scale(weight)),
            _ => None,
        };
        for reason in &cost.unknown {
            if !self.unknown.contains(reason.as_str()) {
                self.unknown.insert(reason.clone());
            }
        }
        for assumption in &cost.assumptions {
            if !self.assumptions.contains(assumption.as_str()) {
                self.assumptions.insert(assumption.clone());
            }
        }
    }

    /// The total so far.
    pub fn cost(self) -> Cost {
        Cost {
            known_usd: self.known,
            total_usd: self.total,
            unknown: self.unknown.into_iter().collect(),
            assumptions: self.assumptions.into_iter().collect(),
        }
    }
}

/// Baseline-minus-alternative savings over the items both scenarios price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairedSavings {
    pub usd: Usd,
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
    let mut usd = Usd::ZERO;
    let mut priced = 0;
    let mut count = 0;
    let mut largest = Usd::ZERO;
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
        decided: unpriced == 0 || (priced > 0 && usd.abs() > largest.saturating_mul(unpriced)),
    }
}

/// Represent missing pricing without substituting a different model.
pub fn unknown_cost(reason: impl Into<String>) -> Cost {
    Cost {
        known_usd: Usd::ZERO,
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

/// Alternative tier totals for missing-tier Codex usage; unsupported alternatives stay unknown.
pub fn tier_sensitivity(catalog: &Catalog, requests: &[&Request]) -> Vec<TierScenario> {
    if !requests
        .iter()
        .any(|r| r.parts.iter().any(|p| conditional_tier(r, p)))
    {
        return vec![];
    }
    let mut scenarios =
        ["default", "fast", "ultrafast", "flex", "scale"].map(|tier| TierScenario {
            tier,
            cost: zero_cost(),
        });
    for r in requests {
        let parts: Vec<_> = r
            .parts
            .iter()
            .map(|p| {
                (
                    price_part(catalog, r, p),
                    conditional_tier(r, p) && selected_rates(catalog, r, p).is_ok(),
                )
            })
            .collect();
        let server = server_cost(r);
        for scenario in &mut scenarios {
            let costs: Vec<_> = r
                .parts
                .iter()
                .zip(&parts)
                .map(|(p, (cost, conditional))| {
                    if !conditional {
                        return cost.clone();
                    }
                    if ["flex", "scale"].contains(&scenario.tier) {
                        return unknown_cost(
                            "effective service tier has no supported catalog rate",
                        );
                    }
                    let mut p = p.clone();
                    p.tier = Some(scenario.tier.into());
                    price_part(catalog, r, &p)
                })
                .chain([server.clone()])
                .collect();
            let cost = request_evidence(r, add_costs(&costs));
            scenario.cost = add_costs([&scenario.cost, &cost]);
        }
    }
    for scenario in &mut scenarios {
        scenario.cost.assumptions.push("tier sensitivity only: assign this served tier to conditionally priced parts; recorded tiers remain authoritative; these scenarios do not bound actual costs or subscription charges".into());
    }
    scenarios.into()
}

/// Pricing assumptions for the same selected rates used to place context pieces.
pub fn pricing_assumptions(catalog: &Catalog, r: &Request, p: &BillingPart) -> Vec<String> {
    selected_rates(catalog, r, p)
        .map(|s| s.assumptions)
        .unwrap_or_default()
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
    let tier = service_tier(r, p)?;
    if conditional_tier(r, p) {
        assumptions.push(format!(
            "conditional {tier}-rate OpenAI API equivalent: effective service tier is unrecorded; requested tier/speed is a scenario, not billing evidence; omitted/auto can inherit nonstandard project defaults; current config is not historical evidence; not a subscription bill"
        ));
        assumptions.push(TIER_SENSITIVITY.into());
        if p.geo.is_none() {
            assumptions.push("unrecorded geography assumed global API equivalent".into());
        }
    }
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

/// One part's per-token output price; None when unpriced.
pub fn output_rate(catalog: &Catalog, r: &Request, p: &BillingPart) -> Option<f64> {
    Some(selected_rates(catalog, r, p).ok()?.rates.output? / 1e6)
}

/// One part's input as consecutive spans of its prompt prefix, each with its
/// token count and per-token price: cache reads, then cache writes, then
/// ordinary input, which runs to the end. None when usage or a price is missing,
/// or when two write lifetimes leave their order in the prefix unknown.
pub fn prefix_spans(catalog: &Catalog, r: &Request, p: &BillingPart) -> Option<Vec<(f64, f64)>> {
    let u = &p.usage;
    let rates = selected_rates(catalog, r, p).ok()?.rates;
    let (input, read) = (rates.input? / 1e6, rates.read? / 1e6);
    let mut spans = vec![(u.read? as f64, read)];
    if r.harness == Harness::Claude {
        if u.write? > 0 {
            // Longer lifetimes must precede shorter ones in a Claude prompt.
            spans.push((u.write1h? as f64, input * 2.0));
            spans.push((u.write5m? as f64, rates.write? / 1e6));
        }
    } else if write_billing(p.model.as_deref()?)? {
        // Only a model that bills writes needs them recorded; otherwise uncached
        // input is ordinary.
        let written = u.write?;
        if written > 0 {
            match (u.write30m.unwrap_or(0), u.write12h.unwrap_or(0)) {
                (0, 0) => spans.push((written as f64, input * 1.25)),
                (short, 0) => spans.push((short as f64, input * 1.25)),
                (0, long) => spans.push((long as f64, input * 2.5)),
                _ => return None,
            }
        }
    }
    u.input?;
    spans.push((f64::INFINITY, input));
    Some(spans)
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
            cost.known_usd += Usd::from_f64((t as f64 * rate.unwrap_or(0.0)) / 1e6);
        }
        _ => cost.unknown.push(component.to_string()),
    }
}

/// Price ordinary input, which is assigned rather than accumulated.
fn ordinary_charge(cost: &mut Cost, tokens: Option<i64>, rate: Option<f64>) {
    match tokens {
        Some(t) if t <= 0 || rate.is_some() => {
            cost.known_usd = Usd::from_f64((t as f64 * rate.unwrap_or(0.0)) / 1e6);
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
            let mut cost = unknown_cost(reason);
            if conditional_tier(r, p) {
                cost.assumptions.push(TIER_SENSITIVITY.into());
            }
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
            output.known_usd = Usd::from_f64((o as f64 * rates.output.unwrap_or(0.0)) / 1e6);
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
    for (name, requests) in tools.iter() {
        if name == "web_search_requests" && r.provider.as_deref() == Some("anthropic") {
            result.known_usd += Usd::from_f64(requests as f64 * 0.01);
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
    request_evidence(r, add_costs(&parts))
}

/// A request's priced parts and fees, unknown when its own record says usage is missing.
pub(crate) fn request_evidence(r: &Request, mut result: Cost) -> Cost {
    let mut assumptions: IndexSet<String> = result.assumptions.drain(..).collect();
    for issue in &r.issues {
        if !assumptions.contains(&**issue) {
            assumptions.insert(issue.to_string());
        }
    }
    result.assumptions = assumptions.into_iter().collect();
    if r.issues.iter().any(|issue| {
        issue.contains("no verified request delta")
            || issue.contains("usage is absent")
            || issue.contains("no linked assistant usage")
    }) {
        result.total_usd = None;
        result
            .unknown
            .extend(r.issues.iter().map(|issue| issue.to_string()));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Text, Usage};
    use crate::test_support::{ClaudeUsage, claude_usage, make_request, prices};
    use serde_json::json;

    /// Recorded Codex usage, normalized as the reader does.
    fn codex(raw: Value) -> Usage {
        crate::reader::usage(&raw, Harness::Codex).unwrap()
    }

    fn codex_request() -> Request {
        {
            let mut built = Request { ..make_request() };
            built.harness = Harness::Codex;
            built.provider = Some("openai".into());
            built.api = Api::Responses;
            built
        }
    }

    #[test]
    fn missing_codex_tiers_are_conditional_without_changing_billing_evidence() {
        let c = prices();
        let mut r = codex_request();
        r.parts[0] = BillingPart {
            model: Some("gpt-6.1-sol".into()),
            tier: None,
            speed: None,
            geo: None,
            usage: codex(json!({"input_tokens": 272001, "output_tokens": 100,
                "cached_input_tokens": 200, "cache_write_input_tokens": 100,
                "cache_creation_30m_input_tokens": 100})),
            ..r.parts[0].clone()
        };
        let original = r.clone();
        let standard = price_request(&c, &r);
        assert!(standard.total_usd.is_some());
        assert!(standard.assumptions.join(" ").contains("conditional"));
        for tier in ["default", "fast", "priority", "ultrafast"] {
            r.requested_speed = Some(crate::model::RequestedSpeed {
                tier: Some(tier.into()),
                speed: None,
            });
            let cost = price_request(&c, &r);
            let multiplier = match tier {
                "default" => 1.0,
                "ultrafast" => 6.0,
                _ => 2.0,
            };
            assert!(close(
                cost.total_usd.map(Usd::to_f64),
                standard.total_usd.unwrap().to_f64() * multiplier
            ));
            assert_eq!(r.parts, original.parts);
        }
        r.parts[0].tier = Some("default".into());
        assert_eq!(price_request(&c, &r).total_usd, standard.total_usd);
        assert!(
            !price_request(&c, &r)
                .assumptions
                .join(" ")
                .contains("conditional")
        );
    }

    #[test]
    fn conditional_tiers_keep_unsupported_and_contradictory_dimensions_unknown() {
        let c = prices();
        let mut r = codex_request();
        r.parts[0] = BillingPart {
            model: Some("gpt-5.4".into()),
            tier: None,
            speed: None,
            geo: None,
            usage: codex(json!({"input_tokens": 1000, "output_tokens": 100,
                "cached_input_tokens": 200})),
            ..r.parts[0].clone()
        };
        for tier in [None, Some("auto"), Some("default")] {
            r.requested_speed = Some(crate::model::RequestedSpeed {
                tier: tier.map(Text::from),
                speed: None,
            });
            assert!(close(
                price_request(&c, &r).total_usd.map(Usd::to_f64),
                0.00355
            ));
        }
        for tier in ["flex", "scale", "unknown", "ultrafast"] {
            r.requested_speed.as_mut().unwrap().tier = Some(tier.into());
            assert_eq!(price_request(&c, &r).total_usd, None, "{tier}");
        }
        r.requested_speed = Some(crate::model::RequestedSpeed {
            tier: Some("default".into()),
            speed: Some("fast".into()),
        });
        assert_eq!(price_request(&c, &r).total_usd, None);
        r.parts[0].tier = Some("default".into());
        assert!(price_request(&c, &r).total_usd.is_some());
        r.parts[0].tier = Some("auto".into());
        assert_eq!(price_request(&c, &r).total_usd, None);
        r.parts[0].tier = None;
        r.requested_speed = None;
        for issue in [
            "conflicting effective service tiers",
            "conflicting requested service tiers",
        ] {
            r.issues = vec![issue.into()];
            assert_eq!(price_request(&c, &r).total_usd, None);
        }
        r.issues.clear();
        r.provider = Some("custom".into());
        assert_eq!(price_request(&c, &r).total_usd, None);
        r.provider = Some("openai".into());
        r.parts[0].model = Some("unknown".into());
        assert_eq!(price_request(&c, &r).total_usd, None);
        r.parts[0].model = Some("gpt-6.1-sol".into());
        // This generation bills cache writes separately; absent writes stay unknown.
        assert_eq!(price_request(&c, &r).total_usd, None);
        r.parts[0].usage = Usage::default();
        assert_eq!(price_request(&c, &r).total_usd, None);
    }

    #[test]
    fn tier_scenarios_preserve_recorded_parts_and_unknown_alternatives() {
        let c = prices();
        let mut r = codex_request();
        r.parts[0] = BillingPart {
            model: Some("gpt-6.1-sol".into()),
            tier: None,
            speed: None,
            geo: None,
            usage: codex(json!({"input_tokens": 272000, "output_tokens": 100,
                "cached_input_tokens": 200, "cache_write_input_tokens": 100})),
            ..r.parts[0].clone()
        };
        let mut recorded = r.parts[0].clone();
        recorded.tier = Some("priority".into());
        r.parts.push(recorded);
        let original = r.clone();
        let rows = tier_sensitivity(&c, &[&r]);
        let unit = price_part(&c, &r, &r.parts[0]).total_usd.unwrap().to_f64();
        assert!(close(rows[0].cost.total_usd.map(Usd::to_f64), unit * 3.0));
        assert!(close(rows[1].cost.total_usd.map(Usd::to_f64), unit * 4.0));
        assert!(close(rows[2].cost.total_usd.map(Usd::to_f64), unit * 8.0));
        assert_eq!(rows[3].cost.total_usd, None);
        assert_eq!(rows[4].cost.total_usd, None);
        assert_eq!(r, original);
        r.issues.push("usage is absent".into());
        assert!(
            tier_sensitivity(&c, &[&r])
                .iter()
                .all(|s| s.cost.total_usd.is_none())
        );
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        (a.unwrap() - b).abs() < 1e-10
    }

    /// Cancellation is sticky for the process; no other test here downloads.
    #[test]
    fn cancellation_deletes_partial_downloads_and_refuses_new_ones() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!("cancel-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("models.json.partial.tmp");
        let finished = dir.join("models.json.finished.tmp");
        let mut file = create_temporary(&partial).unwrap();
        file.write_all(b"{\"anthropic\":").unwrap();
        create_temporary(&finished).unwrap();
        drop(Discard(&finished));
        assert!(!finished.exists());
        assert_eq!(downloads().files, vec![partial.clone()]);
        cancel_downloads();
        // Without POSIX delete semantics, Windows hides a deleted file only once its last handle closes.
        drop(file);
        assert!(!partial.exists());
        assert!(create_temporary(&dir.join("models.json.late.tmp")).is_err());
        assert!(!dir.join("models.json.late.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn openai_cache_reads_and_writes_replace_ordinary_input_and_reasoning_stays_a_subset() {
        let catalog = prices();
        let r = codex_request();
        let p = BillingPart {
            model: Some("gpt-6.1-sol".into()),
            usage: codex(json!({
                "input_tokens": 12000,
                "output_tokens": 100,
                "input_tokens_details": {
                    "cached_tokens": 4000,
                    "cache_write_tokens": 7000,
                    "cache_write_tokens_details": {
                        "ttl_30m_tokens": 3000,
                        "ttl_12h_tokens": 4000,
                    },
                },
                "output_tokens_details": { "reasoning_tokens": 20 },
            })),
            ..r.parts[0].clone()
        };
        let result = price_part(&catalog, &r, &p);
        assert!(close(
            result.total_usd.map(Usd::to_f64),
            (1000.0 * 2.0 + 4000.0 * 0.1 + 3000.0 * 2.5 + 4000.0 * 5.0 + 100.0 * 10.0) / 1e6
        ));
        let missing = price_part(
            &catalog,
            &r,
            &BillingPart {
                usage: codex(json!({
                    "input_tokens": 12000,
                    "output_tokens": 100,
                    "cached_input_tokens": 4000,
                })),
                ..p.clone()
            },
        );
        assert_eq!(missing.total_usd, None);
        assert!(missing.unknown.join(" ").contains("cache writes missing"));
        let unknown_api = {
            let mut built = Request { ..r.clone() };
            built.api = Api::Unknown;
            built
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
            usage: codex(json!({
                "input_tokens": 1000,
                "output_tokens": 100,
                "cached_input_tokens": 200,
            })),
            ..r.parts[0].clone()
        };
        assert_eq!(
            price_part(&c, &r, &p).total_usd.map(Usd::to_f64),
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
            usage: codex(json!({
                "input_tokens": 250000,
                "output_tokens": 100,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0,
            })),
            ..r.parts[0].clone()
        };
        let with_input = |input| BillingPart {
            usage: Usage {
                input: Some(input),
                ..p.usage
            },
            ..p.clone()
        };
        assert_eq!(
            price_part(&c, &r, &p).total_usd.map(Usd::to_f64),
            Some(0.501)
        );
        assert_eq!(
            price_part(&c, &r, &with_input(272000))
                .total_usd
                .map(Usd::to_f64),
            Some(0.545)
        );
        assert_eq!(
            price_part(&c, &r, &with_input(272001))
                .total_usd
                .map(Usd::to_f64),
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
        assert_eq!(
            price_part(&c, &h, &h.parts[0]).total_usd.map(Usd::to_f64),
            Some(0.0500005)
        );
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
            cost.total_usd.map(Usd::to_f64),
            ((1000.0 * 8.0 + 200.0 * 0.2 + 100.0 * 20.0) * 2.0 * 1.1) / 1e6
        ));
        let priority = BillingPart {
            tier: Some("priority".into()),
            ..p.clone()
        };
        assert_eq!(price_part(&c, &r, &priority).total_usd, None);
        let custom = {
            let mut built = Request { ..r.clone() };
            built.provider = Some("custom".into());
            built
        };
        assert_eq!(price_part(&c, &custom, &p).total_usd, None);
        let tools = |entries: &[(&str, i64)]| {
            let mut built = Request { ..r.clone() };
            built.server_tools = Some(entries.iter().map(|&(k, v)| (k.to_string(), v)).collect());
            built
        };
        assert_eq!(
            server_cost(&tools(&[
                ("web_search_requests", 2),
                ("web_fetch_requests", 3)
            ]))
            .total_usd
            .map(Usd::to_f64),
            Some(0.02)
        );
        assert_eq!(
            server_cost(&tools(&[("code_execution_requests", 1)])).total_usd,
            None
        );
        let unrecorded = {
            let mut built = Request {
                parts: vec![p],
                ..r
            };
            built.server_tools = None;
            built
        };
        assert_eq!(price_request(&c, &unrecorded).total_usd, None);
    }

    #[test]
    fn unavailable_claude_geography_uses_the_labelled_global_api_equivalent() {
        let r = make_request();
        let c = prices();
        let with_geo = |geo: Option<&str>| BillingPart {
            geo: geo.map(Text::from),
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
    fn cost_totals_do_not_depend_on_the_order_or_grouping_of_their_parts() {
        // Charges spanning many magnitudes, as per-token prices produce, from a fixed LCG.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            state
        };
        let costs: Vec<Cost> = (0..1000)
            .map(|_| {
                let mantissa = (next() >> 11) as f64 / (1u64 << 53) as f64;
                let usd = mantissa * 10f64.powi((next() % 12) as i32 - 9);
                Cost {
                    known_usd: Usd::from_f64(usd),
                    total_usd: Some(Usd::from_f64(usd)),
                    ..Cost::default()
                }
            })
            .collect();
        let forward = add_costs(&costs);
        assert_eq!(add_costs(costs.iter().rev()), forward);
        // Partial sums merged in another order, as a per-session fold would.
        let mut merged = Sum::new();
        for chunk in costs.chunks(37).rev() {
            merged.add(&add_costs(chunk));
        }
        assert_eq!(merged.cost(), forward);
        let parts = costs.iter().map(|c| (c, &costs[0]));
        let mut reversed: Vec<_> = parts.clone().collect();
        reversed.reverse();
        assert_eq!(paired_savings(parts, 0), paired_savings(reversed, 0));
        // f64 addition is not associative, so the old sums differed here.
        let f64s: Vec<f64> = costs.iter().map(|c| c.known_usd.to_f64()).collect();
        assert_ne!(
            f64s.iter().sum::<f64>().to_bits(),
            f64s.iter().rev().sum::<f64>().to_bits()
        );
    }

    #[test]
    fn dollar_amounts_round_once_to_the_grid() {
        // Amounts of at least 2^-12 dollars are on the grid already.
        for usd in [0.0, 0.501, -1.089504, 3.849, 1e6] {
            assert_eq!(Usd::from_f64(usd).to_f64(), usd);
        }
        let step = 2f64.powi(-64);
        assert_eq!(Usd::from_f64(step * 0.4), Usd::ZERO);
        assert_eq!(Usd::from_f64(step * 0.6).to_f64(), step);
        assert_eq!(Usd::from_f64(-step * 0.6).to_f64(), -step);
        assert!(std::panic::catch_unwind(|| Usd::from_f64(f64::NAN)).is_err());
        assert!(std::panic::catch_unwind(|| Usd::from_f64(1e40)).is_err());
    }

    #[test]
    fn paired_savings_decide_only_when_unpriced_parts_cannot_flip_the_sign() {
        let known = |usd: f64| Cost {
            known_usd: Usd::from_f64(usd),
            total_usd: Some(Usd::from_f64(usd)),
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
                usd: Usd::from_f64(3.0),
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
            parse(r#"{"p":{"models":{"m":{"cost":{"output":1e30}}}}}"#).unwrap_err(),
            "price catalog rate out of range"
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

    // Ported from fast.test.ts: "OpenAI fast and ultrafast API repricing preserves long context and billed write lifetimes".
    #[test]
    fn openai_fast_and_ultrafast_repricing_preserves_long_context_and_billed_write_lifetimes() {
        let catalog = prices();
        let p = BillingPart {
            model: Some("gpt-6.1-sol".into()),
            speed: None,
            tier: Some("default".into()),
            usage: crate::reader::usage(
                &serde_json::json!({
                    "input_tokens": 300000,
                    "output_tokens": 100,
                    "input_tokens_details": {
                        "cached_tokens": 200000,
                        "cache_write_tokens": 50000,
                        "cache_write_tokens_details": {
                            "ttl_30m_tokens": 20000,
                            "ttl_12h_tokens": 30000,
                        },
                    },
                }),
                Harness::Codex,
            )
            .unwrap(),
            ..make_request().parts[0].clone()
        };
        let r = Request {
            parts: vec![p.clone()],
            ..codex_request()
        };
        let normal = price_part(&catalog, &r, &p).total_usd.unwrap().to_f64();
        for tier in ["fast", "priority", "ultrafast"] {
            let multiplier = if tier == "ultrafast" { 6.0 } else { 2.0 };
            let repriced = BillingPart {
                tier: Some(tier.into()),
                ..p.clone()
            };
            // Each component is priced at the higher rate on its own, so the total matches
            // six times the standard one only to rounding.
            let repriced = price_part(&catalog, &r, &repriced)
                .total_usd
                .unwrap()
                .to_f64();
            assert!((repriced - normal * multiplier).abs() < 1e-12, "{tier}");
        }
        let luna = BillingPart {
            tier: Some("ultrafast".into()),
            model: Some("gpt-6-luna".into()),
            ..p.clone()
        };
        assert_eq!(price_part(&catalog, &r, &luna).total_usd, None);
        let unbilled = BillingPart {
            tier: Some("fast".into()),
            usage: Usage {
                write: None,
                ..p.usage
            },
            ..p
        };
        assert_eq!(price_part(&catalog, &r, &unbilled).total_usd, None);
    }

    // Ported from fast.test.ts: "plain GPT-5.6 alias has verified fast rates without substituting catalog model prices".
    #[test]
    fn plain_gpt_5_6_alias_has_verified_fast_rates_without_substituting_catalog_prices() {
        let mut c = prices();
        let r = codex_request();
        let p = BillingPart {
            model: Some("gpt-5.6".into()),
            usage: crate::reader::usage(
                &serde_json::json!({
                    "input_tokens": 300000,
                    "output_tokens": 100,
                    "cached_input_tokens": 200000,
                    "cache_write_input_tokens": 0,
                }),
                Harness::Codex,
            )
            .unwrap(),
            tier: Some("default".into()),
            speed: None,
            ..r.parts[0].clone()
        };
        let standard = price_part(&c, &r, &p).total_usd.unwrap().to_f64();
        assert!(standard > 0.0);
        let fast = BillingPart {
            tier: Some("fast".into()),
            ..p
        };
        assert_eq!(
            price_part(&c, &r, &fast).total_usd.map(Usd::to_f64),
            Some(standard * 2.0)
        );
        c.remove_model("openai", "gpt-5.6");
        assert_eq!(price_part(&c, &r, &fast).total_usd, None);
    }

    #[test]
    fn the_cheapest_model_is_the_newest_agentic_model_of_the_cheapest_family() {
        let model = |family: &str, release: &str, output: f64, extra: &str| {
            format!(
                r#"{{"family":"{family}","release_date":"{release}","tool_call":true,"modalities":{{"output":["text"]}},"cost":{{"input":1,"output":{output},"cache_read":0.1}}{extra}}}"#
            )
        };
        let catalog = parse_catalog(
            format!(
                r#"{{"p":{{"models":{{"small-old":{},"small-new":{},"big":{},"retired":{},"image":{},"unpriced":{}}}}}}}"#,
                model("small", "2026-01-01", 1.0, ""),
                model("small", "2026-06-01", 2.0, ""),
                model("big", "2026-06-01", 5.0, ""),
                model("retired", "2026-06-01", 0.5, r#","status":"deprecated""#),
                r#"{"family":"image","release_date":"2026-06-01","tool_call":true,"modalities":{"output":["image"]},"cost":{"input":1,"output":0.1,"cache_read":0.1}}"#,
                r#"{"family":"unpriced","release_date":"2026-06-01","tool_call":true,"modalities":{"output":["text"]},"cost":{"input":1,"output":0.1}}"#,
            )
            .as_bytes(),
        )
        .ok()
        .unwrap();
        // The older, cheaper model of a family is no longer current.
        assert_eq!(catalog.cheapest("p"), Some("small-new"));
        assert_eq!(catalog.cheapest("other"), None);
        let fixture = crate::test_support::prices();
        assert_eq!(fixture.cheapest("anthropic"), Some("claude-haiku-5-5"));
        assert_eq!(fixture.cheapest("openai"), Some("gpt-6.1-sol"));
    }
}
