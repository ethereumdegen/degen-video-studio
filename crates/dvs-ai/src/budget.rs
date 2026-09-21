//! The spend ceiling.
//!
//! Every other safety property in this crate is about correctness; this one is about money.
//! An agent that discovers `ai.generate-video` in the op catalog and calls it in a retry
//! loop can spend a provider's entire credit balance in minutes, and the operator finds out
//! afterwards. So no request is sent before its estimated cost is checked against a
//! per-project ceiling, and the refusal carries exit code 6 (`budget`) so a shell loop can
//! branch on it without reading prose.
//!
//! Two decisions are deliberate:
//!
//! - **A project with no budget file is not unlimited.** The default ceiling is
//!   [`DEFAULT_CEILING_USD`]; an unconfigured project is bounded by construction, and
//!   raising the bound is an explicit act ([`CEILING_ENV`] or `.dvs-budget.json`).
//! - **Spend lives beside the project, not inside it.** `.dvs-budget.json` is not part of
//!   the document, because undoing an edit must not undo the fact that money was spent.

use chrono::{DateTime, Utc};
use dvs_core::error::{Error, Result};
use dvs_core::paths::ProjectPaths;
use dvs_core::vfs::Vfs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Ledger file, in the project root.
pub const BUDGET_FILE: &str = ".dvs-budget.json";

/// Overrides the ceiling for one invocation.
pub const CEILING_ENV: &str = "DVS_BUDGET_USD";

/// Ceiling applied when nothing is configured. Low enough that an unattended loop cannot
/// do real damage, high enough that a few legitimate generations work out of the box.
pub const DEFAULT_CEILING_USD: f64 = 5.0;

/// How many charges to keep in the file. The ledger is for answering "where did the money
/// go", which the recent history answers; an unbounded list would grow into the document
/// directory forever.
const MAX_CHARGES: usize = 200;

/// What kind of generation is being priced. Providers bill per second of video, per image,
/// and per character of speech, so the unit is part of the estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Image,
    Speech,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Image => "image",
            Kind::Speech => "speech",
        }
    }

    /// The unit `units` is counted in, for the estimate's explanation.
    pub fn unit(self) -> &'static str {
        match self {
            Kind::Video => "second",
            Kind::Image => "image",
            Kind::Speech => "1k characters",
        }
    }

    /// Conservative default price per unit, in USD.
    ///
    /// These are pre-flight estimates, not invoices: the fal catalog changes weekly and its
    /// prices with it, so the numbers are chosen to be at or above the going rate (an
    /// estimate that is too low would let a loop past the ceiling) and are overridable per
    /// model through the `prices` map in `.dvs-budget.json`.
    pub fn default_price(self) -> f64 {
        match self {
            Kind::Video => 0.10,
            Kind::Image => 0.05,
            Kind::Speech => 0.02,
        }
    }
}

/// One charge against the budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Charge {
    pub provider: String,
    pub model: String,
    pub cost_usd: f64,
    pub at: DateTime<Utc>,
}

/// `.dvs-budget.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    pub ceiling_usd: f64,
    #[serde(default)]
    pub spent_usd: f64,
    /// Price per unit by model id, or by kind name (`video`, `image`, `speech`) as a
    /// fallback. Lets an operator encode what their provider actually charges without
    /// waiting for this crate to be updated.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<Charge>,
}

impl Default for Ledger {
    fn default() -> Ledger {
        Ledger {
            ceiling_usd: DEFAULT_CEILING_USD,
            spent_usd: 0.0,
            prices: BTreeMap::new(),
            requests: Vec::new(),
        }
    }
}

impl Ledger {
    pub fn remaining(&self) -> f64 {
        (self.ceiling_usd - self.spent_usd).max(0.0)
    }

    /// Price per unit for a request, most specific first.
    pub fn price(&self, kind: Kind, model: &str) -> f64 {
        self.prices
            .get(model)
            .or_else(|| self.prices.get(kind.as_str()))
            .copied()
            .unwrap_or_else(|| kind.default_price())
    }

    /// What a request is expected to cost. `units` is seconds for video, images for image,
    /// thousands of characters for speech.
    pub fn estimate(&self, kind: Kind, model: &str, units: f64) -> f64 {
        let units = if units.is_finite() { units.max(0.0) } else { 0.0 };
        self.price(kind, model) * units
    }

    /// The `--json` shape behind `ai.budget`.
    pub fn report(&self, source: &Path) -> serde_json::Value {
        serde_json::json!({
            "ceilingUsd": self.ceiling_usd,
            "spentUsd": self.spent_usd,
            "remainingUsd": self.remaining(),
            "requests": self.requests.len(),
            "recent": self.requests.iter().rev().take(10).collect::<Vec<_>>(),
            "ledger": source.display().to_string(),
            "ceilingEnv": CEILING_ENV,
        })
    }
}

