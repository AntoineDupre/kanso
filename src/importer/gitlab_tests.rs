use super::*;
use serde_json::{json, Value};

fn user(id: u64, name: &str) -> Value {
    json!({"id":id,"username":name})
}
fn item(iid: u64, author: &str) -> Value {
    json!({"iid":iid,"project_id":123,"title":"An issue","description":"**body**","state":"opened","author":user(if author=="me" {1} else {2},author),"assignees":[user(1,"me")],"draft":false})
}
fn response(value: Value) -> Response {
    Response {
        body: serde_json::to_vec(&value).unwrap(),
        next: Some(String::new()),
    }
}
fn preview(
    category: &str,
    filters: HashMap<String, String>,
    items: Vec<Value>,
) -> Result<Vec<Candidate>, ProviderError> {
    fetch(
        &format!("https://gitlab.com|group/nested/repo|{category}"),
        &filters,
        |endpoint, _| {
            Ok(if endpoint == "/api/v4/user" {
                response(user(1, "me"))
            } else if endpoint.starts_with("/api/v4/users?") {
                response(json!([user(2, "other")]))
            } else if endpoint == "/api/v4/projects/group%2Fnested%2Frepo" {
                response(json!({"id":123,"path_with_namespace":"group/nested/repo"}))
            } else {
                response(json!(items))
            })
        },
    )
}
fn mr(iid: &str, review_state: &str, author_id: u64) -> Value {
    json!({"iid":iid,"title":"An MR","description":"**body**","state":"opened","draft":false,"author":{"id":format!("gid://gitlab/User/{author_id}"),"username":"other"},"reviewers":{"nodes":[{"username":"me","mergeRequestInteraction":{"reviewState":review_state}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}})
}
fn graph(items: Vec<Value>, more: bool, cursor: Option<&str>) -> Value {
    json!({"data":{"project":{"id":"gid://gitlab/Project/123","mergeRequests":{"nodes":items,"pageInfo":{"hasNextPage":more,"endCursor":cursor}}}}})
}
fn review_preview(items: Vec<Value>) -> Result<Vec<Candidate>, ProviderError> {
    fetch(
        "https://gitlab.com|group/nested/repo|review",
        &HashMap::new(),
        |endpoint, _| {
            Ok(if endpoint == "/api/v4/user" {
                response(user(1, "me"))
            } else if endpoint.starts_with("/api/v4/projects/") {
                response(json!({"id":123,"path_with_namespace":"group/nested/repo"}))
            } else {
                assert!(endpoint.starts_with("/api/graphql?query="));
                assert!(endpoint.contains("reviewerUsername"));
                assert!(!endpoint.contains("assignee"));
                response(graph(items.clone(), false, None))
            })
        },
    )
}

#[test]
fn instance_and_nested_paths_are_safe_and_canonical() {
    assert_eq!(
        normalize_instance("https://GITLAB.com:443/"),
        Some("https://gitlab.com".into())
    );
    assert_eq!(
        normalize_instance("https://gitlab.example:8443"),
        Some("https://gitlab.example:8443".into())
    );
    assert!(valid_repository("group/sub-group/deeper/my_repo.git"));
    for value in [
        "http://gitlab.com",
        "https://secret@gitlab.com",
        "https://gitlab.com/path",
        "https://gitlab.com?token=secret",
        "https://gitlab.com/#secret",
        "https://gitlab.com\\evil",
        "https://gitlab.com:abc",
        "https://gitlab.com:0",
        "https://gitlab.com|anything",
        "https://gitlab.com.",
    ] {
        assert!(normalize_instance(value).is_none(), "{value}");
    }
    for value in [
        "single",
        "group//repo",
        "group/../repo",
        "/group/repo",
        "group/repo/",
        "group/repo?token=secret",
        "group/repo#frag",
        "group/a%2Fb",
    ] {
        assert!(!valid_repository(value), "{value}");
    }
}

#[test]
fn issues_default_me_explicit_user_all_and_open_only() {
    let mut other = item(2, "other");
    other["assignees"] = json!([user(2, "other")]);
    let mut closed = item(3, "other");
    closed["state"] = json!("closed");
    let items = vec![item(1, "other"), other, closed];
    assert_eq!(
        preview("issues", HashMap::new(), items.clone())
            .unwrap()
            .len(),
        1
    );
    let explicit = preview(
        "issues",
        HashMap::from([("user".into(), "other".into())]),
        items.clone(),
    )
    .unwrap();
    assert_eq!(explicit.len(), 1);
    assert!(explicit[0].identity.ends_with("issue:2"));
    assert_eq!(
        preview("issues", HashMap::from([("user".into(), "".into())]), items)
            .unwrap()
            .len(),
        2
    );
    let mut malformed = item(1, "other");
    malformed.as_object_mut().unwrap().remove("assignees");
    assert!(preview("issues", HashMap::new(), vec![malformed]).is_err());
}

#[test]
fn follow_includes_drafts_and_selected_author_but_excludes_closed_merged() {
    let mut draft = item(1, "me");
    draft["draft"] = json!(true);
    let mut merged = item(2, "me");
    merged["state"] = json!("merged");
    let mut closed = item(3, "me");
    closed["state"] = json!("closed");
    let result = preview(
        "follow",
        HashMap::new(),
        vec![draft, merged, closed, item(4, "other")],
    )
    .unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].identity.ends_with("mr:1"));
    assert_eq!(
        preview(
            "follow",
            HashMap::from([("user".into(), "other".into())]),
            vec![item(4, "other")]
        )
        .unwrap()
        .len(),
        1
    );
    assert!(preview(
        "follow",
        HashMap::from([("user".into(), "".into())]),
        vec![]
    )
    .is_err());
}

