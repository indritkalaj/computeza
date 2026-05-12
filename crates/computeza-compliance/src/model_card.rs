//! Model card -- structured AI-system metadata maps to Annex IV
//! technical documentation requirements.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

use crate::eu_ai_act::{ArticleEvidence, RiskClassification, RiskJustification};

/// Errors raised by the model-card registry.
#[derive(Debug, thiserror::Error)]
pub enum ModelCardError {
    /// Filesystem / I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialisation / parsing failure on read or write.
    #[error("malformed model card: {0}")]
    Malformed(String),
    /// Registration of a Prohibited-class card was refused. The
    /// deployer must change the classification or remove the
    /// system before persistence.
    #[error("refusing to register Prohibited classification (Article 5 / Title II)")]
    ProhibitedClassification,
    /// A card with the given id already exists; mutations go
    /// through [`ModelCardRegistry::update`].
    #[error("model card already exists: {0}")]
    AlreadyExists(String),
    /// Lookup by id returned nothing.
    #[error("model card not found: {0}")]
    NotFound(String),
}

/// Where the deployer says the model is actively used. Free-form;
/// the deployer captures their own internal labels (e.g. team
/// names, surface ids) so the registry stays useful across
/// re-organisations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelDeployment {
    /// Surface / endpoint where the model serves predictions.
    pub surface: String,
    /// Owning team / business unit.
    pub team: String,
    /// First date the model went live in this surface.
    pub effective_from: chrono::DateTime<chrono::Utc>,
}

/// Evaluation result keyed by metric. Computeza does not enforce a
/// metric vocabulary -- the deployer records whatever their domain
/// uses (accuracy, F1, MAE, ROC-AUC, fairness deltas across
/// protected groups, etc.). Article 15's "accuracy + robustness"
/// requirement is satisfied by *consistent* + *recorded* metrics,
/// not a specific metric set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssessmentEntry {
    /// Metric name. Free-form; the deployer's own taxonomy.
    pub metric: String,
    /// Recorded value as a string -- preserves whatever
    /// formatting / precision the deployer used.
    pub value: String,
    /// When this assessment was run.
    pub measured_at: chrono::DateTime<chrono::Utc>,
    /// Optional reference to the evaluation run id (MLflow run,
    /// W&B sweep, internal CI job, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_ref: Option<String>,
}

/// Annex-IV-shaped model card persisted in the deployer's
/// `model-cards.jsonl`. One card per deployed AI system. The Act's
/// technical documentation requirement (Article 11) is satisfied
/// by maintaining + retaining these structured records.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelCard {
    /// Stable identifier. The deployer's choice; UUIDs are typical
    /// but human-readable slugs work too ("credit-risk-v3").
    pub id: String,
    /// Display name.
    pub name: String,
    /// Risk classification per [`RiskClassification`].
    pub risk: RiskClassification,
    /// Operator's justification for the classification -- audit-
    /// grade narrative + citations.
    pub risk_justification: RiskJustification,
    /// Intended use, target population, geographies. Drives
    /// Article 13 transparency-to-deployer obligations.
    pub intended_use: String,
    /// Training data summary -- Article 10 requires the deployer
    /// document data provenance + representativeness. Free-form
    /// text, NOT a snapshot of the training data itself.
    pub training_data_summary: String,
    /// Known limitations + out-of-distribution behaviour. Article
    /// 13 requires deployers tell their users what the system
    /// CANNOT reliably do.
    pub limitations: String,
    /// Article 14 human-oversight design: how does a human
    /// override / halt / contest a model decision?
    pub human_oversight_design: String,
    /// One assessment row per evaluation run. Append-only over the
    /// model's life -- no deletion, no overwrite.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluation_metrics: Vec<AssessmentEntry>,
    /// Active deployments (surface, team, effective_from). Empty
    /// when the model is registered but not yet rolled out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<ModelDeployment>,
    /// Article-evidence pairs the deployer has collected. For
    /// High-Risk systems the deployer-side conformity assessment
    /// requires evidence per Article 9-15 + Article 43.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub article_evidence: Vec<ArticleEvidence>,
    /// First registration timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last update timestamp.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl ModelCard {
    /// `true` when every Article-{9..15} obligation has at least
    /// one [`ArticleEvidence`] row attached. Used by the UI to
    /// surface "conformity-assessment complete" state on the
    /// registry index.
    #[must_use]
    pub fn high_risk_evidence_complete(&self) -> bool {
        if !matches!(self.risk, RiskClassification::HighRisk) {
            return true; // not applicable
        }
        for required in &["9", "10", "11", "12", "13", "14", "15"] {
            if !self.article_evidence.iter().any(|e| e.article == *required) {
                return false;
            }
        }
        true
    }
}