/// The per-project ledger on disk.
pub struct Budget {
    path: PathBuf,
    vfs: Arc<dyn Vfs>,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Budget {
    pub fn new(paths: &ProjectPaths, vfs: Arc<dyn Vfs>) -> Budget {
        Budget {
            path: paths.root().join(BUDGET_FILE),
            vfs,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the ledger, applying the environment override to the ceiling.
    ///
    /// The override affects only the ceiling, never the recorded spend: a caller may raise
    /// their own limit for one command, but cannot un-spend money by changing a variable.
    pub fn load(&self) -> Result<Ledger> {
        let mut ledger = if self.vfs.exists(&self.path) {
            let bytes = self.vfs.read(&self.path)?;
            serde_json::from_slice(&bytes).map_err(|e| Error::json(&self.path, e))?
        } else {
            Ledger::default()
        };
        if let Some(override_ceiling) = env_ceiling()? {
            ledger.ceiling_usd = override_ceiling;
        }
        if !ledger.ceiling_usd.is_finite() || ledger.ceiling_usd < 0.0 {
            return Err(Error::op(format!(
                "{} has an invalid ceilingUsd ({}); it must be a non-negative number",
                self.path.display(),
                ledger.ceiling_usd
            )));
        }
        if !ledger.spent_usd.is_finite() || ledger.spent_usd < 0.0 {
            return Err(Error::op(format!(
                "{} has an invalid spentUsd ({}); it must be a non-negative number",
                self.path.display(),
                ledger.spent_usd
            )));
        }
        Ok(ledger)
    }

    /// Refuse a request that would cross the ceiling. Returns the ledger it checked
    /// against, so a caller can report the numbers it was judged by.
    pub fn check(&self, estimate: f64) -> Result<Ledger> {
        let ledger = self.load()?;
        if ledger.spent_usd + estimate > ledger.ceiling_usd {
            return Err(Error::budget(format!(
                "estimated {} for this request exceeds the project AI budget: ceiling {}, \
                 already spent {}, remaining {} — raise it with {CEILING_ENV}=<usd> or by \
                 setting \"ceilingUsd\" in {}",
                usd(estimate),
                usd(ledger.ceiling_usd),
                usd(ledger.spent_usd),
                usd(ledger.remaining()),
                self.path.display()
            )));
        }
        Ok(ledger)
    }

    /// Add a charge. Called after a provider actually did work, so a failed request costs
    /// nothing and a cached one is never recorded.
    pub fn record(&self, provider: &str, model: &str, cost_usd: f64) -> Result<Ledger> {
        let mut ledger = self.load()?;
        let cost = if cost_usd.is_finite() {
            cost_usd.max(0.0)
        } else {
            0.0
        };
        ledger.spent_usd += cost;
        ledger.requests.push(Charge {
            provider: provider.to_string(),
            model: model.to_string(),
            cost_usd: cost,
            at: Utc::now(),
        });
        if ledger.requests.len() > MAX_CHARGES {
            let excess = ledger.requests.len() - MAX_CHARGES;
            ledger.requests.drain(..excess);
        }
        self.save(&ledger)?;
        Ok(ledger)
    }

    /// Persist the ledger. The ceiling written back is the file's own, not an environment
    /// override, so `DVS_BUDGET_USD=20` for one command does not silently become permanent.
    fn save(&self, ledger: &Ledger) -> Result<()> {
        let mut stored = ledger.clone();
        if env_ceiling()?.is_some() {
            stored.ceiling_usd = self.stored_ceiling()?;
        }
        let mut bytes = serde_json::to_vec_pretty(&stored)
            .map_err(|e| Error::op(format!("budget ledger is not serializable: {e}")))?;
        bytes.push(b'\n');
        self.vfs.write(&self.path, &bytes)
    }

    /// The ceiling as the file has it, ignoring the environment.
    fn stored_ceiling(&self) -> Result<f64> {
        if !self.vfs.exists(&self.path) {
            return Ok(DEFAULT_CEILING_USD);
        }
        let bytes = self.vfs.read(&self.path)?;
        let ledger: Ledger =
            serde_json::from_slice(&bytes).map_err(|e| Error::json(&self.path, e))?;
        Ok(ledger.ceiling_usd)
    }
}

fn env_ceiling() -> Result<Option<f64>> {
    let Ok(raw) = std::env::var(CEILING_ENV) else {
        return Ok(None);
    };
    let trimmed = raw.trim().trim_start_matches('$');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: f64 = trimmed.parse().map_err(|_| {
        Error::bad_args(format!(
            "{CEILING_ENV} is not a number: '{raw}' — set it to a dollar amount like 2.50"
        ))
    })?;
    if !value.is_finite() || value < 0.0 {
        return Err(Error::bad_args(format!(
            "{CEILING_ENV} must be a non-negative number, got '{raw}'"
        )));
    }
    Ok(Some(value))
}

/// Dollars for humans. Sub-cent amounts are real here — a sentence of TTS costs a fraction
/// of a cent — and printing them as `$0.00` would make an estimate look free.
pub fn usd(value: f64) -> String {
    if value != 0.0 && value.abs() < 0.01 {
        format!("${value:.4}")
    } else {
        format!("${value:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::vfs::MemVfs;

    use crate::testkit;

    fn fixture() -> (Arc<dyn Vfs>, Budget) {
        let vfs = MemVfs::shared();
        let budget = Budget::new(&ProjectPaths::new("/p"), vfs.clone());
        (vfs, budget)
    }

    fn write_ledger(vfs: &Arc<dyn Vfs>, json: serde_json::Value) {
        vfs.write(
            Path::new("/p").join(BUDGET_FILE).as_path(),
            json.to_string().as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn an_unconfigured_project_is_bounded_not_unlimited() {
        let _env = testkit::isolate();
        let (_vfs, budget) = fixture();

        let ledger = budget.load().unwrap();

        assert_eq!(ledger.ceiling_usd, DEFAULT_CEILING_USD);
        assert!(budget.check(DEFAULT_CEILING_USD + 0.01).is_err());
    }

    #[test]
    fn a_request_over_the_remaining_ceiling_is_refused_with_the_numbers() {
        let _env = testkit::isolate();
        let (vfs, budget) = fixture();
        write_ledger(
            &vfs,
            serde_json::json!({ "ceilingUsd": 1.0, "spentUsd": 0.8 }),
        );

        let error = budget.check(0.4).expect_err("0.8 + 0.4 > 1.0");

        assert_eq!(error.exit_code(), dvs_core::exit::BUDGET);
        let message = error.to_string();
        for expected in ["$0.40", "$1.00", "$0.80", "$0.20"] {
            assert!(message.contains(expected), "{expected} missing from: {message}");
        }
        assert!(budget.check(0.2).is_ok(), "exactly the remainder is allowed");
    }

    #[test]
    fn spend_accumulates_exactly_across_charges() {
        let _env = testkit::isolate();
        let (vfs, budget) = fixture();
        write_ledger(&vfs, serde_json::json!({ "ceilingUsd": 10.0 }));

        budget.record("fal", "model-a", 0.25).unwrap();
        let after = budget.record("fal", "model-b", 0.5).unwrap();

        assert!((after.spent_usd - 0.75).abs() < 1e-9, "{}", after.spent_usd);
        assert_eq!(after.requests.len(), 2);
        let reloaded = budget.load().unwrap();
        assert!((reloaded.spent_usd - 0.75).abs() < 1e-9);
        assert!((reloaded.remaining() - 9.25).abs() < 1e-9);
        assert_eq!(reloaded.requests[0].model, "model-a");
    }

    #[test]
    fn the_environment_raises_the_ceiling_without_rewriting_the_file() {
        let _env = testkit::isolate();
        let (vfs, budget) = fixture();
        write_ledger(
            &vfs,
            serde_json::json!({ "ceilingUsd": 1.0, "spentUsd": 0.9 }),
        );
        std::env::set_var(CEILING_ENV, "20");

        assert_eq!(budget.load().unwrap().ceiling_usd, 20.0);
        budget.check(5.0).expect("the override applies");
        budget.record("fal", "m", 5.0).unwrap();
        std::env::remove_var(CEILING_ENV);

        let persisted = budget.load().unwrap();
        assert_eq!(
            persisted.ceiling_usd, 1.0,
            "a one-command override must not become the project's ceiling"
        );
        assert!((persisted.spent_usd - 5.9).abs() < 1e-9, "spend is permanent");
    }

    #[test]
    fn a_price_override_beats_the_built_in_default() {
        let ledger = Ledger {
            prices: BTreeMap::from([
                ("fal-ai/cheap".to_string(), 0.01),
                ("video".to_string(), 0.5),
            ]),
            ..Ledger::default()
        };

        assert!((ledger.estimate(Kind::Video, "fal-ai/cheap", 5.0) - 0.05).abs() < 1e-9);
        assert!((ledger.estimate(Kind::Video, "fal-ai/other", 5.0) - 2.5).abs() < 1e-9);
        assert!(
            (ledger.estimate(Kind::Image, "fal-ai/other", 1.0) - Kind::Image.default_price()).abs()
                < 1e-9
        );
    }

    #[test]
    fn a_nonsense_environment_ceiling_is_a_bad_argument_not_a_default() {
        let _env = testkit::isolate();
        std::env::set_var(CEILING_ENV, "lots");
        let (_vfs, budget) = fixture();

        let error = budget.load().expect_err("unparseable ceiling must fail");
        std::env::remove_var(CEILING_ENV);

        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
        assert!(error.to_string().contains(CEILING_ENV));
    }

    #[test]
    fn sub_cent_estimates_are_not_printed_as_free() {
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd(0.0004), "$0.0004");
        assert_eq!(usd(1.5), "$1.50");
    }
}