#[test]
fn strict_requested_reviews_use_reviewer_state_and_never_assignees() {
    let mut draft = mr("3", "UNREVIEWED", 2);
    draft["draft"] = json!(true);
    let mut no_request = mr("4", "UNREVIEWED", 2);
    no_request["reviewers"]["nodes"] = json!([]);
    no_request["assignees"] = json!([user(1, "me")]);
    let mut closed = mr("5", "UNREVIEWED", 2);
    closed["state"] = json!("closed");
    let mut merged = mr("6", "UNREVIEWED", 2);
    merged["state"] = json!("merged");
    let mut items = vec![
        mr("1", "UNREVIEWED", 2),
        mr("2", "REVIEW_STARTED", 2),
        draft,
        no_request,
        closed,
        merged,
        mr("7", "UNREVIEWED", 1),
    ];
    for (i, state) in ["REVIEWED", "APPROVED", "REQUESTED_CHANGES", "UNAPPROVED"]
        .iter()
        .enumerate()
    {
        items.push(mr(&(i + 8).to_string(), state, 2));
    }
    let result = review_preview(items).unwrap();
    assert_eq!(result.len(), 2);
    assert!(review_preview(vec![mr("1", "NEW_UNKNOWN", 2)]).is_err());
    let mut missing = mr("1", "UNREVIEWED", 2);
    missing["reviewers"]["nodes"][0]["mergeRequestInteraction"] = Value::Null;
    assert!(review_preview(vec![missing]).is_err());
    let mut partial = mr("1", "UNREVIEWED", 2);
    partial["reviewers"]["pageInfo"]["hasNextPage"] = json!(true);
    assert!(review_preview(vec![partial]).is_err());
}

#[test]
fn stable_identity_survives_rename_and_category_but_instances_are_distinct() {
    let original = Project {
        id: 123,
        path_with_namespace: "group/nested/repo".into(),
    };
    let renamed = Project {
        id: 123,
        path_with_namespace: "renamed/project".into(),
    };
    let make = |instance: &str, p: &Project, c: &str| {
        candidate(instance, p, 4, "Task".into(), None, "me", c).unwrap()
    };
    let before = make("https://gitlab.com", &original, "review");
    let after = make("https://gitlab.com", &renamed, "follow");
    assert_eq!(before.identity, after.identity);
    assert_ne!(before.source_url, after.source_url);
    assert_ne!(
        before.identity,
        make("https://gitlab.example", &original, "review").identity
    );
    assert_ne!(
        before.identity,
        make("https://gitlab.com", &original, "issues").identity
    );
    assert!(after.description.contains("renamed/project"));
    assert!(after.description.contains(&after.source_url));
}

#[test]
fn rest_pagination_is_bounded_and_failure_never_returns_partial_success() {
    for mode in ["complete", "incomplete", "failure", "malformed", "repeated"] {
        let mut calls = 0;
        let result = fetch(
            "https://gitlab.com|group/repo|issues",
            &HashMap::new(),
            |endpoint, _| {
                if endpoint == "/api/v4/user" {
                    return Ok(response(user(1, "me")));
                }
                if endpoint == "/api/v4/projects/group%2Frepo" {
                    return Ok(response(
                        json!({"id":123,"path_with_namespace":"group/repo"}),
                    ));
                }
                calls += 1;
                if mode == "failure" && calls == 2 {
                    return Err(ProviderError);
                }
                if mode == "malformed" {
                    return Ok(Response {
                        body: b"not json".to_vec(),
                        next: None,
                    });
                }
                let mut r = response(json!((0..100)
                    .map(|i| item(
                        if mode == "repeated" {
                            i + 1
                        } else {
                            (calls - 1) * 100 + i + 1
                        },
                        "other"
                    ))
                    .collect::<Vec<_>>()));
                r.next = Some(if mode == "complete" && calls == 2 {
                    String::new()
                } else {
                    (calls + 1).to_string()
                });
                Ok(r)
            },
        );
        if mode == "complete" {
            assert_eq!(result.unwrap().len(), 200);
        } else {
            assert!(result.is_err(), "{mode}");
        }
        assert!(calls <= 3);
    }
}

