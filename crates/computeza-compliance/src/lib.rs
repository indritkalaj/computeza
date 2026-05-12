//! Computeza compliance evidence -- EU AI Act primitives.
//!
//! Computeza is a data platform, not an AI system. But the AI
//! Workspace (spec section 6) means our **customers deploy AI
//! systems** on top of us, often in high-risk regimes (finance,
//! healthcare, credit scoring, hiring). The EU AI Act (Regulation
//! (EU) 2024/1689) places obligations on **deployers** and
//! **providers** of high-risk systems -- our customers are
//! typically deployers, occasionally providers. The Act's high-risk
//! obligations become applicable on **2 August 2026**.
//!
//! This crate ships the evidence primitives a deployer needs to
//! demonstrate compliance:
//!
//! - [`RiskClassification`] -- the four-bucket taxonomy from Title
//!   II of the Act (Prohibited / High-Risk / Limited / Minimal),
//!   with Article references baked in for auditability.
//! - [`ModelCard`] -- structured metadata covering training data
//!   summary, intended use, known limitations, evaluation metrics,
//!   and human oversight design. Maps to Article 11 (technical
//!   documentation) requirements.
//! - [`ModelCardRegistry`] -- JSONL-backed persistence (one card per
//!   line) under the operator's state directory.
//! - [`transparency_banner`] -- HTML snippet for Article 50
//!   transparency obligations (telling users they're interacting
//!   with an AI system).
//!
//! # Legal positioning
//!
//! Computeza Inc. itself is mostly a tooling vendor. The product
//! supports the deployer's compliance posture; it does NOT make the
//! deployer compliant by itself. Marketing must reflect this: we
//! say "designed to support EU AI Act deployer compliance",
//! never "EU AI Act compliant".
//!
//! # What's in scope for v0.0.x
//!
//! - Types + persistence + registry helpers.
//! - HTML primitives that render into the operator console.
//! - Audit-log linkage (the deployer can produce a signed audit
//!   trail of every model card registration, status change, and
//!   risk re-classification).
//!
//! # What's deferred
//!
//! - Conformity-assessment workflow (Article 43) -- the deployer-
//!   side checklist that produces a signed PDF. Lands in v0.1+.
//! - Article 86 right-to-explanation: capture SHAP /
//!   counterfactuals on each high-risk decision and stash in the
//!   audit log. Requires the inference path to integrate (v0.1+).
//! - Article 53/55 GPAI provider obligations -- when our customers
//!   train + place a model on the market.

#![warn(missing_docs)]

mod eu_ai_act;
mod model_card;
mod transparency;

pub use eu_ai_act::{
    eu_ai_act_articles, ArticleEvidence, EuAiActArticle, RiskClassification, RiskJustification,
};
pub use model_card::{
    AssessmentEntry, ModelCard, ModelCardError, ModelCardRegistry, ModelDeployment,
};
pub use transparency::{transparency_banner, TransparencyContext};
