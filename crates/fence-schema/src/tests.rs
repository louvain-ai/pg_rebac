use super::*;

/// The shape slice 1 is built for: a projected type whose view is a union of direct relations, a self-arrow, and a
/// relation reached through an arrow to another type. (Not any one product's schema; the generic shape.)
const SHAPE: &str = r#"
/** a person */
definition user {}

definition workspace {
  relation member: user
  relation admin: user
}

definition container {
  relation workspace: workspace
  relation parent: container // a file under its space
  relation member: user
  relation editor: user
  relation manager: user
  permission view = member + editor + manager + workspace->admin + parent->view
  permission edit = editor + manager + parent->edit
}

definition derived {
  relation source: container
  permission view = source.all(view)
  permission view_any = source->view
}
"#;

fn rel(t: &str, r: &str, k: Kind, target: Option<&str>, through: Option<&str>) -> ViewRelation {
    ViewRelation {
        resource_type: t.into(),
        relation: r.into(),
        kind: k,
        target_type: target.map(Into::into),
        through: through.map(Into::into),
    }
}

#[test]
fn derives_the_supported_shape() {
    let s = parse(SHAPE).unwrap();
    let got = closure(&s, "container", "view").unwrap();
    let mut want = vec![
        rel("container", "editor", Kind::Direct, None, None),
        rel("container", "manager", Kind::Direct, None, None),
        rel("container", "member", Kind::Direct, None, None),
        rel("container", "parent", Kind::Arrow, Some("container"), None),
        rel(
            "container",
            "workspace",
            Kind::Arrow,
            Some("workspace"),
            None,
        ),
        rel("workspace", "admin", Kind::Via, None, Some("workspace")),
    ];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn the_other_permissions_do_not_leak_in() {
    let s = parse(SHAPE).unwrap();
    let got = closure(&s, "container", "view").unwrap();
    assert!(!got
        .iter()
        .any(|r| r.resource_type == "workspace" && r.relation == "member"));
    assert!(!got.iter().any(|r| r.resource_type == "derived"));
}

fn refused(schema: &str, needle: &str) {
    let s = parse(schema).unwrap();
    match closure(&s, "doc", "view") {
        Err(ClosureError::Refused(r)) => {
            assert!(
                r.iter().any(|x| x.reason.contains(needle)),
                "expected a refusal mentioning {needle:?}, got {r:?}"
            )
        }
        other => panic!("expected a refusal mentioning {needle:?}, got {other:?}"),
    }
}

#[test]
fn refuses_exclusion() {
    refused(
        "definition user {} definition doc { relation reader: user relation banned: user permission view = reader - banned }",
        "exclusion",
    );
}

#[test]
fn refuses_exclusion_inside_a_reached_permission() {
    refused(
        "definition user {} definition doc { relation reader: user relation banned: user \
         permission ok = reader - banned permission view = ok + reader }",
        "exclusion",
    );
}

#[test]
fn refuses_intersection_and_all() {
    refused(
        "definition user {} definition doc { relation a: user relation b: user permission view = a & b }",
        "intersection",
    );
    refused(
        "definition user {} definition doc { relation p: doc relation r: user permission view = r + p.all(view) }",
        ".all",
    );
}

#[test]
fn refuses_caveats_expiry_wildcards_and_subject_sets() {
    refused(
        "caveat on_net(ip ipaddress, net string) { ip.in_cidr(net) } definition user {} \
         definition doc { relation reader: user with on_net permission view = reader }",
        "caveated",
    );
    refused(
        "use expiration definition user {} definition doc { relation reader: user with expiration permission view = reader }",
        "expiring",
    );
    refused(
        "definition user {} definition doc { relation reader: user:* permission view = reader }",
        "wildcard",
    );
    refused(
        "definition user {} definition group { relation member: user } \
         definition doc { relation reader: group#member permission view = reader }",
        "subject set",
    );
}

#[test]
fn refuses_two_hops() {
    refused(
        "definition user {} definition org { relation admin: user } \
         definition ws { relation org: org permission admin = org->admin } \
         definition doc { relation ws: ws relation r: user permission view = r + ws->admin }",
        "more than one hop",
    );
}

#[test]
fn unknown_names_are_errors() {
    let s = parse(SHAPE).unwrap();
    assert!(matches!(
        closure(&s, "nope", "view"),
        Err(ClosureError::UnknownType(_))
    ));
    assert!(matches!(
        closure(&s, "container", "nope"),
        Err(ClosureError::UnknownPermission(_))
    ));
}

#[test]
fn parse_errors_name_a_line() {
    let e = parse("definition user {}\ndefinition doc {\n relation : user\n}").unwrap_err();
    assert_eq!(e.line, 3);
    assert!(parse("definition a {} definition a {}").is_err());
}

#[test]
fn parses_namespaced_types_and_parentheses() {
    let s = parse(
        "definition acme/user {} definition acme/doc { relation r: acme/user relation w: acme/user \
         permission view = (r + w) + nil }",
    )
    .unwrap();
    assert_eq!(closure(&s, "acme/doc", "view").unwrap().len(), 2);
}