#[test]
fn graph_errors_null_missing_state_or_cursor_and_partial_pages_fail() {
    for mode in ["errors", "null", "cursor", "incomplete"] {
        let mut calls = 0;
        let result = fetch(
            "https://gitlab.com|group/repo|review",
            &HashMap::new(),
            |endpoint, _| {
                if endpoint == "/api/v4/user" {
                    return Ok(response(user(1, "me")));
                }
                if endpoint.starts_with("/api/v4/projects/") {
                    return Ok(response(
                        json!({"id":123,"path_with_namespace":"group/repo"}),
                    ));
                }
                calls += 1;
                Ok(response(match mode {
                    "errors" => {
                        json!({"data":null,"errors":[{"message":"secret upstream diagnostics"}]})
                    }
                    "null" => json!({"data":{"project":null}}),
                    "cursor" => graph(vec![], true, None),
                    _ => graph(vec![], true, Some(&format!("cursor-{calls}"))),
                }))
            },
        );
        assert!(result.is_err(), "{mode}");
        assert!(calls <= 3);
    }
}

#[test]
fn explicit_reviewer_and_complete_graph_pages_use_one_deadline() {
    let mut graph_calls = 0;
    let mut first_deadline = None;
    let result = fetch(
        "https://gitlab.com|group/repo|review",
        &HashMap::from([("user".into(), "other".into())]),
        |endpoint, deadline| {
            assert_eq!(*first_deadline.get_or_insert(deadline), deadline);
            Ok(if endpoint == "/api/v4/user" {
                response(user(1, "me"))
            } else if endpoint.starts_with("/api/v4/users?") {
                assert!(endpoint.contains("username=other"));
                response(json!([user(2, "other")]))
            } else if endpoint == "/api/v4/projects/group%2Frepo" {
                response(json!({"id":123,"path_with_namespace":"group/repo"}))
            } else {
                graph_calls += 1;
                assert!(endpoint.contains("reviewerUsername%3A%22other%22"));
                assert!(!endpoint.contains("assignee"));
                if graph_calls == 2 {
                    assert!(endpoint.contains("after%3A%22next-cursor%22"));
                }
                let mut item = mr(&graph_calls.to_string(), "UNREVIEWED", 3);
                item["reviewers"]["nodes"][0]["username"] = json!("other");
                response(graph(vec![item], graph_calls == 1, Some("next-cursor")))
            })
        },
    )
    .unwrap();
    assert_eq!(graph_calls, 2);
    assert_eq!(result.len(), 2);
}

#[test]
fn rest_queries_send_exact_assignee_author_and_all_filters() {
    for (category, filters, expected) in [
        ("issues", HashMap::new(), Some("&assignee_id=1")),
        (
            "issues",
            HashMap::from([("user".into(), "other".into())]),
            Some("&assignee_id=2"),
        ),
        ("issues", HashMap::from([("user".into(), "".into())]), None),
        ("follow", HashMap::new(), Some("&author_id=1")),
    ] {
        fetch(
            &format!("https://gitlab.com|group/repo|{category}"),
            &filters,
            |endpoint, _| {
                Ok(if endpoint == "/api/v4/user" {
                    response(user(1, "me"))
                } else if endpoint.starts_with("/api/v4/users?") {
                    response(json!([user(2, "other")]))
                } else if endpoint == "/api/v4/projects/group%2Frepo" {
                    response(json!({"id":123,"path_with_namespace":"group/repo"}))
                } else {
                    assert!(endpoint.contains("state=opened&scope=all&per_page=100&page=1"));
                    if let Some(expected) = expected {
                        assert!(endpoint.ends_with(expected));
                    } else {
                        assert!(!endpoint.contains("assignee"));
                    }
                    response(json!([]))
                })
            },
        )
        .unwrap();
    }
}

#[test]
fn oversized_titles_and_bodies_are_invalid_not_silently_truncated() {
    let mut big = item(1, "other");
    big["description"] = json!("x".repeat(70_000));
    let mut title = item(2, "other");
    title["title"] = json!("x".repeat(201));
    let result = preview("issues", HashMap::new(), vec![big, title, item(3, "other")]).unwrap();
    assert!(result[0]
        .invalid_reason
        .as_ref()
        .unwrap()
        .contains("exceeds"));
    assert!(result[1]
        .invalid_reason
        .as_ref()
        .unwrap()
        .contains("200-character"));
    assert!(result[2].invalid_reason.is_none());
}