/// JSONL-backed model-card registry. One card per line; reads scan
/// the whole file into memory on open and cache. Mutations append
/// (for create) or rewrite (for update / delete) the file
/// atomically.
///
/// The file lives next to the operator state DB at
/// `<state_db_parent>/model-cards.jsonl`.
#[derive(Clone)]
pub struct ModelCardRegistry {
    path: Arc<PathBuf>,
    cache: Arc<RwLock<Vec<ModelCard>>>,
}

impl ModelCardRegistry {
    /// Open the registry at `path`, reading any existing cards.
    /// Creates a fresh empty registry if the file does not exist
    /// (parent directory created with `create_dir_all`).
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, ModelCardError> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let cards = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let raw = tokio::fs::read(&path).await?;
            let mut out = Vec::new();
            for line in raw.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let card: ModelCard = serde_json::from_slice(line)
                    .map_err(|e| ModelCardError::Malformed(e.to_string()))?;
                out.push(card);
            }
            out
        } else {
            Vec::new()
        };
        tracing::debug!(
            path = %path.display(),
            count = cards.len(),
            "opened model card registry"
        );
        Ok(Self {
            path: Arc::new(path),
            cache: Arc::new(RwLock::new(cards)),
        })
    }

    /// `true` when zero cards are registered.
    pub async fn is_empty(&self) -> bool {
        self.cache.read().await.is_empty()
    }

    /// All registered cards in registration order.
    pub async fn list(&self) -> Vec<ModelCard> {
        self.cache.read().await.clone()
    }

    /// Look up one card by id.
    pub async fn get(&self, id: &str) -> Option<ModelCard> {
        self.cache.read().await.iter().find(|c| c.id == id).cloned()
    }

    /// Register a fresh card. Rejects Prohibited classifications +
    /// duplicate ids. Persists by appending one JSON line.
    pub async fn create(&self, card: ModelCard) -> Result<(), ModelCardError> {
        if matches!(card.risk, RiskClassification::Prohibited) {
            return Err(ModelCardError::ProhibitedClassification);
        }
        {
            let cache = self.cache.read().await;
            if cache.iter().any(|c| c.id == card.id) {
                return Err(ModelCardError::AlreadyExists(card.id.clone()));
            }
        }
        let line =
            serde_json::to_vec(&card).map_err(|e| ModelCardError::Malformed(e.to_string()))?;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.as_ref())
            .await?;
        f.write_all(&line).await?;
        f.write_all(b"\n").await?;
        f.flush().await?;
        f.sync_all().await?;
        self.cache.write().await.push(card);
        Ok(())
    }

    /// Replace a card by id. Rewrites the whole file atomically
    /// (write to `<path>.tmp`, rename over the original) so an
    /// interrupted update never leaves the registry truncated.
    pub async fn update(&self, card: ModelCard) -> Result<(), ModelCardError> {
        if matches!(card.risk, RiskClassification::Prohibited) {
            return Err(ModelCardError::ProhibitedClassification);
        }
        let mut cache = self.cache.write().await;
        let idx = cache
            .iter()
            .position(|c| c.id == card.id)
            .ok_or_else(|| ModelCardError::NotFound(card.id.clone()))?;
        cache[idx] = card;
        rewrite_atomic(&self.path, &cache).await?;
        Ok(())
    }

    /// Delete a card by id. Idempotent: succeeds when the id is
    /// already absent.
    pub async fn delete(&self, id: &str) -> Result<(), ModelCardError> {
        let mut cache = self.cache.write().await;
        let before = cache.len();
        cache.retain(|c| c.id != id);
        if cache.len() != before {
            rewrite_atomic(&self.path, &cache).await?;
        }
        Ok(())
    }
}

