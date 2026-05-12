//! Stub server-side implementation of the channel-partner service.
//! Every rpc returns `Status::unimplemented` with a stable code so
//! a v0.1 swap to the real implementation doesn't surprise clients.

use tonic::{Request, Response, Status, Streaming};

use crate::proto::channel_partner::v1::{
    channel_partner_server::ChannelPartner, DeprovisionTenantRequest, DeprovisionTenantResponse,
    ListTenantsRequest, ListTenantsResponse, ProvisionTenantRequest, ProvisionTenantResponse,
    TelemetryAcknowledgement, TelemetryEvent, UpdateTenantRequest, UpdateTenantResponse,
};

/// v0.0.x stub. Construct with [`StubChannelPartner::new`] and pass
/// to [`crate::ChannelPartnerServer::new`] to wire as a tower
/// service. Every rpc returns `unimplemented` with a stable message
/// so callers can distinguish "wrong endpoint" from "feature not yet
/// shipped".
#[derive(Default)]
pub struct StubChannelPartner;

impl StubChannelPartner {
    /// Construct a fresh stub. No state today; v0.1 wires the
    /// SqliteStore + license-chain extractor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[tonic::async_trait]
impl ChannelPartner for StubChannelPartner {
    async fn provision_tenant(
        &self,
        _req: Request<ProvisionTenantRequest>,
    ) -> Result<Response<ProvisionTenantResponse>, Status> {
        Err(unimplemented_stub("provision_tenant"))
    }

    async fn update_tenant(
        &self,
        _req: Request<UpdateTenantRequest>,
    ) -> Result<Response<UpdateTenantResponse>, Status> {
        Err(unimplemented_stub("update_tenant"))
    }

    async fn deprovision_tenant(
        &self,
        _req: Request<DeprovisionTenantRequest>,
    ) -> Result<Response<DeprovisionTenantResponse>, Status> {
        Err(unimplemented_stub("deprovision_tenant"))
    }

    async fn list_tenants(
        &self,
        _req: Request<ListTenantsRequest>,
    ) -> Result<Response<ListTenantsResponse>, Status> {
        Err(unimplemented_stub("list_tenants"))
    }

    async fn stream_telemetry(
        &self,
        _req: Request<Streaming<TelemetryEvent>>,
    ) -> Result<Response<TelemetryAcknowledgement>, Status> {
        Err(unimplemented_stub("stream_telemetry"))
    }
}

fn unimplemented_stub(method: &str) -> Status {
    Status::unimplemented(format!(
        "computeza.channel_partner.v1.ChannelPartner/{method} is not yet implemented; \
         v0.0.x ships the proto + transport scaffold, v0.1+ wires the per-rpc logic"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::channel_partner::v1::ProvisionTenantRequest;

    #[tokio::test]
    async fn every_rpc_returns_unimplemented_today() {
        let stub = StubChannelPartner::new();
        let req = Request::new(ProvisionTenantRequest {
            tenant_id: vec![1, 2, 3, 4],
            display_name: "Acme Corp".into(),
            seat_cap: 50,
            branding: None,
        });
        let err = stub.provision_tenant(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert!(err.message().contains("not yet implemented"));
    }

    #[tokio::test]
    async fn unimplemented_messages_name_the_rpc() {
        let stub = StubChannelPartner::new();
        let req = Request::new(crate::proto::channel_partner::v1::ListTenantsRequest {
            page_token: String::new(),
            page_size: 10,
        });
        let err = stub.list_tenants(req).await.unwrap_err();
        assert!(err.message().contains("list_tenants"));
    }
}