#[test]
fn credentials_are_operator_file_only_bounded_and_host_scoped() {
    let temp = std::env::temp_dir().join(format!(
        "kanso-gitlab-credentials-{}-{}",
        std::process::id(),
        crate::ids::uuid_v4().unwrap()
    ));
    std::fs::write(
        &temp,
        r#"{"https://gitlab.com":"fixture-token","https://gitlab.example":"second-fixture"}"#,
    )
    .unwrap();
    assert_eq!(
        token_from_file(&temp, "https://gitlab.com").unwrap(),
        "fixture-token"
    );
    assert!(token_from_file(&temp, "https://unknown.example").is_err());
    std::fs::write(&temp, r#"{"https://secret@gitlab.com":"fixture-token"}"#).unwrap();
    assert!(token_from_file(&temp, "https://gitlab.com").is_err());
    std::fs::write(&temp, r#"{"https://gitlab.com":"fixture\ntoken"}"#).unwrap();
    assert!(token_from_file(&temp, "https://gitlab.com").is_err());
    std::fs::write(&temp, vec![b' '; MAX_CREDENTIALS as usize + 1]).unwrap();
    assert!(token_from_file(&temp, "https://gitlab.com").is_err());
    std::fs::remove_file(&temp).unwrap();
    assert!(token_from_file(&temp, "https://gitlab.com").is_err());
    assert!(GitLab
        .failure_message()
        .contains("KANSO_GITLAB_TOKENS_FILE"));
}

#[test]
fn auth_transport_errors_invalid_filters_and_remote_project_mismatch_fail_closed() {
    let mut calls = 0;
    assert!(fetch(
        "https://gitlab.com|group/repo|issues",
        &HashMap::new(),
        |_, _| {
            calls += 1;
            Err(ProviderError)
        }
    )
    .is_err());
    assert_eq!(calls, 1);
    assert!(fetch(
        "https://gitlab.com|group/repo|issues",
        &HashMap::from([("unknown".into(), "x".into())]),
        |_, _| panic!("invalid filters must not perform IO")
    )
    .is_err());
    assert!(fetch(
        "https://gitlab.com|group/repo|issues",
        &HashMap::from([("user".into(), "token=secret".into())]),
        |_, _| Ok(response(user(1, "me")))
    )
    .is_err());
    let mut mismatch = item(1, "other");
    mismatch["project_id"] = json!(999);
    assert!(preview("issues", HashMap::new(), vec![mismatch]).is_err());
}

#[test]
fn http_transport_rejects_auth_failures_redirects_oversize_and_credential_reflection() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    assert!(http_agent(TIMEOUT, true).config().https_only());
    assert_eq!(http_agent(TIMEOUT, true).config().max_redirects(), 0);
    for (status, body, extra, success) in [
        (401, "{\"error\":\"fixture-secret\"}".to_owned(), "", false),
        (403, "{\"error\":\"fixture-secret\"}".to_owned(), "", false),
        (404, "{\"error\":\"access denied\"}".to_owned(), "", false),
        (
            302,
            "{}".to_owned(),
            "Location: http://127.0.0.1:1/leak\r\n",
            false,
        ),
        (200, "[\"fixture-secret\"]".to_owned(), "", false),
        (200, r#"["fixture-\u0073ecret"]"#.to_owned(), "", false),
        (200, "x".repeat(MAX_OUTPUT as usize + 1), "", false),
        (200, "[]".to_owned(), "X-Next-Page: \r\n", true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = vec![];
            let mut buffer = [0; 1024];
            while !bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                let len = socket.read(&mut buffer).unwrap();
                assert!(len > 0);
                bytes.extend_from_slice(&buffer[..len]);
            }
            let headers=format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",body.len());
            let _ = socket.write_all(headers.as_bytes());
            let _ = socket.write_all(body.as_bytes());
            String::from_utf8(bytes).unwrap()
        });
        let result = request_with_agent(
            &http_agent(Duration::from_secs(5), false),
            &format!("http://{address}"),
            "fixture-secret",
            "/api/v4/user",
        );
        assert_eq!(result.is_ok(), success, "HTTP fixture status {status}");
        let sent = server.join().unwrap();
        assert!(sent.starts_with("GET /api/v4/user HTTP/1.1"));
        assert!(sent
            .to_ascii_lowercase()
            .contains("private-token: fixture-secret"));
    }
}
