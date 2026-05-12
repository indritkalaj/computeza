//! EU AI Act risk taxonomy + article checklist.

use serde::{Deserialize, Serialize};

/// Four-bucket risk taxonomy from Title II of the EU AI Act.
///
/// The deployer classifies each AI system in their workspace into
/// exactly one bucket; obligations cascade from the bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskClassification {
    /// Title II, Article 5. Practices banned outright: social
    /// scoring by public authorities, real-time biometric ID in
    /// public spaces (except narrow exceptions), exploitation of
    /// vulnerabilities, predictive policing on personality alone,
    /// emotion recognition in workplace / schools, untargeted
    /// scraping for facial-image databases. Computeza rejects
    /// activation of any model card classified as Prohibited --
    /// the deployer must remove the system before continuing.
    Prohibited,
    /// Title III, Annex III. Systems in safety-critical domains:
    /// biometric ID, critical infrastructure, education, employment
    /// and worker management, essential services (credit scoring,
    /// public benefits), law enforcement, migration, justice and
    /// democratic processes. Carries the bulk of the Act's
    /// obligations: risk management (Art 9), data governance
    /// (Art 10), technical documentation (Art 11), automatic
    /// logging (Art 12), transparency to users (Art 13), human
    /// oversight (Art 14), accuracy/robustness/cybersecurity
    /// (Art 15), conformity assessment (Art 43), post-market
    /// monitoring (Art 72).
    HighRisk,
    /// Title IV, Article 50. Systems with transparency obligations
    /// only: chatbots ("you are talking to an AI"), emotion
    /// recognition, biometric categorisation, deepfakes (must be
    /// labelled). Computeza's transparency primitive
    /// ([`crate::transparency_banner`]) targets this bucket.
    LimitedRisk,
    /// Everything else. No specific obligations under the Act;
    /// general principles + voluntary codes of conduct only.
    /// Spam filters, recommendation engines below transparency
    /// thresholds, etc.
    Minimal,
}

impl RiskClassification {
    /// Display label, human-readable.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            RiskClassification::Prohibited => "Prohibited",
            RiskClassification::HighRisk => "High-risk",
            RiskClassification::LimitedRisk => "Limited risk",
            RiskClassification::Minimal => "Minimal risk",
        }
    }

    /// `true` when the deployer may register + activate this
    /// classification. Prohibited is the only `false`.
    #[must_use]
    pub fn is_deployable(&self) -> bool {
        !matches!(self, RiskClassification::Prohibited)
    }

    /// Slug for use in URLs / log fields. Stable across releases.
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            RiskClassification::Prohibited => "prohibited",
            RiskClassification::HighRisk => "high-risk",
            RiskClassification::LimitedRisk => "limited-risk",
            RiskClassification::Minimal => "minimal",
        }
    }
}

/// Operator's narrative justification for the classification.
/// Stored alongside the model card so an audit can verify the
/// reasoning -- the Act demands "the deployer shall classify"
/// (Annex III recital), and `assertion` is the audit-grade record
/// of that classification decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskJustification {
    /// Free-form text from the deployer explaining the rationale.
    pub rationale: String,
    /// References to the model card's section 2 (Intended use) and
    /// section 5 (Limitations) that support the classification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<String>,
}

/// One Article-of-the-Act + the evidence the deployer must collect
/// for high-risk systems. Used to render a checklist on
/// `/compliance/eu-ai-act` and to validate that a model card
/// claiming High-Risk has filled in every required artefact.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EuAiActArticle {
    /// Article number (e.g. "9", "10", "13").
    pub article: &'static str,
    /// One-line title.
    pub title: &'static str,
    /// What the deployer must do / produce / retain.
    pub obligation: &'static str,
    /// Where the evidence comes from inside Computeza's stack
    /// (audit log, model card, transparency primitive, ...).
    pub evidence_source: &'static str,
}

