//! The SpiceDB calls the daemon makes, each under a deadline. A missed deadline is an error: the caller decides what
//! "unavailable" means, and no caller turns it into an empty set.

use crate::authzed::api::v1 as pb;
use crate::config::Consistency;
use futures::StreamExt;
use pb::{
    permissions_service_client::PermissionsServiceClient,
    schema_service_client::SchemaServiceClient, watch_service_client::WatchServiceClient,
};
use std::time::Duration;
use tonic::{
    metadata::MetadataValue,
    service::{interceptor::InterceptedService, Interceptor},
    transport::{Channel, Endpoint},
    Code, Request, Status,
};

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

/// SpiceDB refuses this request, and will refuse it however often it is sent. FAILED_PRECONDITION counts: the daemon
/// sends no preconditions, so on a write it means the tuple names a relation or type the schema does not define
/// (SpiceDB 1.56 answers an unknown relation that way, not with INVALID_ARGUMENT), and resending it would stall every
/// later permission change behind it.
pub fn is_invalid(s: &Status) -> bool {
    matches!(s.code(), Code::InvalidArgument | Code::FailedPrecondition)
}

/// A subject SpiceDB will not even parse: it contributes nothing to a lookup (fail closed for that identity, never
/// for the principal). Only INVALID_ARGUMENT: a FAILED_PRECONDITION on a lookup (a store with no schema) is an
/// outage, because reading it as "nothing" would project every principal empty.
pub fn is_rejected_subject(s: &Status) -> bool {
    s.code() == Code::InvalidArgument
}

impl SpiceDb {
    pub async fn connect(
        endpoint: &str,
        token: &str,
        consistency: Consistency,
    ) -> anyhow::Result<Self> {
        let channel = Endpoint::from_shared(endpoint.to_string())?
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Some(Duration::from_secs(30)))
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
    pub async fn write(
        &self,
        ops: &[TupleOp],
        deadline: Duration,
    ) -> Result<Option<String>, Status> {
        let mut updates = Vec::with_capacity(ops.len());
        for o in ops {
            updates.push(pb::RelationshipUpdate {
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
            });
        }
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

    /// Every subject of `subject_type` holding `permission` on a resource.
    pub async fn lookup_subjects(
        &self,
        resource: &str,
        permission: &str,
        subject_type: &str,
        token: Option<&str>,
        deadline: Duration,
    ) -> Result<Vec<String>, Status> {
        let msg = pb::LookupSubjectsRequest {
            consistency: Some(self.consistency(token)),
            resource: Some(object(resource)?),
            permission: permission.into(),
            subject_object_type: subject_type.into(),
            ..Default::default()
        };
        let mut perms = self.perms.clone();
        let st = subject_type.to_string();
        bounded(deadline, async move {
            let mut stream = perms
                .lookup_subjects(Self::req(msg, deadline))
                .await?
                .into_inner();
            let mut out = Vec::new();
            while let Some(item) = stream.next().await {
                if let Some(s) = item?.subject {
                    if s.permissionship == pb::LookupPermissionship::HasPermission as i32 {
                        out.push(format!("{st}:{}", s.subject_object_id));
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
                optional_limit: 1_000,
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
            if n < 1_000 || cursor.is_none() {
                return Ok(out);
            }
        }
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
