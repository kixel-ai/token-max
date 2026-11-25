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

use crate::model::{
    Api, BillingPart, Cost, Estimate, Harness, Margin, PartKind, Request, Text, TierScenario, Usd,
};
use crate::safe::{ScanError, iso, label, object, truthy};

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

/// Why a cost comparison priced at an assumed tier still decides: every priced session moved
/// the same way.
pub const TIER_SIGNS: &str = "effective service tier is absent or conflicting, so dollars are priced at an assumed tier; each session's saving at an assumed tier, apart wherever one tier would scale its charges differently, such as on another model or assumed tier, and the saving at recorded tiers, went the same way, and a service tier multiplies all of a request's token rates by one positive factor, so no tier could reverse the verdict, assuming one tier bills every request of a session in both replays";

/// What a part priced at an assumed tier scales with when another tier bills it: the
/// documented default, fast and ultrafast rates relative to the assumed one's, or, where one
/// is unknown, its model, its assumed tier and which side of the model's context threshold
/// its input falls on.
/// Parts that share one scale alike under any tier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TierScale {
    /// Each tier's rate per thousand of the assumed tier's.
    ratios: Option<[u64; 3]>,
    model: Option<Text>,
    assumed: Option<Text>,
    long: bool,
}

/// The scale of a part priced at an assumed tier; None for a recorded tier or an unpriced part.
pub fn tier_scale(r: &Request, p: &BillingPart) -> Option<TierScale> {
    if !conditional_tier(r, p) {
        return None;
    }
    let assumed = service_tier(r, p).ok()?;
    let model = p.model.as_deref().or(r.model.as_deref()).unwrap_or("");
    let input = p.usage.input;
    let rate = |tier: &str| match tier {
        "default" | "standard" => Some(1.0),
        tier => premium_multiplier(tier, model, input),
    };
    let ratios = rate(assumed).and_then(|base| {
        let tiers = [rate("default")?, rate("fast")?, rate("ultrafast")?];
        Some(tiers.map(|m| (m / base * 1000.0).round() as u64))
    });
    if ratios.is_some() {
        return Some(TierScale {
            ratios,
            model: None,
            assumed: None,
            long: false,
        });
    }
    let limit = r
        .provider
        .as_deref()
        .and_then(|provider| threshold(provider, model));
    Some(TierScale {
        ratios,
        model: Some(model.into()),
        assumed: Some(tier_name(assumed).into()),
        long: input.zip(limit).is_some_and(|(input, limit)| input > limit),
    })
}

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
    // An advisor call records no tier of its own, and Priority Tier needs a commitment on
    // each model, so under an executor at standard the call is too, barring a commitment on
    // the advisor model alone.
    if p.kind == PartKind::Advisor {
        let mut executor = r
            .parts
            .iter()
            .filter(|e| e.kind == PartKind::Message)
            .map(|e| e.tier.as_deref().map(tier_name));
        let first = executor.next().flatten();
        return match first {
            Some("default") if executor.all(|t| t == first) => Ok("standard"),
            _ => Err("an advisor call's service tier is not recorded"),
        };
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
        // A legacy tier field counts only when set: not null, false, zero or an empty string.
        Some(value) if truthy(Some(value)) => Some(rates(value)?),
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

/// Explicit offline and fixture modes never fetch prices.
#[derive(Clone, Debug, Default)]
pub struct PriceOptions {
    /// A prices file to read instead of fetching.
    pub file: Option<PathBuf>,
    /// Where fetched prices are cached.
    pub cache: PathBuf,
    /// Read the file or cache without ever fetching.
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

/// Delete every partial download, of prices or a release, and refuse new ones, before a
/// cancelled process exits.
pub fn cancel_downloads() {
    let mut downloads = downloads();
    downloads.cancelled = true;
    for file in downloads.files.drain(..) {
        let _ = fs::remove_file(file);
    }
}

/// Create a partial download, registered so cancellation can delete it.
pub(crate) fn create_temporary(temporary: &Path) -> io::Result<File> {
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
pub(crate) struct Discard(pub(crate) PathBuf);

impl Drop for Discard {
    fn drop(&mut self) {
        let mut downloads = downloads();
        downloads.files.retain(|file| file != &self.0);
        let _ = fs::remove_file(&self.0);
    }
}

fn millis(time: SystemTime) -> Option<i64> {
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// A name no concurrent download in this or another process will pick.
pub(crate) fn unique() -> String {
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
    let _discard = Discard(temporary.clone());
    match download(&options.cache, &temporary) {
        Ok(catalog) => Ok(Catalog {
            fetched_at: Some(iso(crate::reader::now())),
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
    known_cost(Usd::ZERO)
}

/// A fully known amount with no missing components.
pub fn known_cost(usd: Usd) -> Cost {
    Cost {
        known_usd: usd,
        total_usd: Some(usd),
        unknown: vec![],
        assumptions: vec![],
    }
}

/// Keep partial known charges separate from an unknown total.
pub fn add_costs<'a>(costs: impl IntoIterator<Item = &'a Cost>) -> Cost {
    let mut sum = Sum::new();
    for cost in costs {
        sum.add(cost);
    }
    sum.cost()
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

/// The z-score of a two-sided 95% normal interval.
const Z95: f64 = 1.959_963_984_540_054;

/// The two-sided 95% Student-t critical value for `df` degrees of freedom, at least 1, so a
/// margin estimated from a few priced items widens as it should; the normal value past 30.
fn critical95(df: u64) -> f64 {
    const T: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    T.get(df as usize - 1).copied().unwrap_or(Z95)
}

/// Per-item dollar figures summed over the items that could be priced, so their total over
/// every item can be extrapolated with a margin of error.
///
/// An item may carry a weight, such as a session's idle periods, so the total is
/// extrapolated per unit of weight rather than per item: a ratio estimator. With every
/// weight 1 it is the plain mean times the item count.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    priced: u64,
    weight: f64,
    /// Summed exactly, so a total priced in full doesn't depend on the order of its items.
    sum: Usd,
    squares: f64,
    cross: f64,
    weights: f64,
}

impl Sample {
    /// Add one priced item's figure.
    pub fn add(&mut self, usd: Usd) {
        self.add_weighted(usd, 1.0);
    }

    /// Add one priced item's figure and its weight.
    pub fn add_weighted(&mut self, exact: Usd, weight: f64) {
        let usd = exact.to_f64();
        self.priced += 1;
        self.weight += weight;
        self.sum += exact;
        self.squares += usd * usd;
        self.cross += usd * weight;
        self.weights += weight * weight;
    }

    /// Take back one item [`Sample::add`] added, as when a request read so far changes.
    pub fn remove(&mut self, exact: Usd) {
        let usd = exact.to_f64();
        self.priced -= 1;
        self.weight -= 1.0;
        self.sum -= exact;
        self.squares -= usd * usd;
        self.cross -= usd;
        self.weights -= 1.0;
    }

    /// Add one item's cost when it was priced in full; a partly priced one is left for the
    /// estimate to cover like any other unpriced item.
    pub fn add_cost(&mut self, cost: &Cost) {
        if let Some(usd) = cost.total_usd {
            self.add(usd);
        }
    }

    /// How many items were priced.
    pub fn priced(&self) -> u64 {
        self.priced
    }

    /// What the priced items cost.
    pub fn sum(&self) -> Usd {
        self.sum
    }

    /// The figure over `total` items, `total` at least the priced ones, extrapolated from the
    /// priced items' mean under the assumption that the rest resemble them; None when some
    /// item exists and none was priced.
    pub fn estimate(&self, total: u64) -> Option<Estimate> {
        self.estimate_weighted(total, total as f64)
    }

    /// The figure over `total` items weighing `weight` in all, extrapolated per unit of the
    /// priced items' weight; None when some item exists and none was priced.
    ///
    /// The margin is a 95% interval's half-width for a total estimated from a simple random
    /// sample of the items, with the finite-population correction, so it shrinks to nothing
    /// as the priced share reaches every item. It is a statistical estimate, not a bound: an
    /// unpriced item has no price to bound it by.
    pub fn estimate_weighted(&self, total: u64, weight: f64) -> Option<Estimate> {
        assert!(
            total >= self.priced,
            "every priced item is one of the total"
        );
        if self.priced == 0 && total > 0 {
            return None;
        }
        if total == self.priced {
            return Some(Estimate {
                usd: self.sum.to_f64(),
                margin: None,
            });
        }
        let ratio = self.sum.to_f64() / self.weight;
        let usd = ratio * weight;
        let margin = if self.priced < 2 {
            Margin::Unmeasured
        } else {
            let (n, total) = (self.priced as f64, total as f64);
            // The residuals' spread around the ratio; rounding can leave it a hair below zero.
            let residuals = self.squares - 2.0 * ratio * self.cross + ratio * ratio * self.weights;
            let variance = (residuals / (n - 1.0)).max(0.0);
            let critical = critical95(self.priced - 1);
            Margin::Usd(critical * total * ((1.0 - n / total) * variance / n).sqrt())
        };
        Some(Estimate {
            usd,
            margin: Some(margin),
        })
    }
}

/// Pool another sample of different items, as though each had been added here.
impl std::ops::AddAssign for Sample {
    fn add_assign(&mut self, other: Self) {
        self.priced += other.priced;
        self.weight += other.weight;
        self.sum += other.sum;
        self.squares += other.squares;
        self.cross += other.cross;
        self.weights += other.weights;
    }
}

/// Baseline-minus-alternative savings over the items both scenarios price, and each
/// scenario's cost over them, every figure extrapolated to all the items.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairedSavings {
    /// Baseline minus alternative; None when no item was priced in both.
    pub savings: Option<Estimate>,
    pub baseline: Option<Estimate>,
    pub alternative: Option<Estimate>,
    pub priced: u64,
    pub total: u64,
}

impl PairedSavings {
    /// The extrapolated savings in dollars, positive when the alternative is cheaper.
    pub fn usd(&self) -> Option<f64> {
        self.savings.map(|s| s.usd)
    }
}

/// Which ways a comparison's priced savings went: each session's at an assumed tier, split by
/// [`TierScale`], and all of those at recorded tiers together. One tier scales a session's
/// charges of one scale by one positive factor, so when one tier bills each session and no
/// saving went the other way, no tier could reverse the total's sign; one that did could be
/// scaled past the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Signs {
    saves: bool,
    loses: bool,
}

impl Signs {
    /// Count one baseline-minus-alternative saving.
    pub fn add(&mut self, savings: Usd) {
        self.saves |= savings > Usd::ZERO;
        self.loses |= savings < Usd::ZERO;
    }

    /// Whether no saving was negative, so the alternative costs no more under any tier.
    pub fn never_loses(&self) -> bool {
        !self.loses
    }

    /// Whether no saving was positive, so the alternative saves nothing under any tier.
    pub fn never_saves(&self) -> bool {
        !self.saves
    }
}

/// A comparison's priced savings grouped as [`Signs`] counts them: by session and
/// [`TierScale`] at an assumed tier, and all together at recorded tiers.
#[derive(Clone, Debug, Default)]
pub struct TierSavings(HashMap<Option<(Text, TierScale)>, Usd>);

impl TierSavings {
    /// Add a saving of `usd` on `session` at `scale`, None for a recorded tier: a baseline
    /// charge as itself and an alternative's negated.
    pub fn add(&mut self, session: &Text, scale: Option<&TierScale>, usd: Usd) {
        let key = scale.map(|scale| (session.clone(), scale.clone()));
        *self.0.entry(key).or_default() += usd;
    }

    /// Which ways the groups went.
    pub fn signs(&self) -> Signs {
        let mut signs = Signs::default();
        for &usd in self.0.values() {
            signs.add(usd);
        }
        signs
    }
}

/// Compares two scenarios pair by pair as (baseline, alternative), so a few unpriced requests
/// cannot hide a clear winner, without keeping a whole history's pairs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pairs {
    count: u64,
    savings: Sample,
    baseline: Sample,
    alternative: Sample,
}

impl Pairs {
    /// Add one (baseline, alternative) pair.
    pub fn add(&mut self, baseline: &Cost, alternative: &Cost) {
        self.count += 1;
        let (Some(baseline), Some(alternative)) = (baseline.total_usd, alternative.total_usd)
        else {
            return;
        };
        self.savings.add(baseline - alternative);
        self.baseline.add(baseline);
        self.alternative.add(alternative);
    }

    /// How many pairs were added, priced or not.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The savings over every pair added, extrapolated over `unknown` further items that could
    /// differ but left no evidence, such as unreadable transcript lines.
    pub fn savings(&self, unknown: u64) -> PairedSavings {
        let total = self.count + unknown;
        PairedSavings {
            savings: self.savings.estimate(total),
            baseline: self.baseline.estimate(total),
            alternative: self.alternative.estimate(total),
            priced: self.savings.priced(),
            total,
        }
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
        scenario.cost.assumptions.push("tier sensitivity only: assign this served tier to conditionally priced parts; recorded tiers remain authoritative; these scenarios do not bound actual costs or a plan's usage".into());
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
    if p.kind == PartKind::Advisor && p.tier.is_none() {
        assumptions.push(
            "an advisor call records no service tier; priced at its executor's standard tier, which a Priority Tier commitment on the advisor model alone would change".into(),
        );
    }
    if conditional_tier(r, p) {
        assumptions.push(format!(
            "conditional {tier}-rate OpenAI API equivalent: effective service tier is unrecorded; requested tier/speed is a scenario, not billing evidence; omitted/auto can inherit nonstandard project defaults; current config is not historical evidence; not an invoice"
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
            // As `price_components` prices them: an unsplit count takes the default 30m
            // lifetime, a split missing one lifetime is unpriced, and only the Responses API
            // takes 12h writes.
            let (short, long) = match (u.write30m, u.write12h) {
                (None, None) => (written, 0),
                (Some(short), Some(long)) => (short, long),
                _ => return None,
            };
            match (short, long) {
                (short, 0) => spans.push((short as f64, input * 1.25)),
                (0, long) if r.api == Api::Responses => spans.push((long as f64, input * 2.5)),
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

    /// Compare two scenarios item by item as (baseline, alternative), so a few unpriced requests
    /// cannot hide a clear winner. `unknown` counts further items that could differ but left no
    /// evidence, such as unreadable transcript lines; they are extrapolated over like the rest.
    fn paired_savings<'a>(
        pairs: impl IntoIterator<Item = (&'a Cost, &'a Cost)>,
        unknown: u64,
    ) -> PairedSavings {
        let mut sum = Pairs::default();
        for (baseline, alternative) in pairs {
            sum.add(baseline, alternative);
        }
        sum.savings(unknown)
    }
    use crate::model::{Text, Usage};
    use crate::test_support::{ClaudeUsage, claude_usage, make_request, prices};
    use serde_json::json;

    #[test]
    fn an_advisor_call_takes_a_standard_executors_tier_and_no_other() {
        let catalog = prices();
        let mut r = make_request();
        let mut advisor = r.parts[0].clone();
        advisor.kind = PartKind::Advisor;
        advisor.tier = None;
        advisor.speed = None;
        r.parts.push(advisor.clone());
        let mut recorded = advisor.clone();
        recorded.tier = Some("standard".into());
        let unrecorded = price_part(&catalog, &r, &advisor);
        assert_eq!(
            unrecorded.total_usd,
            price_part(&catalog, &r, &recorded).total_usd
        );
        assert!(unrecorded.total_usd.is_some());
        assert!(
            unrecorded
                .assumptions
                .iter()
                .any(|a| a.starts_with("an advisor call records no service tier"))
        );
        // A priority executor, or executors that disagree, leave the advisor's tier unknown.
        for tiers in [&["priority"][..], &["standard", "priority"]] {
            let mut r = make_request();
            r.parts = tiers
                .iter()
                .map(|t| BillingPart {
                    tier: Some((*t).into()),
                    ..r.parts[0].clone()
                })
                .chain([advisor.clone()])
                .collect();
            let last = r.parts.last().unwrap();
            assert_eq!(price_part(&catalog, &r, last).total_usd, None, "{tiers:?}");
        }
    }

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
    fn tier_scales_part_what_one_tier_scales_alike() {
        let part = |model: &str, input: i64, requested: Option<&str>, tier: Option<&str>| {
            let mut r = codex_request();
            r.requested_speed = requested.map(|t| crate::model::RequestedSpeed {
                tier: Some(t.into()),
                speed: None,
            });
            r.parts[0].model = Some(model.into());
            r.parts[0].tier = tier.map(Into::into);
            r.parts[0].speed = None;
            r.parts[0].geo = None;
            r.parts[0].usage.input = Some(input);
            tier_scale(&r, &r.parts[0])
        };
        let base = part("gpt-6.1-sol", 1000, None, None);
        assert!(base.is_some());
        assert_eq!(part("gpt-6.1-sol", 2000, Some("default"), None), base);
        // A premium tier is 2x on GPT-6.1 and 2.5x on GPT-5.5, so the models scale apart.
        assert_ne!(part("gpt-5.5", 1000, None, None), base);
        assert_ne!(part("gpt-6.1-sol", 1000, Some("fast"), None), base);
        // GPT-5.5 has no published ultrafast rate, so its assumed tier keys it instead.
        assert_ne!(
            part("gpt-5.5", 1000, Some("fast"), None),
            part("gpt-5.5", 1000, None, None)
        );
        assert_eq!(
            part("gpt-5.5", 1000, Some("default"), None),
            part("gpt-5.5", 2000, None, None)
        );
        assert_ne!(
            part("gpt-5.5", 300_000, None, None),
            part("gpt-5.5", 1000, None, None)
        );
        assert_eq!(part("gpt-6.1-sol", 1000, None, Some("standard")), None);

        // A saving at one scale and a loss at another leave the sign open.
        let session: Text = "s".into();
        let mut savings = TierSavings::default();
        savings.add(&session, base.as_ref(), Usd::from_f64(1.1));
        savings.add(
            &session,
            part("gpt-5.5", 1000, None, None).as_ref(),
            -Usd::from_f64(1.0),
        );
        let signs = savings.signs();
        assert!(!signs.never_loses() && !signs.never_saves());
        let mut savings = TierSavings::default();
        savings.add(&session, base.as_ref(), Usd::from_f64(1.1));
        savings.add(&session, base.as_ref(), -Usd::from_f64(1.0));
        assert!(savings.signs().never_loses());
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
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.home;
        let partial = dir.join("models.json.partial.tmp");
        let finished = dir.join("models.json.finished.tmp");
        let mut file = create_temporary(&partial).unwrap();
        file.write_all(b"{\"anthropic\":").unwrap();
        create_temporary(&finished).unwrap();
        drop(Discard(finished.clone()));
        assert!(!finished.exists());
        assert_eq!(downloads().files, vec![partial.clone()]);
        cancel_downloads();
        // Without POSIX delete semantics, Windows hides a deleted file only once its last handle closes.
        drop(file);
        assert!(!partial.exists());
        assert!(create_temporary(&dir.join("models.json.late.tmp")).is_err());
        assert!(!dir.join("models.json.late.tmp").exists());
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
        // Off Anthropic the per-search fee is unknown, never zero.
        let mut elsewhere = tools(&[("web_search_requests", 1)]);
        elsewhere.provider = Some("bedrock".into());
        assert_eq!(server_cost(&elsewhere).total_usd, None);
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
                known_cost(Usd::from_f64(usd))
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
    fn paired_savings_extrapolate_over_unpriced_parts() {
        let known = |usd: f64| known_cost(Usd::from_f64(usd));
        let missing = unknown_cost("missing");
        let (three, two, one) = (known(3.0), known(2.0), known(1.0));
        let a = (&three, &one);
        let b = (&two, &one);
        let gap = (&missing, &missing);
        let exact = |usd| Some(Estimate { usd, margin: None });
        assert_eq!(
            paired_savings([a, b], 0),
            PairedSavings {
                savings: exact(3.0),
                baseline: exact(5.0),
                alternative: exact(2.0),
                priced: 2,
                total: 2,
            }
        );
        // Savings of 2 and 1 average 1.5 over three parts, with a sample variance of 0.5.
        let partial = paired_savings([a, b, gap], 0);
        let savings = partial.savings.unwrap();
        assert_eq!(savings.usd, 4.5);
        let Some(Margin::Usd(margin)) = savings.margin else {
            panic!("two priced parts measure a spread");
        };
        // Two priced parts leave one degree of freedom, so the t value 12.706 stands for Z95.
        assert!((margin - 12.706 * 3.0 * (1.0f64 / 12.0).sqrt()).abs() < 1e-12);
        // Both priced parts cost 1 under the alternative, so its cost has no spread.
        assert_eq!(
            partial.alternative,
            Some(Estimate {
                usd: 3.0,
                margin: Some(Margin::Usd(0.0))
            })
        );
        // An unreadable line counts as one more unpriced part.
        assert_eq!(paired_savings([a, b], 1).savings, partial.savings);
        assert_eq!(
            paired_savings([a, gap], 0).savings,
            Some(Estimate {
                usd: 4.0,
                margin: Some(Margin::Unmeasured)
            })
        );
        assert_eq!(paired_savings([gap], 0).savings, None);
        assert_eq!(paired_savings([], 0).savings, exact(0.0));
    }

    #[test]
    fn weighted_samples_extrapolate_per_unit_of_weight() {
        // Sessions that save $2 per idle period, one of them with three periods.
        let mut sample = Sample::default();
        sample.add_weighted(Usd::from_f64(2.0), 1.0);
        sample.add_weighted(Usd::from_f64(6.0), 3.0);
        // Two more sessions with six periods between them were unpriced: 10 periods in all.
        let estimate = sample.estimate_weighted(4, 10.0).unwrap();
        assert!((estimate.usd - 20.0).abs() < 1e-12);
        // Every priced session saved exactly $2 a period, so the residuals leave no spread.
        assert_eq!(estimate.margin, Some(Margin::Usd(0.0)));
        sample.add_weighted(Usd::from_f64(1.0), 1.0);
        let Some(Margin::Usd(margin)) = sample.estimate_weighted(5, 12.0).unwrap().margin else {
            panic!("three priced sessions measure a spread");
        };
        assert!(margin > 0.0);
    }

    /// Draws from a fixed linear congruential generator, so the check is deterministic.
    struct Draws(u64);

    impl Draws {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn small_samples_widen_their_margin_to_keep_95_percent_coverage() {
        // Five priced requests of a hundred, from a bell-shaped spread of costs.
        let mut draws = Draws(11);
        let population: Vec<f64> = (0..100)
            .map(|_| (0..4).map(|_| draws.next()).sum::<f64>())
            .collect();
        let truth: f64 = population.iter().sum();
        let trials = 4000;
        let mut covered = 0;
        for _ in 0..trials {
            let mut sample = Sample::default();
            let mut picked = std::collections::BTreeSet::new();
            while picked.len() < 5 {
                picked.insert((draws.next() * population.len() as f64) as usize);
            }
            for &i in &picked {
                sample.add(Usd::from_f64(population[i]));
            }
            let estimate = sample.estimate(population.len() as u64).unwrap();
            let Some(Margin::Usd(margin)) = estimate.margin else {
                panic!("a partial sample has a margin");
            };
            covered += usize::from((estimate.usd - truth).abs() <= margin);
        }
        let rate = covered as f64 / trials as f64;
        // The normal value would cover about 88% here.
        assert!((0.93..=0.98).contains(&rate), "covered {rate}");
    }

    #[test]
    fn extrapolation_margins_cover_the_true_total_about_95_percent_of_the_time() {
        // Skewed per-request costs, as cache writes make them: most small, a few large.
        let mut draws = Draws(7);
        let population: Vec<f64> = (0..400)
            .map(|_| {
                let x = draws.next();
                if x < 0.9 { x } else { 40.0 * x }
            })
            .collect();
        let truth: f64 = population.iter().sum();
        let trials = 4000;
        let mut covered = 0;
        for _ in 0..trials {
            // Price a random 95% of the requests, leaving 20 unpriced.
            let mut order: Vec<usize> = (0..population.len()).collect();
            for i in (1..order.len()).rev() {
                let j = (draws.next() * (i + 1) as f64) as usize;
                order.swap(i, j);
            }
            let mut sample = Sample::default();
            for &i in &order[20..] {
                sample.add(Usd::from_f64(population[i]));
            }
            let estimate = sample.estimate(population.len() as u64).unwrap();
            let Some(Margin::Usd(margin)) = estimate.margin else {
                panic!("a partial sample has a margin");
            };
            covered += usize::from((estimate.usd - truth).abs() <= margin);
        }
        let rate = covered as f64 / trials as f64;
        assert!((0.92..=0.975).contains(&rate), "covered {rate}");
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

    // OpenAI fast and ultrafast API repricing preserves long context and billed write lifetimes.
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

    #[test]
    fn codex_prefix_spans_price_writes_only_where_the_whole_request_is_priced() {
        let catalog = prices();
        let part = |details: Value| BillingPart {
            model: Some("gpt-6.1-sol".into()),
            usage: codex(json!({
                "input_tokens": 100000,
                "output_tokens": 100,
                "input_tokens_details": {"cached_tokens": 20000, "cache_write_tokens": 30000, "cache_write_tokens_details": details},
            })),
            ..make_request().parts[0].clone()
        };
        let r = codex_request();
        let (input, read) = (2.0 / 1e6, 0.1 / 1e6);
        let priced = |p: &BillingPart, r: &Request| price_part(&catalog, r, p).total_usd.is_some();
        let spans = |p: &BillingPart, r: &Request| prefix_spans(&catalog, r, p);
        let write = |tokens: f64, rate: f64| {
            Some(vec![
                (20000.0, read),
                (tokens, rate),
                (f64::INFINITY, input),
            ])
        };
        // An unsplit count takes the default 30m lifetime.
        assert_eq!(spans(&part(json!({})), &r), write(30000.0, input * 1.25));
        assert_eq!(
            spans(
                &part(json!({"ttl_30m_tokens": 30000, "ttl_12h_tokens": 0})),
                &r
            ),
            write(30000.0, input * 1.25)
        );
        let long = part(json!({"ttl_30m_tokens": 0, "ttl_12h_tokens": 30000}));
        assert_eq!(spans(&long, &r), write(30000.0, input * 2.5));
        // Two lifetimes leave their order in the prefix unknown.
        assert_eq!(
            spans(
                &part(json!({"ttl_30m_tokens": 10000, "ttl_12h_tokens": 20000})),
                &r
            ),
            None
        );
        // A split that names one lifetime leaves the other's tokens unpriced, not ordinary.
        let partial = part(json!({"ttl_30m_tokens": 10000}));
        assert!(!priced(&partial, &r));
        assert_eq!(spans(&partial, &r), None);
        // Only the Responses API takes 12h writes.
        let mut messages = codex_request();
        messages.api = Api::Messages;
        assert!(priced(&long, &r) && !priced(&long, &messages));
        assert_eq!(spans(&long, &messages), None);
    }

    #[test]
    fn premium_tiers_on_gpt_5_5_and_earlier_are_priced_only_within_the_short_context() {
        let fixture = crate::test_support::fixture_path("prices.json");
        let mut catalog: Value =
            serde_json::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
        catalog["openai"]["models"]["gpt-5.5"] = json!({
            "family": "gpt",
            "release_date": "2026-04-23",
            "tool_call": true,
            "modalities": {"output": ["text"]},
            "cost": {
                "input": 5, "output": 30, "cache_read": 0.5,
                "tiers": [{"tier": {"type": "context", "size": 272000}, "input": 10, "output": 45, "cache_read": 1}],
                "context_over_200k": {"input": 10, "output": 45, "cache_read": 1},
            },
        });
        let Ok(catalog) = parse_catalog(catalog.to_string().as_bytes()) else {
            panic!("the catalog parses");
        };
        let r = codex_request();
        for (model, multiplier) in [("gpt-5.4", 2.0), ("gpt-5.5", 2.5)] {
            let at = |input: i64, tier: &str| {
                let p = BillingPart {
                    model: Some(model.into()),
                    tier: Some(tier.into()),
                    usage: codex(json!({
                        "input_tokens": input,
                        "output_tokens": 100,
                        "cached_input_tokens": 1000,
                    })),
                    ..r.parts[0].clone()
                };
                price_part(&catalog, &r, &p).total_usd.map(Usd::to_f64)
            };
            for tier in ["fast", "priority"] {
                let standard = at(272000, "default").unwrap();
                let premium = at(272000, tier).unwrap();
                assert!(
                    (premium - standard * multiplier).abs() < 1e-12,
                    "{model} {tier}: {premium} vs {standard}"
                );
                // Above the threshold no premium rate is documented, so the price is unknown.
                assert!(at(272001, "default").is_some(), "{model}");
                assert_eq!(at(272001, tier), None, "{model} {tier}");
            }
            assert_eq!(at(1000, "ultrafast"), None, "{model}");
        }
    }

    // Plain GPT-5.6 alias has verified fast rates without substituting catalog model prices.
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
