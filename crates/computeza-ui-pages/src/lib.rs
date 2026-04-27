//! Computeza UI pages — page modules organised around the operator's mental
//! model of the lakehouse.
//!
//! Per spec §4.2, the console is organised by user task — Overview,
//! Workspaces (multi-tenant only), Identity, Storage, Catalogs, Compute,
//! AI Workspace, Pipelines, Dashboards, Audit & Compliance — not by
//! underlying component. This abstraction lets us swap components without
//! retraining users.
//!
//! Every page follows the List → Detail → Edit interaction pattern (spec
//! §4.4). Detail pages always carry Overview / Permissions / Code / Audit
//! tabs; the Code tab is the GitOps escape hatch (spec §4.4 "The Code Tab").
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
