//! The SpiceDB calls the daemon makes, each under a deadline. A missed deadline is an error: the caller decides what
//! "unavailable" means, and no caller turns it into an empty set.

use crate::authzed::api::v1 as pb;
use crate::config::Consistency;
use futures::StreamExt;
use pb::{
    permissions_service_client::PermissionsServiceClient,
    schema_service_client::SchemaServiceClient, watch_service_client::WatchServiceClient,
};
use std::{net::IpAddr, path::PathBuf, time::Duration};
use tonic::{
    metadata::MetadataValue,
    service::{interceptor::InterceptedService, Interceptor},
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
    Code, Request, Status,
};

/// How the daemon reaches SpiceDB. `https://` is TLS, verified against the public roots or `ca_file`. `http://` sends
/// the preshared key in the clear, so it is refused to a public address unless `insecure`; to a loopback or private
/// address it is allowed (a private one with a warning at start).
#[derive(Debug, Clone, Default)]
pub struct Transport {
    /// A PEM file of the CA that signed SpiceDB's certificate, for a private CA.
    pub ca_file: Option<PathBuf>,
    /// Allow `http://` to a public address.
    pub insecure: bool,
}

/// Where a plaintext endpoint resolves.
#[derive(Debug, PartialEq, Eq)]
enum Reach {
    Loopback,
    Private,
    Public,
}

fn reach(addrs: &[IpAddr]) -> Reach {
    let private = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    };
    if !addrs.is_empty() && addrs.iter().all(IpAddr::is_loopback) {
        Reach::Loopback
    } else if !addrs.is_empty() && addrs.iter().all(private) {
        Reach::Private
    } else {
        Reach::Public
    }
}

#[derive(Clone)]
pub struct Bearer(MetadataValue<tonic::metadata::Ascii>);

impl Interceptor for Bearer {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        req.metadata_mut().insert("authorization", self.0.clone());
        Ok(req)
    }
}

type Svc = InterceptedService<Channel, Bearer>;

#[derive(Clone)]
pub struct SpiceDb {
    perms: PermissionsServiceClient<Svc>,
    schema: SchemaServiceClient<Svc>,
    watch: WatchServiceClient<Svc>,
    consistency: Consistency,
}

/// One relationship write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleOp {
    pub touch: bool,
    pub resource: String,
    pub relation: String,
    pub subject: String,
}

fn object(r: &str) -> Result<pb::ObjectReference, Status> {
    let (t, id) = r
        .split_once(':')
        .ok_or_else(|| Status::invalid_argument("not a type:id reference"))?;
    Ok(pb::ObjectReference {
        object_type: t.into(),
        object_id: id.into(),
    })
}

fn subject(r: &str) -> Result<pb::SubjectReference, Status> {
    let (obj, rel) = match r.split_once('#') {
        Some((o, rel)) => (o, rel.to_string()),
        None => (r, String::new()),
    };
    Ok(pb::SubjectReference {
        object: Some(object(obj)?),
        optional_relation: rel,
    })
}

/// What a refused write means for its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// SpiceDB refuses THIS tuple and always will (`INVALID_ARGUMENT`): quarantine it, deliver the rest.
    Quarantine,
    /// The schema SpiceDB holds lacks the relation ("relation/permission `x` not found under definition `y`",
    /// `FAILED_PRECONDITION`). Possibly permanent, possibly a schema that lags a deploy: the row is left undelivered and
    /// tried again, never holding the rest, and quarantined only once it has failed for longer than the settle window.
    Defer,
    /// Anything else, including "object definition `x` not found" (a store with no schema yet): an outage.
    Outage,
}

/// How a refused write is treated (see `Refusal`). The `Defer` case is told by SpiceDB's message, as its code alone
/// also covers a store with no schema at all.
#[must_use]
pub fn refusal(s: &Status) -> Refusal {
    match s.code() {
        Code::InvalidArgument => Refusal::Quarantine,
        Code::FailedPrecondition
            if s.message().contains("relation/permission")
                && s.message().contains("not found under definition") =>
        {
            Refusal::Defer
        }
        _ => Refusal::Outage,
    }
}

/// The whole request may contain a tuple SpiceDB refuses on its own: re-send it one row at a time.
#[must_use]
pub fn may_be_one_tuple(s: &Status) -> bool {
    refusal(s) != Refusal::Outage
}