/// The canonical Article 9-72 checklist used to gate High-Risk
/// model-card registration. Static; baked into the binary.
#[must_use]
pub fn eu_ai_act_articles() -> &'static [EuAiActArticle] {
    &[
        EuAiActArticle {
            article: "9",
            title: "Risk management system",
            obligation:
                "Identify + analyse + evaluate + mitigate foreseeable risks across the lifecycle, including residual-risk acceptance.",
            evidence_source:
                "ModelCard.intended_use + ModelCard.limitations + RiskJustification.rationale",
        },
        EuAiActArticle {
            article: "10",
            title: "Data and data governance",
            obligation:
                "Training / validation / test sets meet quality criteria: relevance, representativeness, freedom from errors, free from prohibited biases.",
            evidence_source:
                "ModelCard.training_data_summary + Lakekeeper lineage (data plane)",
        },
        EuAiActArticle {
            article: "11",
            title: "Technical documentation",
            obligation:
                "Maintain technical documentation per Annex IV (system architecture, training methodology, evaluation metrics, deployment env).",
            evidence_source: "ModelCard (Computeza's structured representation of Annex IV)",
        },
        EuAiActArticle {
            article: "12",
            title: "Record-keeping (automatic logging)",
            obligation:
                "Automatically log events that identify operational risks; retain logs for the lifetime of the system.",
            evidence_source:
                "computeza-audit append-only signed log (Ed25519 chain over JSONL)",
        },
        EuAiActArticle {
            article: "13",
            title: "Transparency + information to deployers",
            obligation:
                "Provide deployers with instructions for use, including system capabilities, limitations, and required human oversight.",
            evidence_source: "ModelCard.intended_use + ModelCard.human_oversight_design",
        },
        EuAiActArticle {
            article: "14",
            title: "Human oversight",
            obligation:
                "Design + implement so natural persons can effectively oversee operation during use, including override + halt.",
            evidence_source: "ModelCard.human_oversight_design",
        },
        EuAiActArticle {
            article: "15",
            title: "Accuracy, robustness, cybersecurity",
            obligation:
                "Achieve appropriate accuracy + robustness + resilience to errors / attacks throughout the lifecycle.",
            evidence_source:
                "ModelCard.evaluation_metrics + Computeza's signed audit log + TLS hybrid KEX (PQ readiness)",
        },
        EuAiActArticle {
            article: "43",
            title: "Conformity assessment",
            obligation:
                "Carry out + maintain a conformity assessment procedure before placing on market / putting into service.",
            evidence_source: "Deferred to v0.1 -- /compliance/eu-ai-act/assessment workflow",
        },
        EuAiActArticle {
            article: "50",
            title: "Transparency to natural persons",
            obligation:
                "Inform users they are interacting with AI; label deepfakes; flag emotion recognition + biometric categorisation.",
            evidence_source: "transparency_banner() injected into the AI surface",
        },
        EuAiActArticle {
            article: "72",
            title: "Post-market monitoring",
            obligation:
                "Collect + analyse data on the system's performance throughout its lifetime to detect emerging risks.",
            evidence_source: "Audit log + evaluation_metrics updates over time + Grafana dashboards",
        },
        EuAiActArticle {
            article: "86",
            title: "Right to explanation",
            obligation:
                "Affected persons may request meaningful explanations of decisions made by high-risk AI systems.",
            evidence_source:
                "Deferred to v0.1 -- per-decision SHAP / counterfactual capture in audit log",
        },
    ]
}

/// A single Article-Evidence pair as recorded against a model card.
/// The deployer fills these in to demonstrate compliance per-
/// article; the registry persists them so an auditor can review.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArticleEvidence {
    /// Article number this evidence attaches to.
    pub article: String,
    /// What the deployer produced / where it lives (e.g. "Audit
    /// log entries 2026-08-12T00:00..2026-09-01T00:00", "MLflow
    /// run id abc123").
    pub artefact: String,
    /// When the evidence was collected / linked.
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_classification_prohibited_blocks_deployment() {
        assert!(!RiskClassification::Prohibited.is_deployable());
        assert!(RiskClassification::HighRisk.is_deployable());
        assert!(RiskClassification::LimitedRisk.is_deployable());
        assert!(RiskClassification::Minimal.is_deployable());
    }

    #[test]
    fn risk_classification_serializes_kebab_case() {
        let json = serde_json::to_string(&RiskClassification::HighRisk).unwrap();
        assert_eq!(json, "\"high-risk\"");
    }

    #[test]
    fn eu_ai_act_articles_covers_the_core_obligations() {
        let articles = eu_ai_act_articles();
        // Must include the four cornerstone articles.
        for needed in &["9", "10", "11", "12", "13", "14", "15", "50"] {
            assert!(
                articles.iter().any(|a| a.article == *needed),
                "missing Article {needed}"
            );
        }
    }

    #[test]
    fn risk_classification_slug_is_stable() {
        assert_eq!(RiskClassification::HighRisk.slug(), "high-risk");
        assert_eq!(RiskClassification::LimitedRisk.slug(), "limited-risk");
        assert_eq!(RiskClassification::Prohibited.slug(), "prohibited");
        assert_eq!(RiskClassification::Minimal.slug(), "minimal");
    }
}