async fn rewrite_atomic(path: &Path, cards: &[ModelCard]) -> Result<(), ModelCardError> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut f = tokio::fs::File::create(&tmp).await?;
    for card in cards {
        let line =
            serde_json::to_vec(card).map_err(|e| ModelCardError::Malformed(e.to_string()))?;
        f.write_all(&line).await?;
        f.write_all(b"\n").await?;
    }
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eu_ai_act::RiskClassification;

    fn sample_card() -> ModelCard {
        let now = chrono::Utc::now();
        ModelCard {
            id: "credit-risk-v3".into(),
            name: "Credit risk scoring v3".into(),
            risk: RiskClassification::HighRisk,
            risk_justification: RiskJustification {
                rationale: "Annex III item 5(b) -- creditworthiness assessment of natural persons.".into(),
                citations: vec!["intended_use".into()],
            },
            intended_use: "Assist underwriters in initial scoring of personal loan applications.".into(),
            training_data_summary: "Anonymised internal applications 2020-2024, plus public bureau data.".into(),
            limitations: "Not for use in jurisdictions where Annex III item 5(b) is prohibited at the national level.".into(),
            human_oversight_design: "Every model output reviewed by a human underwriter; override produces audit-log entry.".into(),
            evaluation_metrics: Vec::new(),
            deployments: Vec::new(),
            article_evidence: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn registry_round_trips_create_list_get_via_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-cards.jsonl");
        let reg = ModelCardRegistry::open(&path).await.unwrap();
        assert!(reg.is_empty().await);
        let card = sample_card();
        reg.create(card.clone()).await.unwrap();
        let listed = reg.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, card.id);
        let got = reg.get(&card.id).await.unwrap();
        assert_eq!(got.id, card.id);

        // Re-open from disk reads the same card back.
        let reg2 = ModelCardRegistry::open(&path).await.unwrap();
        let listed2 = reg2.list().await;
        assert_eq!(listed2.len(), 1);
        assert_eq!(listed2[0].id, card.id);
    }

    #[tokio::test]
    async fn registry_rejects_prohibited_classification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-cards.jsonl");
        let reg = ModelCardRegistry::open(&path).await.unwrap();
        let mut card = sample_card();
        card.risk = RiskClassification::Prohibited;
        let err = reg.create(card).await.expect_err("must reject");
        assert!(matches!(err, ModelCardError::ProhibitedClassification));
    }

    #[tokio::test]
    async fn registry_rejects_duplicate_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-cards.jsonl");
        let reg = ModelCardRegistry::open(&path).await.unwrap();
        let card = sample_card();
        reg.create(card.clone()).await.unwrap();
        let err = reg.create(card).await.expect_err("must reject duplicate");
        assert!(matches!(err, ModelCardError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn registry_update_and_delete_rewrite_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-cards.jsonl");
        let reg = ModelCardRegistry::open(&path).await.unwrap();
        let card = sample_card();
        reg.create(card.clone()).await.unwrap();

        let mut updated = card.clone();
        updated.name = "Credit risk v3 (updated)".into();
        reg.update(updated.clone()).await.unwrap();
        let got = reg.get(&card.id).await.unwrap();
        assert_eq!(got.name, "Credit risk v3 (updated)");

        reg.delete(&card.id).await.unwrap();
        assert!(reg.get(&card.id).await.is_none());
        // Idempotent delete.
        reg.delete(&card.id).await.unwrap();
    }

    #[test]
    fn high_risk_evidence_complete_requires_articles_9_through_15() {
        let mut card = sample_card();
        assert!(!card.high_risk_evidence_complete());
        for art in &["9", "10", "11", "12", "13", "14", "15"] {
            card.article_evidence.push(ArticleEvidence {
                article: (*art).to_string(),
                artefact: format!("artefact-for-art-{art}"),
                recorded_at: chrono::Utc::now(),
                note: None,
            });
        }
        assert!(card.high_risk_evidence_complete());
    }

    #[test]
    fn high_risk_evidence_complete_true_for_non_high_risk() {
        let mut card = sample_card();
        card.risk = RiskClassification::Minimal;
        // Minimal-risk cards don't need the checklist.
        assert!(card.high_risk_evidence_complete());
    }
}