/// A subject SpiceDB will not even parse: it contributes nothing to a lookup (fail closed for that identity, never
/// for the principal). Only `INVALID_ARGUMENT`: a `FAILED_PRECONDITION` on a lookup (a store with no schema) is an
/// outage, because reading it as "nothing" would project every principal empty.
#[must_use]
pub fn is_rejected_subject(s: &Status) -> bool {
    s.code() == Code::InvalidArgument
}

impl SpiceDb {
    /// A client for `endpoint`. The channel connects lazily, on the first call; `async` is kept so connecting eagerly
    /// later is not an API change.
    pub async fn connect(
        endpoint: &str,
        token: &str,
        consistency: Consistency,
    ) -> anyhow::Result<Self> {
        Self::connect_with(endpoint, token, consistency, &Transport::default()).await
    }

    /// `connect`, with the transport the configuration asks for.
    pub async fn connect_with(
        endpoint: &str,
        token: &str,
        consistency: Consistency,
        transport: &Transport,
    ) -> anyhow::Result<Self> {
        let url = url::Url::parse(endpoint)?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("spicedb_endpoint {endpoint} names no host"))?;
        let mut builder = Endpoint::from_shared(endpoint.to_string())?;
        match url.scheme() {
            "https" => {
                let mut tls = ClientTlsConfig::new()
                    .domain_name(host.trim_matches(['[', ']']))
                    .with_webpki_roots();
                if let Some(path) = &transport.ca_file {
                    let pem = std::fs::read(path)
                        .map_err(|e| anyhow::anyhow!("spicedb_ca_file {}: {e}", path.display()))?;
                    tls = tls.ca_certificate(Certificate::from_pem(pem));
                }
                builder = builder.tls_config(tls)?;
            }
            "http" if transport.insecure => {
                tracing::warn!("spicedb_insecure: the SpiceDB key is sent without TLS");
            }
            "http" => {
                let port = url.port_or_known_default().unwrap_or(80);
                let addrs: Vec<IpAddr> = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|e| anyhow::anyhow!("spicedb_endpoint {endpoint}: {e}"))?
                    .map(|a| a.ip())
                    .collect();
                match reach(&addrs) {
                    Reach::Loopback => {}
                    Reach::Private => tracing::warn!(
                        "spicedb_endpoint is plaintext http:// on a private network: the SpiceDB key is sent without TLS"
                    ),
                    Reach::Public => anyhow::bail!(
                        "spicedb_endpoint {endpoint} is plaintext to a public address, which would send the SpiceDB \
                         key in the clear: use https://, or set spicedb_insecure = true"
                    ),
                }
            }
            other => anyhow::bail!("spicedb_endpoint must be http:// or https://, not {other}://"),
        }
        let channel = builder
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            // A Watch stream on a dead connection must end, not hang: it is what keeps the heartbeat going.
            .http2_keep_alive_interval(Duration::from_secs(5))
            .keep_alive_timeout(Duration::from_secs(5))
            .keep_alive_while_idle(true)
            .connect_lazy();
        let bearer = Bearer(format!("Bearer {token}").parse()?);
        Ok(Self {
            perms: PermissionsServiceClient::with_interceptor(channel.clone(), bearer.clone()),
            schema: SchemaServiceClient::with_interceptor(channel.clone(), bearer.clone()),
            watch: WatchServiceClient::with_interceptor(channel, bearer),
            consistency,
        })
    }

    fn consistency(&self, token: Option<&str>) -> pb::Consistency {
        use pb::consistency::Requirement;
        let req = match (self.consistency, token) {
            (Consistency::AtLeastAsFresh, Some(t)) if !t.is_empty() => {
                Requirement::AtLeastAsFresh(pb::ZedToken {
                    token: t.to_string(),
                })
            }
            _ => Requirement::FullyConsistent(true),
        };
        pb::Consistency {
            requirement: Some(req),
        }
    }

    fn req<T>(msg: T, deadline: Duration) -> Request<T> {
        let mut r = Request::new(msg);
        r.set_timeout(deadline);
        r
    }

    pub async fn read_schema(&self, deadline: Duration) -> Result<String, Status> {
        let mut schema = self.schema.clone();
        let resp = bounded(
            deadline,
            schema.read_schema(Self::req(pb::ReadSchemaRequest {}, deadline)),
        )
        .await?;
        Ok(resp.into_inner().schema_text)
    }

    /// Write a schema. The daemon never does this itself; it is for tests and examples.
    pub async fn write_schema(&self, schema: &str, deadline: Duration) -> Result<(), Status> {
        let mut client = self.schema.clone();
        let msg = pb::WriteSchemaRequest {
            schema: schema.into(),
        };
        bounded(deadline, client.write_schema(Self::req(msg, deadline))).await?;
        Ok(())
    }

    /// Delete every relationship of a resource type. For tests and examples; the daemon never does this.
    pub async fn delete_all(&self, resource_type: &str, deadline: Duration) -> Result<(), Status> {
        let mut client = self.perms.clone();
        let msg = pb::DeleteRelationshipsRequest {
            relationship_filter: Some(pb::RelationshipFilter {
                resource_type: resource_type.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        bounded(
            deadline,
            client.delete_relationships(Self::req(msg, deadline)),
        )
        .await?;
        Ok(())
    }

    /// Write relationships, returning the token SpiceDB wrote them at.
    pub async fn write<'a>(
        &self,
        ops: impl IntoIterator<Item = &'a TupleOp>,
        deadline: Duration,
    ) -> Result<Option<String>, Status> {
        let updates = ops
            .into_iter()
            .map(|o| {
                Ok(pb::RelationshipUpdate {
                    operation: if o.touch {
                        pb::relationship_update::Operation::Touch as i32
                    } else {
                        pb::relationship_update::Operation::Delete as i32
                    },
                    relationship: Some(pb::Relationship {
                        resource: Some(object(&o.resource)?),
                        relation: o.relation.clone(),
                        subject: Some(subject(&o.subject)?),
                        optional_caveat: None,
                        optional_expires_at: None,
                    }),
                })
            })
            .collect::<Result<Vec<_>, Status>>()?;
        let msg = pb::WriteRelationshipsRequest {
            updates,
            ..Default::default()
        };
        let mut perms = self.perms.clone();
        let resp = bounded(
            deadline,
            perms.write_relationships(Self::req(msg, deadline)),
        )
        .await?;
        Ok(resp.into_inner().written_at.map(|t| t.token))
    }

    /// Every resource of `resource_type` the subject holds `permission` on. The whole stream is bounded.
    pub async fn lookup_resources(
        &self,
        resource_type: &str,
        permission: &str,
        subject_ref: &str,
        token: Option<&str>,
        deadline: Duration,
    ) -> Result<Vec<String>, Status> {
        let msg = pb::LookupResourcesRequest {
            consistency: Some(self.consistency(token)),
            resource_object_type: resource_type.into(),
            permission: permission.into(),
            subject: Some(subject(subject_ref)?),
            ..Default::default()
        };
        let mut perms = self.perms.clone();
        bounded(deadline, async move {
            let mut stream = perms
                .lookup_resources(Self::req(msg, deadline))
                .await?
                .into_inner();
            let mut out = Vec::new();
            while let Some(item) = stream.next().await {
                let item = item?;
                if item.permissionship == pb::LookupPermissionship::HasPermission as i32 {
                    out.push(item.resource_object_id);
                }
                // A conditional answer (a caveat SpiceDB could not evaluate) is NOT readable: fail closed.
            }
            Ok(out)
        })
        .await
    }

    /// Every subject of `subject_type` holding `permission` on a resource. With `subject_relation`, the subject sets
    /// of that type and relation that hold it (`group:acme/eng#member`).
    pub async fn lookup_subjects(
        &self,
        resource: &str,
        permission: &str,
        subject_type: &str,
        subject_relation: Option<&str>,
        token: Option<&str>,
        deadline: Duration,
    ) -> Result<Vec<String>, Status> {
        let msg = pb::LookupSubjectsRequest {
            consistency: Some(self.consistency(token)),
            resource: Some(object(resource)?),
            permission: permission.into(),
            subject_object_type: subject_type.into(),
            optional_subject_relation: subject_relation.unwrap_or_default().into(),
            ..Default::default()
        };
        let mut perms = self.perms.clone();
        let st = subject_type.to_string();
        let rel = subject_relation
            .map(|r| format!("#{r}"))
            .unwrap_or_default();
        bounded(deadline, async move {
            let mut stream = perms
                .lookup_subjects(Self::req(msg, deadline))
                .await?
                .into_inner();
            let mut out = Vec::new();
            while let Some(item) = stream.next().await {
                if let Some(s) = item?.subject {
                    if s.permissionship == pb::LookupPermissionship::HasPermission as i32 {
                        out.push(format!("{st}:{}{rel}", s.subject_object_id));
                    }
                }
            }
            Ok(out)
        })
        .await
    }

    /// Page through every relationship of a resource type under an id prefix.
    pub async fn read_relationships(
        &self,
        resource_type: &str,
        id_prefix: &str,
        deadline: Duration,
    ) -> Result<Vec<pb::Relationship>, Status> {
        let mut out = Vec::new();
        let mut cursor: Option<pb::Cursor> = None;
        loop {
            let msg = pb::ReadRelationshipsRequest {
                consistency: Some(self.consistency(None)),
                relationship_filter: Some(pb::RelationshipFilter {
                    resource_type: resource_type.into(),
                    optional_resource_id_prefix: id_prefix.into(),
                    ..Default::default()
                }),
                optional_limit: PAGE,
                optional_cursor: cursor.take(),
            };
            let mut perms = self.perms.clone();
            let page: Vec<pb::ReadRelationshipsResponse> = bounded(deadline, async move {
                let mut stream = perms
                    .read_relationships(Self::req(msg, deadline))
                    .await?
                    .into_inner();
                let mut page = Vec::new();
                while let Some(item) = stream.next().await {
                    page.push(item?);
                }
                Ok(page)
            })
            .await?;
            let n = page.len();
            for r in page {
                cursor = r.after_result_cursor;
                if let Some(rel) = r.relationship {
                    out.push(rel);
                }
            }
            if n < PAGE as usize || cursor.is_none() {
                return Ok(out);
            }
        }
    }

    /// SpiceDB's current revision, read-only: a fully consistent CheckPermission (of `resource#permission@subject`,
    /// whose answer is ignored) is evaluated at the head revision and says which (`checked_at`).
    pub async fn head_revision(
        &self,
        resource: &str,
        permission: &str,
        subject_ref: &str,
        deadline: Duration,
    ) -> Result<String, Status> {
        let mut perms = self.perms.clone();
        let msg = pb::CheckPermissionRequest {
            consistency: Some(pb::Consistency {
                requirement: Some(pb::consistency::Requirement::FullyConsistent(true)),
            }),
            resource: Some(object(resource)?),
            permission: permission.into(),
            subject: Some(subject(subject_ref)?),
            ..Default::default()
        };
        let resp = bounded(deadline, perms.check_permission(Self::req(msg, deadline))).await?;
        resp.into_inner()
            .checked_at
            .map(|t| t.token)
            .ok_or_else(|| Status::internal("CheckPermission returned no checked_at"))
    }

    /// The Watch stream from a position (or from now).
    pub async fn watch(
        &self,
        object_types: Vec<String>,
        from: Option<String>,
    ) -> Result<tonic::Streaming<pb::WatchResponse>, Status> {
        let msg = pb::WatchRequest {
            optional_object_types: object_types,
            optional_start_cursor: from.map(|token| pb::ZedToken { token }),
            optional_relationship_filters: vec![],
            optional_update_kinds: vec![
                pb::WatchKind::IncludeRelationshipUpdates as i32,
                pb::WatchKind::IncludeSchemaUpdates as i32,
                pb::WatchKind::IncludeCheckpoints as i32,
            ],
        };
        let mut watch = self.watch.clone();
        Ok(watch.watch(Request::new(msg)).await?.into_inner())
    }
}

/// Relationships per ReadRelationships page.
const PAGE: u32 = 1_000;

/// A backstop beside the gRPC deadline: a channel that does not honour even that still ends.
async fn bounded<T>(
    deadline: Duration,
    fut: impl std::future::Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    match tokio::time::timeout(deadline + Duration::from_secs(1), fut).await {
        Ok(r) => r,
        Err(_) => Err(Status::deadline_exceeded("no reply within the deadline")),
    }
}

#[cfg(test)]
mod tests {
    use super::{reach, Reach};
    use std::net::IpAddr;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn plaintext_is_judged_by_where_the_endpoint_resolves() {
        assert_eq!(reach(&ips(&["127.0.0.1", "::1"])), Reach::Loopback);
        assert_eq!(reach(&ips(&["10.0.0.5"])), Reach::Private);
        assert_eq!(reach(&ips(&["172.18.0.3", "fd00::3"])), Reach::Private);
        assert_eq!(reach(&ips(&["10.0.0.5", "203.0.113.7"])), Reach::Public);
        assert_eq!(reach(&ips(&["203.0.113.7"])), Reach::Public);
        assert_eq!(reach(&[]), Reach::Public);
    }
}
