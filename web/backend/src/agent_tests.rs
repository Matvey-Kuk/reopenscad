use super::*;

/// A check cancelled partway stops within a fraction of its full cost: the
/// browser's cancel reaches it through the client watch's flag.
#[test]
fn a_cancelled_intersection_check_stops_promptly() {
    // Top-level objects each; neighbours in a row overlap.
    let code: String = (0..12).map(|i| format!("translate([{}, {}, 0]) sphere(r = 5, $fn = 16);\n", i % 4 * 7, i / 4 * 7)).collect();
    let tolerance = 0.2;
    let contact = ContactPolicy::from_input(&json!({})).unwrap();
    let (state, root) = state();
    let id = state.workspaces.create(code, None).unwrap().id;
    let source = eval_source(&state, Some(&id), &json!({})).unwrap();
    let compiled = compile_eval(&source).unwrap();
    let far = Instant::now() + Duration::from_secs(600);
    let run = || {
        let targets = intersection_targets(&compiled, &json!({}), &contact).unwrap();
        let started = Instant::now();
        let pairs = check_pairs(&targets, tolerance, &contact, &source, far).unwrap();
        (started.elapsed(), pairs)
    };
    let (full, pairs) = run();
    assert_eq!(pairs.len(), 66);
    assert!(pairs.iter().all(|pair| pair["skipped"] != json!(true)), "{}", pairs[0]);

    // Set before it starts: nothing is measured at all.
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let scope = crate::csg::bsp::cancellation_scope(std::sync::Arc::clone(&flag));
    let (at_once, skipped) = run();
    assert_eq!(skipped.len(), 66);
    assert!(skipped.iter().all(|pair| pair.to_string().contains("cancelled")), "{}", skipped[0]);
    assert!(at_once < full / 20, "{at_once:?} of {full:?}");
    drop(scope);

    // Set 10% of the way in, as the client watch would.
    flag.store(false, std::sync::atomic::Ordering::Relaxed);
    let _scope = crate::csg::bsp::cancellation_scope(std::sync::Arc::clone(&flag));
    let setter = std::thread::spawn({
        let flag = std::sync::Arc::clone(&flag);
        move || {
            std::thread::sleep(full / 10);
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    let (late, _) = run();
    setter.join().unwrap();
    println!("intersection check, 12 spheres: full {full:?}, flag set first {at_once:?}, cancelled at 10% {late:?}");
    assert!(late < full / 3, "{late:?} of {full:?}");
    let _ = std::fs::remove_dir_all(root);
}

fn state() -> (AppState, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = env::temp_dir().join(format!(
        "reopenscad-agent-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let store = WorkspaceStore::open(root.clone()).unwrap();
    (
        AppState {
            web_root: PathBuf::new(),
            render_queue: RenderQueue::new(
                MAX_CONCURRENT_JOBS,
                MAX_QUEUED_JOBS,
                MAX_QUEUED_PER_CLIENT,
                MAX_QUEUE_WAIT,
            ),
            poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
            rate_limiter: RateLimiter::new(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            workspaces: Store::Filesystem(store),
            public_origin: None,
        },
        root,
    )
}

/// One request through the real pipeline: status and body bytes.
fn http(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (u16, Vec<u8>) {
    http_with(state, method, path, body, "")
}

/// `http` with extra raw header lines (each ending in `\r\n`).
fn http_with(
    state: &AppState,
    method: &str,
    path: &str,
    body: Option<Value>,
    extra_headers: &str,
) -> (u16, Vec<u8>) {
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    let raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\n{extra_headers}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        response
    });
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    serve_request(&mut stream, state, &mut None).unwrap();
    drop(stream);
    let response = client.join().unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8_lossy(&response[..split]).to_string();
    let status = head[9..12].parse().unwrap();
    (status, response[split + 4..].to_vec())
}

fn http_json(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let (status, body) = http(state, method, path, body);
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn mcp_request() -> Request {
    Request {
        method: "POST".into(),
        path: "/mcp".into(),
        query: String::new(),
        headers: vec![("host".into(), "127.0.0.1:5173".into())],
        body: Vec::new(),
    }
}

fn mcp(state: &AppState, tool: &str, arguments: Value) -> Value {
    crate::mcp_call_tool(state, &mcp_request(), tool, arguments)
}

fn create(state: &AppState, code: &str) -> (String, u64) {
    let (status, body) = http_json(
        state,
        "POST",
        "/api/workspaces",
        Some(json!({"code": code})),
    );
    assert_eq!(status, 201, "{body}");
    (
        body["workspace"]["id"].as_str().unwrap().to_string(),
        body["workspace"]["revision"].as_u64().unwrap(),
    )
}

fn close(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}

const TWO_BOXES: &str =
    "gap = 0.35;\nsize = 2;\ncube(size);\ntranslate([size + gap, 0, 0]) cube(3);\n";

#[test]
fn objects_and_measure_read_the_saved_revision() {
    let (state, root) = state();
    let (status, body) = http_json(&state, "POST", &format!("/api/designs/{}/objects", crate::scancommon::testkit::save_revision(&state, None, Some(TWO_BOXES), Some(&json!({"size": 4})))), Some(json!({})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], json!(2));
    assert_eq!(body["objects"].as_array().unwrap().len(), 2);
    assert_eq!(body["objects"][0]["bounds"]["max"], json!([4.0, 4.0, 4.0]));
    assert_eq!(body["objects"][0]["source"]["startLine"], json!(3));

    let result = mcp(
        &state,
        "measure",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(TWO_BOXES), None),
            "pairs": [["object-1", "object-2"]],
        }));
    assert_eq!(result["isError"], json!(false), "{result}");
    let measured = &result["structuredContent"];
    assert!(close(
        measured["objects"][0]["volume"].as_f64().unwrap(),
        8.0,
        1e-9
    ));
    assert!(close(
        measured["objects"][1]["volume"].as_f64().unwrap(),
        27.0,
        1e-9
    ));
    assert_eq!(measured["objects"][0]["watertight"], json!(true));
    assert_eq!(measured["objects"][0]["windingConsistent"], json!(true));
    assert_eq!(measured["objects"][0]["components"], json!(1));
    assert!(close(
        measured["total"]["volume"].as_f64().unwrap(),
        35.0,
        1e-9
    ));
    let distance = &measured["distances"][0];
    assert!(
        close(distance["clearance"].as_f64().unwrap(), 0.35, 1e-9),
        "{distance}"
    );
    assert_eq!(distance["intersects"], json!(false));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn clearance_is_exact_where_the_sampled_check_underestimated() {
    // A non-uniformly scaled sphere: its SDF is only a lower bound, and
    // the old sampled check reported 0.32 mm for a 0.474 mm gap.
    let code = "scale([3,1,1]) sphere(5,$fn=64); translate([12,3.5,-1]) cube(4);";
    let (state, root) = state();
    let result = mcp(
        &state,
        "check_intersections",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "tolerance": 0.4,
            "verbose": true,
        }));
    let pair = &result["structuredContent"]["intersections"][0];
    let clearance = pair["clearance"].as_f64().unwrap();
    // Ground truth: brute force over every triangle pair.
    let output = engine::compile_parts(code, engine::Quality::Preview, None).unwrap();
    let (a, b) = (
        analysis::mesh_triangles(&output.parts[0].mesh),
        analysis::mesh_triangles(&output.parts[1].mesh),
    );
    let brute = a
        .iter()
        .flat_map(|ta| {
            b.iter()
                .map(move |tb| analysis::triangle_distance(ta, tb).0)
        })
        .fold(f64::INFINITY, f64::min);
    assert!(close(clearance, brute, 1e-12), "{clearance} vs {brute}");
    assert!(clearance > 0.45, "{clearance}");
    assert_eq!(pair["withinTolerance"], json!(false));
    assert!(pair["closestPoints"]["a"].is_array());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn intersections_report_overlap_volume_and_bounds() {
    let (state, root) = state();
    let (id, revision) = create(
        &state,
        "cube(10);\nshift = 20;\ntranslate([shift, 2, 2]) cube(8);\n",
    );
    crate::scancommon::testkit::save_revision(&state, Some(&id), None, Some(&json!({"shift": 6})));
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/intersections"),
        Some(json!({"tolerance": 0.2})),
    );
    assert_eq!(status, 200, "{body}");
    let pair = &body["intersections"][0];
    assert_eq!(pair["intersects"], json!(true));
    // Overlap is [6,10] x [2,10] x [2,10].
    assert!(
        close(
            pair["overlapVolume"].as_f64().unwrap(),
            4.0 * 8.0 * 8.0,
            1e-6
        ),
        "{pair}"
    );
    assert_eq!(pair["overlapBoundsExact"], json!(true));
    let bounds = &pair["overlapBounds"];
    assert!(close(bounds["min"][0].as_f64().unwrap(), 6.0, 1e-9));
    assert!(close(bounds["max"][0].as_f64().unwrap(), 10.0, 1e-9));
    assert_eq!(pair["aSource"]["startLine"], json!(1));
    assert_eq!(pair["bSource"]["startLine"], json!(3));
    assert_eq!(body["revision"], json!(revision + 1));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn render_image_over_http_and_mcp() {
    let (state, root) = state();
    let (id, _) = create(
        &state,
        "color(\"red\") cube(10);\ntranslate([15, 0, 0]) sphere(5);\n",
    );
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/image"),
        Some(
            json!({"view": "front", "width": 200, "height": 150, "section": {"axis": "y", "offset": 5}}),
        ),
    );
    assert_eq!(status, 200, "{body}");
    let png = crate::base64_decode(body["image"]["base64"].as_str().unwrap()).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(body["render"]["width"], json!(200));
    assert_eq!(body["objects"][0]["color"], json!("#ff0000"));

    let (status, raw) = http(
        &state,
        "POST",
        &format!("/api/designs/{id}/image?raw=1"),
        Some(json!({"width": 64, "height": 64})),
    );
    assert_eq!(status, 200);
    assert_eq!(&raw[..4], b"\x89PNG");

    let result = mcp(
        &state,
        "render_image",
        json!({"workspaceId": id, "hiddenObjectIds": ["object-2"], "colors": {"object-1": "#00ff00"}, "width": 128, "height": 96}),
    );
    assert_eq!(result["isError"], json!(false), "{result}");
    assert_eq!(result["content"][0]["type"], json!("image"));
    assert_eq!(result["content"][0]["mimeType"], json!("image/png"));
    assert_eq!(
        result["structuredContent"]["objects"][1]["visible"],
        json!(false)
    );
    assert_eq!(
        result["structuredContent"]["objects"][0]["color"],
        json!("#00ff00")
    );

    let bad = mcp(
        &state,
        "render_image",
        json!({"workspaceId": id, "objectIds": ["nope"]}),
    );
    assert_eq!(bad["isError"], json!(true));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn diff_between_stored_revisions() {
    let (state, root) = state();
    let (id, revision) = create(&state, "cube(10);\ntranslate([20, 0, 0]) cube(5);\n");
    let result = mcp(
        &state,
        "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision,
            "edits": [{"search": "cube(10)", "replace": "cube([10, 10, 12])"}],
            "revisionName": "Test revision",
        }));
    assert_eq!(result["isError"], json!(false), "{result}");
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/diff"),
        Some(json!({})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["from"]["revision"], json!(1));
    assert_eq!(body["to"]["revision"], json!(2));
    assert_eq!(body["summary"]["changed"], json!(1));
    assert_eq!(body["summary"]["unchanged"], json!(1));
    let changed = &body["objects"][0];
    assert_eq!(changed["status"], json!("changed"));
    assert!(close(changed["volumeDelta"].as_f64().unwrap(), 200.0, 1e-6));
    assert!(
        close(changed["addedVolume"].as_f64().unwrap(), 200.0, 1e-6),
        "{changed}"
    );
    assert!(close(changed["removedVolume"].as_f64().unwrap(), 0.0, 1e-6));
    let region = &body["summary"]["changedBounds"];
    assert!(
        close(region["min"][2].as_f64().unwrap(), 10.0, 1e-9),
        "{region}"
    );
    assert!(close(region["max"][2].as_f64().unwrap(), 12.0, 1e-9));
    assert_eq!(body["sourceChange"]["fromLines"], json!([1, 1]));

    // A third revision that drops an object, against the second.
    let result = mcp(
        &state,
        "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision + 1,
            "code": "cube([10, 10, 12]);\n",
            "revisionName": "Test revision",
        }));
    assert_eq!(result["isError"], json!(false), "{result}");
    let result = mcp(&state, "diff", json!({"workspaceId": id}));
    let diff = &result["structuredContent"];
    assert_eq!(diff["from"]["revision"], json!(2));
    assert_eq!(diff["to"]["revision"], json!(3));
    assert_eq!(diff["summary"]["removed"], json!(1));

    let same = mcp(&state, "diff", json!({"designId": crate::scancommon::testkit::design_with_revisions(&state, "cube(1);", "cube(1);")}));
    assert_eq!(same["structuredContent"]["identical"], json!(true), "{same}");

    let (status, body) = http_json(
        &state,
        "GET",
        &format!("/api/workspaces/{id}/revisions"),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(body["current"], json!(3));
    assert_eq!(body["revisions"][2]["revision"], json!(1));

    // An older revision reads as text; restoring it is what makes it measurable.
    let old = mcp(&state, "get_design_code", json!({"workspaceId": id, "revision": 1}));
    assert!(old["structuredContent"]["code"].as_str().unwrap().contains("cube(10)"), "{old}");
    let restored = mcp(&state, "restore_revision", json!({"designId": id, "revision": 1, "expectedRevision": 3, "revisionName": "Test revision"}));
    assert_eq!(restored["isError"], json!(false), "{restored}");
    let result = mcp(&state, "measure", json!({"workspaceId": id}));
    assert!(close(
        result["structuredContent"]["objects"][0]["volume"]
            .as_f64()
            .unwrap(),
        1000.0,
        1e-6
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn parameters_list_set_and_validate() {
    let (state, root) = state();
    let code = "open_angle = 90; // [0:360]\npart = \"all\";\ncube(10);\n";
    let (id, revision) = create(&state, code);
    let (status, body) = http_json(
        &state,
        "GET",
        &format!("/api/workspaces/{id}/parameters"),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(body["parameters"][0]["name"], json!("open_angle"));
    assert_eq!(body["parameters"][0]["value"], json!(90.0));
    assert_eq!(body["parameters"][1]["line"], json!(2));

    let result = mcp(
        &state,
        "set_parameters",
        json!({
            "workspaceId": id,
            "expectedRevision": revision,
            "values": {"open_angle": 35, "part": "inner"},
            "revisionName": "Test revision",
        }));
    assert_eq!(result["isError"], json!(false), "{result}");
    let workspace = state.workspaces.get(&id).unwrap().unwrap();
    assert_eq!(workspace.revision, revision + 1);
    assert_eq!(
        workspace.code,
        "open_angle = 35; // [0:360]\npart = \"inner\";\ncube(10);\n"
    );
    // One minimal edit, so the browser highlights only the change.
    let event = workspace.events.last().unwrap();
    assert_eq!(event.source, "ai");
    assert_eq!(event.start, 13);

    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/parameters"),
        Some(json!({"expectedRevision": revision + 1, "values": {"missing": 1}})),
    );
    assert_eq!(status, 400);
    assert!(
        body["error"].as_str().unwrap().contains("open_angle, part"),
        "{body}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn patch_updates_by_lines_and_unique_search() {
    let (state, root) = state();
    let (id, revision) = create(&state, "cube(1);\ncube(1);\nsphere(2);\n");
    let ambiguous = mcp(
        &state,
        "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision,
            "edits": [{"search": "cube(1)", "replace": "cube(3)"}],
            "revisionName": "Test revision",
        }));
    assert_eq!(ambiguous["isError"], json!(true));
    assert!(ambiguous["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("lines 1, 2"));

    let (status, body) = http_json(
        &state,
        "PATCH",
        &format!("/api/workspaces/{id}"),
        Some(
            json!({"expectedRevision": revision, "startLine": 3, "endLine": 3, "text": "sphere(4);"}),
        ),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["workspace"]["code"],
        json!("cube(1);\ncube(1);\nsphere(4);\n")
    );
    assert_eq!(body["workspace"]["updatedRanges"][0]["start"], json!(25));

    let stale = mcp(
        &state,
        "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision,
            "startLine": 1,
            "text": "x",
            "revisionName": "Test revision",
        }));
    assert!(stale["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Revision conflict"));
    let invalid = mcp(
        &state,
        "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision + 1,
            "edits": [{"search": "sphere(4)", "replace": "sphere("}],
            "revisionName": "Test revision",
        }));
    assert_eq!(invalid["structuredContent"]["valid"], json!(false));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_reads_list_cached_objects_and_share_links_mint() {
    let (state, root) = state();
    let (id, _) = create(&state, "cube(1);\ntranslate([3,0,0]) cube(1);\n");
    // Opting out, or a browser fetch (which renders the workspace itself),
    // leaves an uncached revision uncompiled, with a hint.
    let (_, body) = http_json(&state, "GET", &format!("/api/workspaces/{id}?objects=none"), None);
    assert_eq!(body["workspace"]["objectsStatus"], json!("not_compiled"));
    assert!(body["workspace"]["objectsHint"].as_str().unwrap().contains("objects=compute"));
    let (_, browser) = http_with(
        &state,
        "GET",
        &format!("/api/workspaces/{id}"),
        None,
        "Sec-Fetch-Mode: cors\r\n",
    );
    let browser: Value = serde_json::from_slice(&browser).unwrap();
    assert_eq!(browser["workspace"]["objectsStatus"], json!("not_compiled"));
    let (_, body) = http_json(
        &state,
        "GET",
        &format!("/api/workspaces/{id}?objects=compute"),
        None,
    );
    assert_eq!(body["workspace"]["objectsStatus"], json!("current"));
    assert_eq!(body["workspace"]["objects"].as_array().unwrap().len(), 2);
    let (_, body) = http_json(&state, "GET", &format!("/api/workspaces/{id}"), None);
    assert_eq!(body["workspace"]["objects"].as_array().unwrap().len(), 2);

    // Agent gap 12: a plain read on an idle server compiles a cache miss.
    let (fresh, _) = create(&state, "cube(1);\ntranslate([3,0,0]) cube(1);\ntranslate([6,0,0]) cube(1);\n");
    let (_, body) = http_json(&state, "GET", &format!("/api/workspaces/{fresh}"), None);
    assert_eq!(body["workspace"]["objectsStatus"], json!("current"));
    assert_eq!(body["workspace"]["objects"].as_array().unwrap().len(), 3);
    // ...and MCP get_workspace_code carries the objects too.
    let code = mcp(&state, "get_workspace_code", json!({"workspaceId": fresh}));
    assert_eq!(code["structuredContent"]["objects"].as_array().unwrap().len(), 3, "{code}");
    // A busy server never queues work for a plain read: it answers at once.
    let (busy, _) = create(&state, "sphere(2);\n");
    let held: Vec<_> = (0..MAX_CONCURRENT_JOBS)
        .map(|_| state.render_queue.acquire(current_client_key(), None, None).unwrap())
        .collect();
    let (status, body) = http_json(&state, "GET", &format!("/api/workspaces/{busy}"), None);
    assert_eq!(status, 200);
    assert_eq!(body["workspace"]["objectsStatus"], json!("not_compiled"));
    let code = mcp(&state, "get_workspace_code", json!({"workspaceId": busy}));
    assert_eq!(code["structuredContent"]["objectsStatus"], json!("not_compiled"), "{code}");
    drop(held);

    let shared = mcp(&state, "create_share_link", json!({"workspaceId": id}));
    let url = shared["structuredContent"]["url"].as_str().unwrap();
    let token = shared["structuredContent"]["shareToken"].as_str().unwrap();
    assert!(url.ends_with(&format!("/shared/{token}")));
    assert!(!url.contains(&id));
    let (status, body) =
        http_json(&state, "POST", &format!("/api/workspaces/{id}/share"), None);
    assert_eq!(status, 200);
    assert_eq!(body["url"], json!(url));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn new_routes_are_metered_by_cost() {
    let id = "fuzzy-wombat-00112233445566778899aabbccdd";
    for action in BROWSER_CODE_ACTIONS {
        assert_eq!(
            route_cost("POST", &format!("/api/{action}")),
            RouteCost::Evaluator
        );
    }
    for action in ["objects", "measure", "image", "diff", "intersections"] {
        assert_eq!(
            route_cost("POST", &format!("/api/workspaces/{id}/{action}")),
            RouteCost::Evaluator
        );
    }
    assert_eq!(
        route_cost("GET", &format!("/api/workspaces/{id}/parameters")),
        RouteCost::Cheap
    );
    assert_eq!(
        route_cost("GET", &format!("/api/workspaces/{id}/revisions")),
        RouteCost::Cheap
    );
    assert_eq!(
        route_cost("POST", &format!("/api/workspaces/{id}/parameters")),
        RouteCost::Evaluator
    );
    for tool in [
        "render_image",
        "measure",
        "diff",
        "check_intersections",
        "set_parameters",
    ] {
        assert!(mcp_tool_runs_evaluator(tool), "{tool}");
    }
    assert!(!mcp_tool_runs_evaluator("get_parameters"));
    assert!(!mcp_tool_runs_evaluator("create_share_link"));
}

#[test]
fn revision_history_is_bounded_and_evicted_with_the_workspace() {
    let (state, root) = state();
    let (id, mut revision) = create(&state, "cube(1);\n");
    for size in 2..=(MAX_REVISION_HISTORY as u64 + 5) {
        let result = mcp(
            &state,
            "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision,
            "code": format!("cube({size});\n"),
            "revisionName": "Test revision",
        }));
        assert_eq!(result["isError"], json!(false), "{result}");
        revision += 1;
    }
    let retained = state.workspaces.revision_list(&id).unwrap();
    assert_eq!(retained.len(), MAX_REVISION_HISTORY);
    assert_eq!(retained.last().unwrap().revision, revision - 1);
    assert_eq!(
        state
            .workspaces
            .revision_code(&id, revision - 1)
            .unwrap()
            .as_deref(),
        Some(format!("cube({});\n", revision - 1).as_str())
    );
    assert!(state.workspaces.revision_code(&id, 1).unwrap().is_none());
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/diff"),
        Some(json!({"fromRevision": 1})),
    );
    assert_eq!(status, 404);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("not retained"));
    #[allow(irrefutable_let_patterns)] // refutable with the postgres feature
    if let Store::Filesystem(store) = &state.workspaces {
        assert!(store.history_path(&id).exists());
        let mut inner = store.inner.lock().unwrap();
        store.forget(&mut inner, &id);
        assert!(!store.history_path(&id).exists());
    }
    let _ = fs::remove_dir_all(root);
}

/// The usage-report case: a hinge knuckle with a 45-degree end chamfer
/// and a steeper (30-degree half-angle) cone socket, leaving a 75-degree
/// lip whose top is only 0.3 mm wide. No face normal near the lip crosses
/// it (the centroid-ray scan this replaced reported 3.8 mm and no thin
/// spot). The largest ball that fits under the lip's top is the excircle
/// of the 0.3 mm / 45 / 60 degree triangle the flanks make above it:
/// 0.302 mm radius, so 0.60 mm across.
#[test]
fn thickness_finds_the_ligament_between_a_chamfer_and_a_socket() {
    let (state, root) = state();
    let code = "rotate_extrude($fn = 160) polygon([[0, 0], [5, 0], [5, 8], [3.2, 9.8], [2.9, 9.8], [0, 4.777]]);\n";
    let (status, body) = http_json(&state, "POST", &format!("/api/designs/{}/measure", crate::scancommon::testkit::save_revision(&state, None, Some(code), None)), Some(json!({"checks": ["thickness"], "thicknessAt": {"point": [0, 3.05, 9.8], "radius": 1.0}})));
    assert_eq!(status, 200, "{body}");
    let checks = &body["objects"][0]["checks"];
    let min = checks["thickness"]["min"].as_f64().unwrap();
    assert!((min - 0.604).abs() < 0.06, "{min} {}", checks["thickness"]["minLocation"]);
    assert!(checks["thickness"]["belowThreshold"].as_u64().unwrap() > 0, "{checks}");
    let probe = checks["thicknessAt"]["min"].as_f64().unwrap();
    assert!((probe - 0.604).abs() < 0.06, "{probe} {}", checks["thicknessAt"]["minLocation"]);
    let _ = fs::remove_dir_all(root);
}

/// The usage-report gaps: computed values and assert margins in one
/// evaluate call, instead of re-implementing the SCAD in Python.
#[test]
fn evaluate_echoes_values_and_reports_every_assert_margin() {
    let (state, root) = state();
    let code = "w = 1.0;\nr = w * 2;\nassert(r < 2.5, \"r\");\nassert(w > 1.5);\nmodule m(x) { assert(x >= 0.5) cube(x); }\nm(1);\ntranslate([5, 0, 0]) m(0.7);\n";
    let result = mcp(
        &state,
        "evaluate",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "echo": ["r", "w * 3", "nope"],
            "asserts": "all",
            "intersections": false,
        }));
    let structured = &result["structuredContent"];
    assert_eq!(result["isError"], json!(false), "{result}");
    assert_eq!(structured["echo"][0]["value"], json!(2.0), "{structured}");
    assert_eq!(structured["echo"][1]["value"], json!(3.0));
    assert_eq!(structured["echo"][2]["value"], Value::Null);
    let asserts = &structured["asserts"];
    assert_eq!((asserts["total"].as_u64(), asserts["failed"].as_u64()), (Some(3), Some(1)), "{asserts}");
    let rows = asserts["asserts"].as_array().unwrap();
    assert_eq!(rows[0]["line"], json!(3));
    assert!(close(rows[0]["margin"].as_f64().unwrap(), 0.5, 1e-9), "{asserts}");
    assert_eq!(rows[1]["ok"], json!(false));
    assert!(close(rows[1]["margin"].as_f64().unwrap(), -0.5, 1e-9));
    assert_eq!(rows[1]["comparisons"][0]["lhsValue"], json!(1.0));
    // Inside a module called twice: the tighter call is reported.
    assert_eq!(rows[2]["evaluations"], json!(2));
    assert!(close(rows[2]["margin"].as_f64().unwrap(), 0.2, 1e-9), "{asserts}");
    assert!(asserts["note"].is_string());
    // The failing assert did not stop the model: both cubes are there.
    assert_eq!(structured["objects"].as_array().unwrap().len(), 2, "{structured}");
    // Probe lines stay out of the console.
    assert!(!structured["console"].to_string().contains(scadsource::PROBE_MARKER));
    // Object source lines still point into the original code.
    assert_eq!(structured["objects"][0]["source"]["startLine"], json!(6), "{structured}");

    // An override that changes the line count above an assert keeps
    // the reported line in original-source terms.
    // Truthy non-boolean conditions pass, as in the engine.
    let truthy = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("assert(1);\nassert(\"x\");\ncube(1);\n"), None), "asserts": "all", "intersections": false}));
    assert_eq!(truthy["structuredContent"]["asserts"]["failed"], json!(0), "{truthy}");
    // asserts: true lists only failing ones and the tightest passing.
    let compact = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("a = 1;\nassert(a < 2);\nassert(a < 9);\ncube(1);\n"), None), "asserts": true, "intersections": false}));
    let rows = compact["structuredContent"]["asserts"]["asserts"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert!(close(rows[0]["margin"].as_f64().unwrap(), 1.0, 1e-9));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn list_modules_and_code_grep_find_a_feature_without_reading_the_file() {
    let (state, root) = state();
    let outline = format!(
        "// ---- Params ----\nbig = [{}];\n// ---- Hinge ----\n// Hinge pin with a cone tip.\nmodule pin(d = 3) {{\n  cylinder(d = d, h = 5);\n}}\npin();\n",
        vec!["1"; 3000].join(", ")
    );
    let (id, _) = create(&state, &outline);
    let modules = mcp(&state, "list_modules", json!({"designId": id, "grep": "cone"}));
    let structured = &modules["structuredContent"];
    assert_eq!(structured["definitions"][0]["name"], json!("pin"), "{structured}");
    assert_eq!(structured["definitions"][0]["lines"], json!([5, 7]));
    assert_eq!(structured["sections"][1]["title"], json!("Hinge"));

    // A whole-file read elides the 9000-char data line.
    let read = mcp(&state, "get_design_code", json!({"designId": id}));
    let read = &read["structuredContent"];
    assert_eq!(read["elidedLines"][0]["line"], json!(2), "{read}");
    assert_eq!(read["exact"], json!(false));
    assert!(read["code"].as_str().unwrap().len() < 1000);
    // ...but asking for that one line returns it whole.
    let line = mcp(&state, "get_design_code", json!({"designId": id, "startLine": 2, "endLine": 2}));
    assert!(line["structuredContent"]["code"].as_str().unwrap().len() > 9000);
    // grep returns just the matching window.
    let grep = mcp(&state, "get_design_code", json!({"designId": id, "grep": "cylinder", "around": 1}));
    let grep = &grep["structuredContent"];
    assert_eq!(grep["matchLines"], json!([6]), "{grep}");
    assert_eq!(grep["matches"][0]["startLine"], json!(5));
    assert!(grep["matches"][0]["code"].as_str().unwrap().contains("module pin"));

    // get_parameters summarises the long list.
    let parameters = mcp(&state, "get_parameters", json!({"designId": id}));
    let parameters = &parameters["structuredContent"];
    assert_eq!(parameters["parameters"][0]["value"], json!("<list, 3000 items, 9000 chars>"), "{parameters}");
    assert!(parameters["parameters"][0].get("start").is_none());
    let only = mcp(&state, "get_parameters", json!({"designId": id, "names": ["nothing"]}));
    assert_eq!(only["structuredContent"]["parameters"], json!([]));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn diff_explains_the_edit_geometrically() {
    let (state, root) = state();
    let from = "cube(10);\ntranslate([12, 0, 0]) cube(10);\n";
    let to = "cube(10);\ntranslate([11, 0, 0]) cube([10, 10, 11]);\n";
    let result = mcp(&state, "diff", json!({"designId": crate::scancommon::testkit::design_with_revisions(&state, from, to)}));
    let structured = &result["structuredContent"];
    let text = structured["summary"]["text"].as_array().unwrap();
    assert_eq!(text.len(), 2, "{structured}");
    assert!(text[0].as_str().unwrap().contains("(+10.00%)"), "{text:?}");
    assert!(text[0].as_str().unwrap().contains("10.000 x 10.000 x 10.000 -> 10.000 x 10.000 x 11.000"), "{text:?}");
    assert!(text[1].as_str().unwrap().contains("clearance 2.000 -> 1.000 mm"), "{text:?}");
    let _ = fs::remove_dir_all(root);
}

/// usage-report-2: grown script lettering used to come out with
/// non-manifold edges (offset contours overlapping) and no warning.
#[test]
fn grown_script_text_is_a_valid_solid() {
    let (state, root) = state();
    for code in [
        "linear_extrude(1) offset(r = 0.45) text(\"Forever yours\", size = 8, font = \"Great Vibes\");",
        "linear_extrude(1) offset(delta = 0.45) text(\"Forever yours\", size = 8, font = \"Great Vibes\", $fn = 64);",
        "linear_extrude(1) offset(r = -0.1) offset(r = 0.6) text(\"With love\", size = 8, font = \"Dancing Script\");",
    ] {
        let result = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "intersections": false}));
        let structured = &result["structuredContent"];
        assert_eq!(structured["meshes"]["valid"], json!(true), "{code}: {structured}");
        assert_eq!(structured["objects"][0]["meshOk"], json!(true), "{code}");
    }
    let _ = fs::remove_dir_all(root);
}

/// An open mesh says where its holes are: one located boundary group per
/// hole, in measure and evaluate, and the warning points at the largest.
#[test]
fn open_meshes_locate_their_holes() {
    let (state, root) = state();
    // A 10 mm box with no top face (z = 10).
    let code = "polyhedron(points = [[0,0,0],[10,0,0],[10,10,0],[0,10,0],[0,0,10],[10,0,10],[10,10,10],[0,10,10]], \
                    faces = [[0,1,2,3],[0,4,5,1],[1,5,6,2],[2,6,7,3],[3,7,4,0]]);\n";
    let measured = mcp(&state, "measure", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None)}));
    let object = &measured["structuredContent"]["objects"][0];
    assert_eq!(object["boundaryEdges"], json!(4), "{object}");
    let defects = object["defects"].as_array().expect("defects listed");
    assert_eq!(defects.len(), 1, "{object}");
    assert_eq!(defects[0]["kind"], json!("boundary"));
    assert_eq!(defects[0]["edges"], json!(4));
    assert_eq!(defects[0]["centroid"], json!([5.0, 5.0, 10.0]));
    assert_eq!(defects[0]["bounds"], json!([[0.0, 0.0, 10.0], [10.0, 10.0, 10.0]]));
    assert_eq!(defects[0]["samples"].as_array().unwrap().len(), 3);
    assert!(object.get("defectClusters").is_none(), "only when capped");
    let evaluated = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "intersections": false}));
    let structured = &evaluated["structuredContent"];
    assert_eq!(structured["objects"][0]["defects"], object["defects"], "{structured}");
    assert!(structured["warnings"].to_string().contains("around [5.0,5.0,10.0]"), "{structured}");
    // Healthy meshes carry no defects field.
    let clean = mcp(&state, "measure", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10);"), None)}));
    assert!(clean["structuredContent"]["objects"][0].get("defects").is_none());
    let _ = fs::remove_dir_all(root);
}

/// A model that compiles but is not a closed solid says so: a warning,
/// meshes.invalid, and a verdict that is compiled but not printable.
#[test]
fn compiled_but_invalid_meshes_are_flagged() {
    let (state, root) = state();
    let code = "polyhedron(points = [[0,0,0],[10,0,0],[0,10,0],[0,0,10]], faces = [[0,2,1],[0,1,3],[0,3,2],[1,3,2]]);\n";
    let result = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "intersections": false}));
    let structured = &result["structuredContent"];
    assert_eq!(structured["meshes"]["valid"], json!(false), "{structured}");
    assert!(structured["warnings"].to_string().contains("not a valid closed solid"), "{structured}");
    assert_eq!(structured["verdict"]["compiled"], json!(true));
    assert_eq!(structured["verdict"]["printable"], json!(false));
    assert_eq!(structured["objects"][0]["meshOk"], json!(false));
    // A clean model passes, and the verdict says what was not checked.
    // Without the supports check a clean mesh is not called printable.
    let clean = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10);"), None), "intersections": false}));
    assert_eq!(clean["structuredContent"]["verdict"]["printable"], json!(false));
    assert_eq!(clean["structuredContent"]["verdict"]["printability"], json!("not-checked"));
    assert!(clean["structuredContent"]["verdict"]["notChecked"].to_string().contains("thickness"));
    assert!(clean["structuredContent"]["verdict"]["notChecked"].to_string().contains("supports"));
    let checked = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10);"), None), "intersections": false, "checks": ["supports"]}));
    let verdict = &checked["structuredContent"]["verdict"];
    assert_eq!(verdict["printable"], json!(true), "{verdict}");
    assert_eq!(verdict["printability"], json!("clear-under-profile"));
    assert!(!verdict["notChecked"].to_string().contains("supports"), "{verdict}");
    // Write replies flag it too.
    let (id, revision) = create(&state, "cube(1);\n");
    let update = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": revision, "code": code, "revisionName": "Test revision"}));
    assert!(update["structuredContent"]["warnings"].to_string().contains("object-1"), "{update}");
    let _ = fs::remove_dir_all(root);
}

/// set_text measures and fixes printability, writes one marked block,
/// and a second call with the same id replaces it in place.
#[test]
fn set_text_writes_printable_lettering_as_a_replaceable_block() {
    let (state, root) = state();
    let (id, revision) = create(&state, "cube([60, 20, 2]);\n");
    let first = mcp(
        &state,
        "set_text",
        json!({
            "designId": id,
            "expectedRevision": revision,
            "id": "label",
            "text": "\u{5e9}\u{5dc}\u{5d5}\u{5dd}",
            "font": "Liberation Sans",
            "size": 8,
            "direction": "rtl",
            "language": "he",
            "translate": [30, 10, 2],
            "color": "gold",
            "revisionName": "Test revision",
        }));
    let structured = &first["structuredContent"];
    assert_eq!(first["isError"], json!(false), "{first}");
    let text = &structured["text"];
    assert_eq!(text["ok"], json!(true), "{text}");
    assert!(text["narrowestStroke"]["after"].as_f64().unwrap() >= 0.84 - 0.06, "{text}");
    assert!(text["narrowestStroke"]["before"].as_f64().unwrap() < 0.84, "{text}");
    let revision = structured["revision"].as_u64().unwrap();
    let second = mcp(
        &state,
        "set_text",
        json!({
            "designId": id,
            "expectedRevision": revision,
            "id": "label",
            "text": "Hi",
            "font": "Liberation Sans:style=Bold",
            "fit": {"width": 20},
            "translate": [30, 10, 2],
            "color": "gold",
            "revisionName": "Test revision",
        }));
    assert_eq!(second["isError"], json!(false), "{second}");
    let code = mcp(&state, "get_design_code", json!({"designId": id}));
    let code = code["structuredContent"]["code"].as_str().unwrap().to_string();
    assert_eq!(code.matches("// reopenscad:text id=label").count(), 1, "{code}");
    assert!(code.contains("text(\"Hi\"") && !code.contains("\u{5e9}"), "{code}");
    assert!(code.starts_with("cube([60, 20, 2]);"), "{code}");
    // returnCode is honoured.
    let revision = second["structuredContent"]["revision"].as_u64().unwrap();
    let third = mcp(&state, "set_text", json!({"designId": id, "expectedRevision": revision, "id": "label", "text": "Ho", "font": "Liberation Sans", "size": 6, "returnCode": true, "revisionName": "Test revision"}));
    assert!(third["structuredContent"]["code"].as_str().unwrap().contains("text(\"Ho\""), "{third}");
    // A fit the text cannot meet is reported, not ok, and it still saves: the
    // person sees what the agent got.
    let revision = third["structuredContent"]["revision"].as_u64().unwrap();
    let tiny = mcp(&state, "set_text", json!({"designId": id, "expectedRevision": revision, "text": "H", "fit": {"width": 0.001}, "minStroke": 0, "minGap": 0, "revisionName": "Test revision"}));
    assert_eq!(tiny["structuredContent"]["text"]["ok"], json!(false), "{tiny}");
    // Without a design there is nothing to write to.
    assert_eq!(mcp(&state, "set_text", json!({"text": "A", "size": 5, "revisionName": "Test revision"}))["isError"], json!(true));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn measure_runs_opt_in_printability_checks() {
    let (state, root) = state();
    // A 0.5 mm plate with a 10 mm block standing on it, and a bridge
    // whose underside is an overhang.
    let code = "cube([20, 20, 0.5]);\ntranslate([30, 0, 0]) cube(10);\n\
                    translate([0, 30, 0]) { cube([2, 2, 5]); translate([0, 0, 5]) cube([10, 2, 2]); }\n";
    let (status, body) = http_json(&state, "POST", &format!("/api/designs/{}/measure", crate::scancommon::testkit::save_revision(&state, None, Some(code), None)), Some(json!({"checks": ["thickness", "overhang", "sharpEdges"]})));
    assert_eq!(status, 200, "{body}");
    let plate = &body["objects"][0]["checks"];
    assert!(close(plate["thickness"]["min"].as_f64().unwrap(), 0.5, 1e-3), "{plate}");
    assert!(plate["thickness"]["belowThreshold"].as_u64().unwrap() > 0);
    assert_eq!(plate["thickness"]["complete"], json!(true));
    assert_eq!(plate["overhang"]["area"], json!(0.0), "{plate}");
    let block = &body["objects"][1]["checks"];
    assert!(close(block["thickness"]["min"].as_f64().unwrap(), 10.0, 1e-3));
    assert_eq!(block["thickness"]["belowThreshold"], json!(0));
    assert_eq!(block["sharpEdges"]["edges"], json!(12), "{block}");
    assert_eq!(block["sharpEdges"]["creases"], json!(1));
    assert!(close(block["sharpEdges"]["length"].as_f64().unwrap(), 120.0, 1e-9));
    let bridge = &body["objects"][2]["checks"]["overhang"];
    // Underside of the 10 x 2 arm minus the 2 x 2 where the post meets it.
    assert!(close(bridge["area"].as_f64().unwrap(), 16.0, 1e-6), "{bridge}");
    assert_eq!(bridge["bounds"]["min"][2], json!(5.0));
    assert!(close(bridge["bedArea"].as_f64().unwrap(), 4.0, 1e-9));

    // Not requested, not computed; bad check names are rejected.
    let result = mcp(&state, "measure", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None)}));
    assert!(result["structuredContent"]["objects"][0].get("checks").is_none());
    let result = mcp(&state, "measure", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "checks": ["wobble"]}));
    assert_eq!(result["isError"], json!(true), "{result}");
    let result = mcp(
        &state,
        "measure",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "checks": ["sharpEdges"],
            "sharpEdgeAngle": 89,
        }));
    assert_eq!(
        result["structuredContent"]["objects"][1]["checks"]["sharpEdges"]["edges"],
        json!(0)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn undetermined_pairs_count_as_problems() {
    let clean = json!({"toleranceViolated": false, "determined": true});
    let undecided = json!({"toleranceViolated": false, "determined": false});
    assert!(!is_problem_pair(&clean));
    assert!(is_problem_pair(&undecided));
    assert!(is_problem_pair(&json!({"skipped": true})));
    let summary = pair_summary(&[clean, undecided]);
    assert_eq!(summary["undetermined"], json!(1));
}

/// A clean pair proved clear with only an upper-bound distance keeps
/// saying so after slimming.
#[test]
fn a_slim_clean_pair_keeps_an_inexact_distance() {
    let pair = json!({
        "a": "a", "b": "b", "relation": "clear", "clearance": 1.5, "clearanceLowerBound": 0.9,
        "distanceExact": false, "toleranceViolated": false, "determined": true,
    });
    let slim = slim_pair(&pair);
    assert_eq!(slim["distanceExact"], json!(false), "{slim}");
    assert_eq!(slim["clearanceLowerBound"], json!(0.9), "{slim}");
    let exact = json!({"a": "a", "b": "b", "relation": "clear", "clearance": 1.5, "clearanceLowerBound": 1.5,
        "distanceExact": true, "toleranceViolated": false, "determined": true});
    let slim = slim_pair(&exact);
    assert!(slim.get("distanceExact").is_none() && slim.get("clearanceLowerBound").is_none(), "{slim}");
}

#[test]
fn an_incomplete_measure_is_unknown_not_bad() {
    let triangles = [[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]];
    let measure = {
        let _deadline = crate::csg::bsp::deadline_scope(Instant::now());
        analysis::measure_triangles(&triangles)
    };
    let mut value = measure_public(&measure);
    assert_eq!(value["complete"], json!(false));
    assert!(value["watertight"].is_null(), "{value}");
    slim_object(&mut value);
    assert!(value.get("meshOk").is_none(), "{value}");
}

fn pair_of(state: &AppState, code: &str, extra: Value) -> Value {
    // Full records: these tests read every pair field.
    let mut arguments = json!({"designId": crate::scancommon::testkit::save_revision(state, None, Some(code), None), "verbose": true});
    if let (Value::Object(arguments), Value::Object(extra)) = (&mut arguments, extra) {
        arguments.extend(extra);
    }
    let result = mcp(state, "check_intersections", arguments);
    assert_eq!(result["isError"], json!(false), "{result}");
    result["structuredContent"]["intersections"][0].clone()
}

// Feedback (gridfinity, threads, skadis): contact was reported as both
// withinTolerance and toleranceViolated even at tolerance 0.
#[test]
fn contact_overlap_and_clearance_are_classified_separately() {
    let (state, root) = state();
    let touching = "cube(10); translate([10,0,0]) cube(10);";
    let pair = pair_of(&state, touching, json!({"tolerance": 0}));
    assert_eq!(pair["relation"], json!("contact"), "{pair}");
    assert_eq!(pair["touching"], json!(true));
    assert_eq!(pair["withinTolerance"], json!(false));
    assert_eq!(pair["toleranceViolated"], json!(false));
    assert_eq!(pair["contactAllowed"], json!(true));
    assert_eq!(pair["overlapVolume"], json!(0.0));
    // A required gap makes contact a violation, unless it is intended.
    let pair = pair_of(&state, touching, json!({"tolerance": 0.2}));
    assert_eq!(pair["relation"], json!("contact"));
    assert_eq!(pair["toleranceViolated"], json!(true));
    assert_eq!(pair["contactAllowed"], json!(false));
    let pair = pair_of(&state, touching, json!({"tolerance": 0.2, "allowContact": true}));
    assert_eq!(pair["toleranceViolated"], json!(false), "{pair}");
    let pair = pair_of(
        &state,
        touching,
        json!({"tolerance": 0.2, "allowContact": [["object-2", "object-1"]]}),
    );
    assert_eq!(pair["toleranceViolated"], json!(false), "{pair}");
    let result = mcp(
        &state,
        "check_intersections",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(touching), None),
            "allowContact": [["object-1", "object-9"]],
        }));
    assert_eq!(result["isError"], json!(true), "{result}");
    // A gap equal to the tolerance passes; a smaller one does not.
    let pair = pair_of(&state, "cube(10); translate([10.2,0,0]) cube(10);", json!({"tolerance": 0.2}));
    assert_eq!(pair["relation"], json!("clear"), "{pair}");
    assert_eq!(pair["toleranceViolated"], json!(false));
    let pair = pair_of(&state, "cube(10); translate([10.1,0,0]) cube(10);", json!({"tolerance": 0.2}));
    assert_eq!(pair["relation"], json!("clearanceViolation"), "{pair}");
    assert_eq!(pair["toleranceViolated"], json!(true));
    assert_eq!(pair["withinTolerance"], json!(true));
    // Allowing contact never excuses a real overlap.
    let result = mcp(
        &state,
        "check_intersections",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10); translate([9,0,0]) cube(10);"), None),
            "tolerance": 0,
            "allowContact": true,
        }));
    let structured = &result["structuredContent"];
    assert_eq!(structured["intersections"][0]["relation"], json!("overlap"));
    assert_eq!(structured["intersections"][0]["toleranceViolated"], json!(true));
    assert_eq!(structured["summary"]["overlaps"], json!(1));
    // Zero counts and the semantics text are left out of compact replies
    // (the tool description defines the relations).
    assert!(structured["summary"].get("contacts").is_none(), "{structured}");
    assert_eq!(structured["complete"], json!(true));
    assert!(structured.get("semantics").is_none());
    let _ = fs::remove_dir_all(root);
}

// Feedback (gridfinity, maker-hardware): depth was the overlap box's
// thinnest extent, 7 mm for a 0.01 mm over-insertion of a tilted plate.
#[test]
fn depth_measures_local_interference_not_the_overlap_box() {
    let (state, root) = state();
    let tilted = "rotate([0,45,0]) cube([10,10,2]);\nrotate([0,45,0]) translate([0,0,1.99]) cube([10,10,2]);";
    let pair = pair_of(&state, tilted, json!({"tolerance": 0}));
    assert_eq!(pair["relation"], json!("overlap"), "{pair}");
    let depth = pair["depth"].as_f64().unwrap();
    assert!(close(depth, 0.01, 1e-6), "{pair}");
    assert_eq!(pair["depthMethod"], json!("overlapThickness"));
    assert!(pair["overlapBoundsMinExtent"].as_f64().unwrap() > 7.0, "{pair}");
    // Crossing bars: no vertex is inside the other; the overlap solid
    // (2 x 2 x 1) is 1 mm thick.
    let bars = "cube([20,2,2], center=true); cube([2,20,1], center=true);";
    let pair = pair_of(&state, bars, json!({"tolerance": 0}));
    assert!(close(pair["depth"].as_f64().unwrap(), 1.0, 1e-9), "{pair}");
    assert!(pair["vertexPenetration"].is_null());
    // A press fit: 0.025 mm radial interference over a 7 mm height.
    let press = "$fn=64;\ndifference() { cylinder(d=30,h=7); translate([0,0,-1]) cylinder(d=21.95/cos(180/64),h=9); }\ndifference() { cylinder(d=22,h=7,$fn=128); translate([0,0,-1]) cylinder(d=8,h=9); }\n";
    let pair = pair_of(&state, press, json!({"tolerance": 0.01}));
    assert_eq!(pair["relation"], json!("overlap"), "{pair}");
    let depth = pair["depth"].as_f64().unwrap();
    assert!(depth > 0.02 && depth <= 0.026, "{pair}");
    let _ = fs::remove_dir_all(root);
}

// Feedback (gridfinity, opengrid): measure and intersections had no
// console, and the objects console miscounted triangles.
#[test]
fn every_analysis_response_carries_console_warnings_and_timing() {
    let (state, root) = state();
    let code = "echo(\"CHECK ME\");\ncube(1, bogus=2);\ntranslate([2,0,0]) cube(1);\n";
    for tool in ["list_workspace_objects", "measure", "check_intersections", "render_image"] {
        let design = crate::scancommon::testkit::save_revision(&state, None, Some(code), None);
        let result = mcp(&state, tool, json!({"designId": design, "width": 64, "height": 64}));
        let structured = &result["structuredContent"];
        let console = structured["console"].as_array().unwrap_or_else(|| panic!("{tool}: {structured}"));
        assert!(
            console.iter().any(|line| line.as_str().unwrap().contains("CHECK ME")),
            "{tool}: {console:?}"
        );
        let warnings = structured["warnings"].as_array().unwrap();
        assert!(
            warnings.iter().all(|line| line.as_str().unwrap().starts_with("WARNING")),
            "{tool}: {warnings:?}"
        );
        assert!(!warnings.is_empty(), "{tool}: bogus argument should warn");
        assert!(structured["elapsedMs"].is_u64(), "{tool}");
    }
    let result = mcp(
        &state,
        "diff",
        json!({
            "designId": crate::scancommon::testkit::design_with_revisions(&state, "cube(1);", "echo(\"TO\"); cube(2);"),
        }));
    let structured = &result["structuredContent"];
    assert!(structured["toConsole"].to_string().contains("TO"), "{structured}");
    // Per-object and combined triangle counts are labelled separately.
    let result = mcp(
        &state,
        "list_workspace_objects",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10); translate([5,0,0]) cube(10);"), None),
        }));
    let console = result["structuredContent"]["console"].to_string();
    assert!(console.contains("2 objects with 24 triangles in total"), "{console}");
    assert!(console.contains("combined (unioned) model mesh"), "{console}");
    let _ = fs::remove_dir_all(root);
}

// Feedback (skadis, din-rail, maker-hardware): parts built by one module
// call were one object, and unions could not be measured.
#[test]
fn measure_splits_components_and_measures_groups() {
    let (state, root) = state();
    let code = "module assembly(){cube(2); translate([5,0,0]) cube(2);} assembly();";
    let result = mcp(
        &state,
        "measure",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "splitComponents": true,
            "pairs": [["object-1#1", "object-1#2"]],
        }));
    let structured = &result["structuredContent"];
    assert_eq!(result["isError"], json!(false), "{result}");
    let objects = structured["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2, "{structured}");
    assert_eq!(objects[0]["kind"], json!("component"));
    assert_eq!(objects[0]["parent"], json!("object-1"));
    assert!(close(objects[0]["volume"].as_f64().unwrap(), 8.0, 1e-9));
    assert!(
        close(structured["distances"][0]["clearance"].as_f64().unwrap(), 3.0, 1e-9),
        "{structured}"
    );
    // A hollow part is one solid: its void stays with it.
    let result = mcp(
        &state,
        "measure",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("difference(){cube(10,center=true); cube(5,center=true);}"), None),
            "splitComponents": true,
        }));
    let objects = result["structuredContent"]["objects"].as_array().unwrap().clone();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0]["id"], json!("object-1"));
    assert!(close(objects[0]["volume"].as_f64().unwrap(), 875.0, 1e-6));
    // Groups: the union's volume counts the overlap once.
    let result = mcp(
        &state,
        "measure",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10); translate([5,0,0]) cube(10); translate([30,0,0]) cube(1);"), None),
            "groups": {"pair": ["object-1", "object-2"]},
            "pairs": [["pair", "object-3"]],
        }));
    let structured = &result["structuredContent"];
    assert_eq!(result["isError"], json!(false), "{result}");
    let group = &structured["groups"][0];
    assert_eq!(group["id"], json!("pair"));
    assert_eq!(group["members"], json!(["object-1", "object-2"]));
    assert!(close(group["volume"].as_f64().unwrap(), 1500.0, 1e-6), "{group}");
    assert!(close(structured["total"]["volume"].as_f64().unwrap(), 2001.0, 1e-6));
    assert!(close(structured["distances"][0]["clearance"].as_f64().unwrap(), 15.0, 1e-9));
    // Intersections: a group is checked as one target.
    let result = mcp(
        &state,
        "check_intersections",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10); translate([5,0,0]) cube(10); translate([10,0,0]) cube(10);"), None),
            "groups": [["object-1", "object-2"]],
            "tolerance": 0,
        }));
    let structured = &result["structuredContent"];
    assert_eq!(structured["summary"]["pairs"], json!(1), "{structured}");
    assert_eq!(structured["intersections"][0]["b"], json!("group-1"));
    assert_eq!(structured["intersections"][0]["relation"], json!("overlap"));
    let result = mcp(
        &state,
        "measure",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(1);"), None),
            "groups": [["object-7"]],
        }));
    assert_eq!(result["isError"], json!(true));
    let _ = fs::remove_dir_all(root);
}

/// Overrides are refused, not ignored, on a read-only link as everywhere else.
#[test]
fn overrides_are_refused_on_every_render_route() {
    let (state, root) = state();
    let code = "size = 10;\ncube(size);\n";
    let (status, plain) = http_json(&state, "POST", "/api/render", Some(json!({"code": code, "requestId": "ovr00000001"})));
    assert_eq!(status, 200, "{plain}");
    assert!(plain.get("overrides").is_none());
    assert_eq!(plain["degenerateTriangles"], json!(0));
    let (status, a) = http(&state, "POST", "/api/export", Some(json!({"code": code, "format": "stl"})));
    assert_eq!(status, 200);
    let (id, revision) = create(&state, code);
    let (_, workspace_plain) = http(&state, "POST", &format!("/api/workspaces/{id}/export"), Some(json!({"revision": revision})));
    assert_eq!(workspace_plain, a);
    let query = format!("/api/workspaces/{id}/export?format=stl&revision={revision}&overrides={}", percent_encode("{\"size\":20}"));
    let (status, _) = http(&state, "GET", &query, None);
    assert_eq!(status, 400);

    let token = state.workspaces.share_token(&id).unwrap();
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/shared/{token}/render"),
        Some(json!({"revision": revision, "requestId": "ovr00000005", "overrides": {"size": 20}})),
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(state.workspaces.get(&id).unwrap().unwrap().revision, revision);
    let (status, plain) = http_json(
        &state,
        "POST",
        &format!("/api/shared/{token}/render"),
        Some(json!({"revision": revision, "requestId": "ovr00000007"})),
    );
    assert_eq!(status, 200, "{plain}");
    assert_eq!(plain["objects"][0]["bounds"]["max"], json!([10.0, 10.0, 10.0]));
    let (status, bad) = http_json(
        &state,
        "POST",
        &format!("/api/shared/{token}/render"),
        Some(json!({"revision": revision, "requestId": "ovr00000006", "overrides": [1]})),
    );
    assert_eq!(status, 400, "{bad}");

    let result = mcp(&state, "render_workspace", json!({"workspaceId": id}));
    let structured = &result["structuredContent"];
    assert_eq!(structured["revision"], json!(revision));
    assert_eq!(base64_decode(structured["dataBase64"].as_str().unwrap()).unwrap(), a);
    let _ = fs::remove_dir_all(root);
}

/// A second render of an unchanged revision is served from the stored
/// response, identical but for the per-request fields; changing an input
/// of the key misses.
#[test]
fn an_unchanged_revision_renders_from_the_stored_response() {
    let (state, root) = state();
    let (id, revision) = create(&state, "cube(3); translate([5,0,0]) sphere(1);");
    let render = |request_id: &str, extra: Value| {
        let mut body = json!({"revision": revision, "requestId": request_id});
        if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
            body.extend(extra);
        }
        http_json(&state, "POST", &format!("/api/designs/{id}/render"), Some(body))
    };
    let (status, first) = render("cache0000001", json!({}));
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["cached"], json!(false));
    assert!(root.join("render-cache").join(&id).is_dir());
    let (status, second) = render("cache0000002", json!({}));
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["cached"], json!(true));
    assert_eq!(second["revision"], json!(revision));
    assert_eq!(second["designId"], json!(id));
    for field in ["objects", "backgroundObjects", "triangles", "console", "mesh", "overrides"] {
        assert_eq!(first[field], second[field], "{field}");
    }
    let (_, rendered) = render("cache0000003", json!({"mode": "render"}));
    assert_eq!(rendered["cached"], json!(false));
    assert_eq!(rendered["mode"], json!("render"));
    let object = first["objects"][0]["id"].clone();
    let (status, selected) = render("cache0000004", json!({"objectIds": [object]}));
    assert_eq!(status, 200, "{selected}");
    assert_eq!(selected["cached"], json!(false));
    assert!(selected["mesh"].is_string());
    // Unseeded randomness differs per run, so it is never served stored.
    let (random, random_revision) = create(&state, "cube(rands(1, 2, 1)[0]);");
    for request_id in ["cache0000005", "cache0000006"] {
        let (status, body) = http_json(
            &state,
            "POST",
            &format!("/api/designs/{random}/render"),
            Some(json!({"revision": random_revision, "requestId": request_id})),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["cached"], json!(false));
    }
    let _ = fs::remove_dir_all(root);
}

/// One request, one compile: objects with bounds/volume/manifold, every
/// pairwise verdict against the tolerance, a PNG and the diagnostics.
#[test]
fn evaluate_answers_everything_from_one_compile() {
    let (state, root) = state();
    let code = "gap = 0.1;\ncube(2);\ntranslate([2 + gap, 0, 0]) cube(2);\necho(\"hi\");\n";
    let (status, body) = http_json(&state, "POST", &format!("/api/designs/{}/evaluate", crate::scancommon::testkit::save_revision(&state, None, Some(code), None)), Some(json!({"tolerance": 0.2, "image": {"width": 160, "height": 120, "view": "iso"}})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["objects"].as_array().unwrap().len(), 2);
    assert!(close(body["objects"][0]["volume"].as_f64().unwrap(), 8.0, 1e-9));
    assert_eq!(body["objects"][0]["manifold"], json!(true));
    assert_eq!(body["objects"][1]["bounds"]["min"][0], json!(2.1));
    let pair = &body["intersections"]["intersections"][0];
    assert_eq!(pair["relation"], json!("clearanceViolation"), "{pair}");
    assert!(close(pair["clearance"].as_f64().unwrap(), 0.1, 1e-9));
    assert_eq!(body["intersections"]["tolerance"], json!(0.2));
    assert_eq!(body["intersections"]["summary"]["clearanceViolations"], json!(1));
    assert_eq!(body["complete"], json!(true));
    let png = base64_decode(body["image"]["base64"].as_str().unwrap()).unwrap();
    assert_eq!(&png[1..4], b"PNG");
    assert_eq!(body["image"]["render"]["width"], json!(160));
    assert!(body["console"].as_array().unwrap().iter().any(|line| line.as_str().unwrap().contains("hi")));
    assert!(body["elapsedMs"].is_number());
    assert!(body["warnings"].is_array());

    // A saved workspace, intersections off, no image.
    let (id, _) = create(&state, code);
    crate::scancommon::testkit::save_revision(&state, Some(&id), None, Some(&json!({"gap": 1})));
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/evaluate"),
        Some(json!({"intersections": false})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["workspaceId"], json!(id));
    assert_eq!(body["objects"][1]["bounds"]["min"][0], json!(3.0));
    assert!(body["intersections"].is_null());
    assert!(body.get("image").is_none());
    // Intersection options of their own: allowContact at an exact touch.
    crate::scancommon::testkit::save_revision(&state, Some(&id), None, Some(&json!({"gap": 0})));
    let (_, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/evaluate"),
        Some(json!({"intersections": {"tolerance": 0.2, "allowContact": true}})),
    );
    assert_eq!(body["intersections"]["intersections"][0]["relation"], json!("contact"));
    assert_eq!(body["intersections"]["intersections"][0]["toleranceViolated"], json!(false));
    // MCP: the picture as an image block, the numbers as structured data.
    let result = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "image": true}));
    assert_eq!(result["isError"], json!(false), "{result}");
    assert_eq!(result["content"][0]["type"], json!("image"));
    assert!(result["structuredContent"]["image"]["base64"].is_null());
    assert_eq!(result["structuredContent"]["intersections"]["summary"]["pairs"], json!(1));
    let _ = fs::remove_dir_all(root);
}

/// Pairs that were not decided carry every flag as "not clear", so no
/// reader can take them for a pass, and they count against complete.
#[test]
fn skipped_and_undecided_pairs_never_read_as_clear() {
    let (state, root) = state();
    let source = eval_source(&state, None, &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(1); translate([5,0,0]) cube(1);"), None)})).unwrap();
    let compiled = compile_eval(&source).unwrap();
    let targets: Vec<Target> = compiled.parts.iter().map(Target::object).collect();
    let past = Instant::now() - Duration::from_secs(1);
    let pairs = check_pairs(&targets, 0.2, &ContactPolicy::Never, &source, past).unwrap();
    assert_eq!(pairs.len(), 1);
    let pair = &pairs[0];
    assert_eq!(pair["relation"], json!("skipped"));
    assert_eq!(pair["determined"], json!(false));
    assert_eq!(pair["toleranceViolated"], json!(true));
    assert!(pair["intersects"].is_null() && pair["clearance"].is_null());
    assert!(is_problem_pair(pair));
    let summary = pair_summary(&pairs);
    assert_eq!(summary["complete"], json!(false));
    assert_eq!(summary["clear"], json!(0));
    // A pair the distance search cannot even start on is kept, undecided.
    let empty = engine::Mesh { triangles: Vec::new() };
    let mut hollow = Target::object(&compiled.parts[1]);
    hollow.mesh = std::borrow::Cow::Owned(empty);
    let both = [Target::object(&compiled.parts[0]), hollow];
    let pairs = check_pairs(&both, 0.2, &ContactPolicy::Never, &source, Instant::now() + BATCH_BUDGET).unwrap();
    assert_eq!(pairs.len(), 1, "never dropped");
    assert_eq!(pairs[0]["relation"], json!("undetermined"));
    assert_eq!(pairs[0]["toleranceViolated"], json!(true));
    assert_eq!(pair_summary(&pairs)["complete"], json!(false));
    let _ = fs::remove_dir_all(root);
}

/// An object floating above the bed has its underside counted as
/// overhang: the bed is the model's lowest Z (or bedZ), not each
/// object's own.
#[test]
fn a_floating_object_is_not_bed_supported() {
    let (state, root) = state();
    let code = "cube(10); translate([20, 0, 10]) cube(10);";
    let value = measure_value(&state, None, &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "checks": ["overhang"]})).unwrap();
    let grounded = &value["objects"][0]["checks"]["overhang"];
    let floating = &value["objects"][1]["checks"]["overhang"];
    assert_eq!(grounded["bedArea"], json!(100.0), "{grounded}");
    assert_eq!(grounded["area"], json!(0.0));
    assert_eq!(floating["bedArea"], json!(0.0), "{floating}");
    assert!(close(floating["area"].as_f64().unwrap(), 100.0, 1e-9));
    assert_eq!(floating["bedZ"], json!(0.0));
    // Alone, it floats too.
    let value = measure_value(
        &state,
        None,
        &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("translate([0,0,10]) cube(10);"), None), "checks": ["overhang"], "bedZ": 0}),
    )
    .unwrap();
    assert!(close(value["objects"][0]["checks"]["overhang"]["area"].as_f64().unwrap(), 100.0, 1e-9));
    let _ = fs::remove_dir_all(root);
}

/// Oversized ID lists and group counts are refused before any per-ID
/// work; a group too big to union exactly is refused, not attempted.
#[test]
fn oversized_selections_are_refused_up_front() {
    let (state, root) = state();
    let code = "cube(1); translate([3,0,0]) cube(1);";
    let groups: Vec<Value> = (0..MAX_GROUPS + 1).map(|index| json!([format!("nope-{index}")])).collect();
    let error = measure_value(&state, None, &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "groups": groups})).unwrap_err();
    assert!(error.message.contains("groups per call"), "{}", error.message);
    let ids: Vec<String> = (0..MAX_ID_LIST + 1).map(|index| format!("object-{index}")).collect();
    let error = measure_value(&state, None, &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "objectIds": ids})).unwrap_err();
    assert!(error.message.contains("at most"), "{}", error.message);
    let error = intersections_value(&state, None, &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "allowContact": vec![json!(["object-1", "object-2"]); MAX_ID_LIST + 1]})).unwrap_err();
    assert!(error.message.contains("allowContact"), "{}", error.message);
    let heavy = "sphere(5, $fn=150); translate([20,0,0]) sphere(5, $fn=150); translate([40,0,0]) sphere(5, $fn=150);";
    let error = measure_value(&state, None, &json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(heavy), None), "groups": [["object-1", "object-2", "object-3"]]})).unwrap_err();
    assert_eq!(error.status, 422);
    assert!(error.message.contains("exact union"), "{}", error.message);
    let _ = fs::remove_dir_all(root);
}

/// Workspace reads remember revisions that do not compile or have too
/// many objects, instead of recompiling them on every read.
#[test]
fn workspace_reads_remember_uncompilable_and_oversized_revisions() {
    let (state, root) = state();
    let (id, revision) = create(&state, "cube(1);");
    remember_compile_failure(&id, revision, "boom");
    let (status, body) = http_json(&state, "GET", &format!("/api/workspaces/{id}?objects=compute"), None);
    assert_eq!(status, 200);
    assert_eq!(body["workspace"]["objectsStatus"], json!("failed"), "{body}");
    assert!(body["workspace"]["objectsHint"].as_str().unwrap().contains("boom"));
    remember(&id, revision, CachedObjects::TooMany(9000));
    let (_, body) = http_json(&state, "GET", &format!("/api/workspaces/{id}"), None);
    assert_eq!(body["workspace"]["objectsStatus"], json!("too_many"));
    assert!(body["workspace"]["objectsHint"].as_str().unwrap().contains("9000"));
    let parts: Vec<engine::CompiledPart> = engine::compile_parts(
        &"cube(1);".repeat(OBJECT_SUMMARY_MAX_PARTS + 1),
        engine::Quality::Preview,
        None,
    )
    .unwrap()
    .parts
    .clone();
    remember_objects(&id, revision, &parts);
    assert!(matches!(object_cache(&id, revision), Some(CachedObjects::TooMany(count)) if count == parts.len()));
    let _ = fs::remove_dir_all(root);
}

/// An HTTP set-parameters call is an agent edit unless it says otherwise,
/// so the browser highlights it as AI.
#[test]
fn http_parameter_writes_are_highlighted_as_ai() {
    let (state, root) = state();
    let (id, revision) = create(&state, "w = 2;\ncube(w);\n");
    let (status, body) = http_json(
        &state,
        "POST",
        &format!("/api/workspaces/{id}/parameters"),
        Some(json!({"values": {"w": 5}, "baseRevision": revision})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["workspace"]["updatedRanges"][0]["source"], json!("ai"));
    let _ = fs::remove_dir_all(root);
}

/// A full-code MCP update reports (and stores for the browser) only the
/// changed lines, region by region.
#[test]
fn full_code_mcp_updates_report_minimal_ranges() {
    let (state, root) = state();
    let old: String = (1..=40).map(|line| format!("// {line}\n")).collect::<String>() + "cube(1);\n";
    let (id, revision) = create(&state, &old);
    let new = old.replace("// 3\n", "// three\n").replace("cube(1);", "cube(2);");
    let result = mcp(
        &state,
        "update_workspace_code",
        json!({
            "workspaceId": id,
            "expectedRevision": revision,
            "code": new,
            "revisionName": "Test revision",
        }));
    let structured = &result["structuredContent"];
    assert_eq!(structured["ok"], json!(true), "{structured}");
    let ranges = structured["edit"]["ranges"].as_array().unwrap();
    assert_eq!(ranges.len(), 2, "{structured}");
    assert_eq!(ranges[0]["lines"], json!([3, 3]));
    assert_eq!(ranges[1]["lines"], json!([41, 41]));
    assert!(ranges.iter().all(|range| range["source"] == json!("ai")));
    // The agent sent the code; the reply does not echo it back.
    assert!(structured.get("code").is_none() && structured.get("workspace").is_none(), "{structured}");
    let _ = fs::remove_dir_all(root);
}

/// `/api/designs/...` is canonical and `/api/workspaces/...` keeps
/// answering identically, so links and agents written before the rename
/// keep working; replies carry designId next to workspaceId.
#[test]
fn design_routes_and_their_workspace_aliases_behave_identically() {
    let (state, root) = state();
    let (status, body) = http_json(&state, "POST", "/api/designs", Some(json!({"code": "cube(2);\n"})));
    assert_eq!(status, 201, "{body}");
    let id = body["designId"].as_str().unwrap().to_string();
    assert_eq!(body["workspace"]["id"], json!(id));
    assert!(body["url"].as_str().unwrap().ends_with(&format!("/designs/{id}")), "{body}");
    for prefix in ["/api/designs", "/api/workspaces"] {
        let (status, read) = http_json(&state, "GET", &format!("{prefix}/{id}"), None);
        assert_eq!(status, 200, "{read}");
        assert_eq!(read["workspace"]["code"], json!("cube(2);\n"));
        assert_eq!(read["designId"], json!(id));
        let (status, events) = http_json(&state, "GET", &format!("{prefix}/{id}/events?since=0"), None);
        assert_eq!(status, 200, "{events}");
        assert_eq!(events["designId"], json!(id));
        assert_eq!(events["workspaceId"], json!(id));
        let (status, evaluated) =
            http_json(&state, "POST", &format!("{prefix}/{id}/evaluate"), Some(json!({})));
        assert_eq!(status, 200, "{evaluated}");
        assert_eq!(evaluated["designId"], json!(id));
    }
    let (status, missing) = http_json(&state, "GET", "/api/designs/no-such-design-00112233445566778899", None);
    assert_eq!(status, 404);
    assert_eq!(missing["error"], json!("Design not found."));
    let _ = fs::remove_dir_all(root);
}

/// Canonical design tools take designId; the deprecated workspace tools
/// and the workspaceId argument still work, and disagreeing ids are refused.
#[test]
fn mcp_design_tools_and_their_workspace_aliases() {
    let (state, root) = state();
    let created = mcp(&state, "create_design", json!({"code": "cube(3);\n", "revisionName": "Test revision"}));
    let structured = &created["structuredContent"];
    assert_eq!(structured["ok"], json!(true), "{structured}");
    let id = structured["designId"].as_str().unwrap().to_string();
    assert!(structured["url"].as_str().unwrap().contains("/designs/"));
    let canonical = mcp(&state, "get_design_code", json!({"designId": id}));
    let alias = mcp(&state, "get_workspace_code", json!({"workspaceId": id}));
    let mixed = mcp(&state, "get_design_code", json!({"workspaceId": id}));
    for result in [&canonical, &alias, &mixed] {
        assert_eq!(result["structuredContent"]["code"], json!("cube(3);\n"), "{result}");
        assert_eq!(result["structuredContent"]["designId"], json!(id));
    }
    let updated = mcp(
        &state,
        "update_design_code",
        json!({
            "designId": id,
            "expectedRevision": 1,
            "code": "cube(4);\n",
            "revisionName": "Test revision",
        }));
    assert_eq!(updated["structuredContent"]["ok"], json!(true), "{updated}");
    let link = mcp(&state, "get_design_link", json!({"designId": id}));
    assert_eq!(
        link["structuredContent"]["appUri"],
        json!(format!("ui://reopenscad/design/{id}"))
    );
    let conflicting = mcp(
        &state,
        "get_design_code",
        json!({"designId": id, "workspaceId": "other-otter-00112233445566778899"}),
    );
    assert_eq!(conflicting["isError"], json!(true));
    let null_design = mcp(&state, "get_design_code", json!({"designId": null, "workspaceId": id}));
    assert_eq!(null_design["structuredContent"]["designId"], json!(id), "{null_design}");
    let _ = fs::remove_dir_all(root);
}

/// evaluate with checks: ["supports"] on the report's comparison cases.
fn supports_eval(state: &AppState, code: &str) -> Value {
    let result = mcp(state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(state, None, Some(code), None), "intersections": false, "checks": ["supports"]}));
    result["structuredContent"].clone()
}

fn support_issues<'a>(value: &'a Value, kind: &str) -> Vec<&'a Value> {
    value["objects"][0]["checks"]["supports"]["issues"]
        .as_array()
        .map(|issues| issues.iter().filter(|issue| issue["kind"] == json!(kind)).collect())
        .unwrap_or_default()
}

/// Separate top-level objects support each other: a slab written as
/// its own object, resting on two posts, is a bridge, not an island.
#[test]
fn supports_are_judged_on_the_whole_model() {
    let (state, root) = state();
    let code = "cube([2, 10, 10]);\ntranslate([8, 0, 0]) cube([2, 10, 10]);\ntranslate([0, 0, 10]) cube([10, 10, 2]);\n";
    let result = mcp(&state, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "checks": ["supports"], "intersections": false}));
    let structured = &result["structuredContent"];
    let slab = &structured["objects"][2]["checks"]["supports"];
    assert_eq!(slab["counts"]["islands"], json!(0), "{slab}");
    assert_eq!(slab["counts"]["bridges"], json!(1), "{slab}");
    assert_eq!(structured["verdict"]["printability"], json!("clear-under-profile"), "{}", structured["verdict"]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn supports_check_tells_bridges_cantilevers_and_islands_apart() {
    let (state, root) = state();
    // A short bridge anchored at both ends prints.
    let bridge = supports_eval(&state, "union() { cube([2,10,10]); translate([8,0,0]) cube([2,10,10]); translate([0,0,10]) cube([10,10,2]); }");
    let supports = &bridge["objects"][0]["checks"]["supports"];
    assert_eq!(supports["profile"]["assumed"], json!(true), "{supports}");
    assert_eq!(supports["complete"], json!(true), "{supports}");
    let bridges = support_issues(&bridge, "bridge");
    assert!(bridges.iter().all(|issue| issue["ok"] == json!(true)), "{supports}");
    assert_eq!(supports["counts"]["islands"], json!(0), "{supports}");
    assert_eq!(supports["counts"]["cantilevers"], json!(0), "{supports}");
    assert_eq!(bridge["verdict"]["printable"], json!(true), "{}", bridge["verdict"]);
    assert_eq!(bridge["verdict"]["printability"], json!("clear-under-profile"));

    // A cantilever: one anchor.
    let cantilever = supports_eval(&state, "union() { cube([4,10,10]); translate([0,0,10]) cube([12,10,2]); }");
    let found = support_issues(&cantilever, "cantilever");
    assert!(!found.is_empty(), "{}", cantilever["objects"][0]["checks"]);
    assert!((found[0]["z"].as_f64().unwrap() - 10.0).abs() < 0.5, "{}", found[0]);
    assert!(found[0]["location"][0].as_f64().unwrap() > 4.0, "{}", found[0]);
    assert_eq!(cantilever["verdict"]["printable"], json!(false));
    assert_eq!(cantilever["verdict"]["printability"], json!("obstacles"));
    assert!(cantilever["verdict"]["problems"].to_string().contains("cantilever"), "{}", cantilever["verdict"]);

    // An island that only joins the model higher up: one component, yet it
    // starts in midair.
    let island = supports_eval(&state, "union() { cube([4,10,20]); translate([10,0,8]) cube([4,10,12]); translate([0,0,20]) cube([14,10,2]); }");
    assert_eq!(island["meshes"]["valid"], json!(true), "{island}");
    let found = support_issues(&island, "island");
    assert!(!found.is_empty(), "{}", island["objects"][0]["checks"]);
    assert!((found[0]["z"].as_f64().unwrap() - 8.0).abs() < 0.5, "{}", found[0]);
    assert!(found[0]["location"][0].as_f64().unwrap() > 9.0, "{}", found[0]);
    assert!(found[0]["joinsAt"].as_f64().is_some_and(|z| (z - 20.0).abs() < 0.5), "{}", found[0]);
    assert!(!found[0]["reason"].as_str().unwrap_or_default().is_empty());
    assert_eq!(island["verdict"]["printability"], json!("obstacles"));
    assert!(island["verdict"]["problems"].to_string().contains("island"), "{}", island["verdict"]);

    // A lattice rooted in the bed: posts with short bridges between them.
    let lattice = supports_eval(&state, "union() {\n  for (x = [0:6:24]) translate([x,0,0]) cube([2,2,20]);\n  for (z = [6:6:18]) translate([0,0,z]) cube([26,2,1.5]);\n}");
    let supports = &lattice["objects"][0]["checks"]["supports"];
    assert_eq!(lattice["verdict"]["printability"], json!("clear-under-profile"), "{supports}");
    assert_eq!(lattice["verdict"]["printable"], json!(true));
    let _ = fs::remove_dir_all(root);
}

/// The report's soap dish: a stem and flared cone shell with an
/// intersecting grid subtracted leaves fragments that start in midair.
#[test]
fn supports_check_finds_midair_fragments_in_a_lattice_soap_dish() {
    let (state, root) = state();
    let code = "$fn = 48;\ndifference() {\n  union() {\n    cylinder(h = 12, r = 5);\n    translate([0,0,12]) cylinder(h = 20, r1 = 5, r2 = 30);\n  }\n  translate([0,0,12]) cylinder(h = 20.01, r1 = 3, r2 = 28);\n  for (i = [-30:6:30]) {\n    translate([i,0,24]) cube([3,80,10], center = true);\n    translate([0,i,24]) cube([80,3,10], center = true);\n    translate([i,0,16]) rotate([0,90,0]) cube([3,80,80], center = true);\n  }\n}\n";
    let dish = supports_eval(&state, code);
    let supports = &dish["objects"][0]["checks"]["supports"];
    assert!(supports.is_object(), "{dish}");
    assert!(supports["layers"].as_u64().unwrap() > 50, "{supports}");
    let counts = &supports["counts"];
    let problems = counts["islands"].as_u64().unwrap() + counts["cantilevers"].as_u64().unwrap();
    assert!(problems > 0, "{supports}");
    for issue in supports["issues"].as_array().unwrap() {
        assert!(issue["location"].as_array().is_some_and(|point| point.len() == 3), "{issue}");
        assert!(issue["firstLayer"].as_u64().is_some(), "{issue}");
    }
    assert_eq!(dish["verdict"]["printable"], json!(false));
    let printability = dish["verdict"]["printability"].as_str().unwrap();
    assert!(printability == "obstacles" || printability == "incomplete", "{}", dish["verdict"]);
    let _ = fs::remove_dir_all(root);
}

/// diff re-checks supports: adding a post under a cantilever resolves it.
#[test]
fn diff_rechecks_support_problems() {
    let (state, root) = state();
    let from = "union() { cube([4,10,10]); translate([0,0,10]) cube([12,10,2]); }";
    let to = "union() { cube([4,10,10]); translate([0,0,10]) cube([12,10,2]); translate([8,0,0]) cube([4,10,10]); }";
    let result = mcp(&state, "diff", json!({"designId": crate::scancommon::testkit::design_with_revisions(&state, from, to), "checks": ["supports"]}));
    let diff = &result["structuredContent"];
    let changes = &diff["objects"][0]["printabilityChanges"];
    assert!(changes["resolved"].as_array().unwrap().len() >= 1, "{changes}");
    assert_eq!(changes["remaining"], json!(0), "{changes}");
    assert!(diff["summary"]["text"].to_string().contains("support problems resolved"), "{}", diff["summary"]);
    // Without checks the diff is unchanged.
    let plain = mcp(&state, "diff", json!({"designId": crate::scancommon::testkit::design_with_revisions(&state, from, to)}));
    assert!(plain["structuredContent"]["objects"][0].get("printabilityChanges").is_none());
    let _ = fs::remove_dir_all(root);
}

/// Counts come from the scan's totals: an island past the 50 listed
/// issues (behind many ok bridges) still makes the model not printable.
#[test]
fn supports_verdict_counts_issues_beyond_the_truncated_list() {
    let (state, root) = state();
    let code = "union() {\n  for (i = [0:7], j = [0:6]) translate([i*9, j*9, 0]) {\n    cube([1,4,5]); translate([4,0,0]) cube([1,4,5]); translate([0,0,5]) cube([5,4,1]);\n  }\n  translate([0,70,8]) cube([4,4,4]);\n}\n";
    let value = supports_eval(&state, code);
    let supports = &value["objects"][0]["checks"]["supports"];
    assert!(supports["counts"]["bridges"].as_u64().unwrap() >= 50, "{}", supports["counts"]);
    assert!(supports["counts"]["islands"].as_u64().unwrap() >= 1, "{}", supports["counts"]);
    assert!(supports["issues"].as_array().unwrap().len() <= support::MAX_SUPPORT_ISSUES);
    assert_eq!(value["verdict"]["printable"], json!(false), "{}", value["verdict"]);
    assert_eq!(value["verdict"]["printability"], json!("obstacles"));
    assert!(value["verdict"]["problems"].to_string().contains("island"), "{}", value["verdict"]);
    if supports["truncated"].as_u64().unwrap() > 0 {
        assert!(value["verdict"]["supportNotes"].to_string().contains("not listed"), "{}", value["verdict"]);
    }
    let _ = fs::remove_dir_all(root);
}

/// Separate objects support each other: a slab object on two post
/// objects is a bridge, listed on the slab and on a group of them.
#[test]
fn supports_are_sliced_across_objects_and_groups() {
    let (state, root) = state();
    let code = "cube([2,10,10]); translate([8,0,0]) cube([2,10,10]); translate([0,0,10]) cube([10,10,2]);";
    let result = mcp(&state, "evaluate",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "intersections": false,
            "checks": ["supports"],
            "groups": {"table": ["object-1", "object-2", "object-3"]},
        }));
    let value = &result["structuredContent"];
    assert_eq!(value["verdict"]["printability"], json!("clear-under-profile"), "{}", value["verdict"]);
    let slab = &value["objects"][2]["checks"]["supports"];
    assert_eq!(slab["counts"]["islands"], json!(0), "{slab}");
    let bridges: Vec<&Value> = slab["issues"].as_array().unwrap().iter().filter(|issue| issue["kind"] == json!("bridge")).collect();
    assert!(!bridges.is_empty(), "{slab}");
    let group = &value["groups"][0]["checks"]["supports"];
    assert!(group["issues"].to_string().contains("bridge"), "{group}");
    let _ = fs::remove_dir_all(root);
}

/// Diff matching is one-to-one: two nearby islands before and one after
/// is one resolved, not zero.
#[test]
fn diff_matches_support_issues_one_to_one() {
    let (state, root) = state();
    let from = "union() { cube([4,10,20]); translate([8,0,8]) cube([1,10,12]); translate([10,0,8]) cube([1,10,12]); translate([0,0,20]) cube([14,10,2]); }";
    let to = "union() { cube([4,10,20]); translate([8,0,8]) cube([1,10,12]); translate([0,0,20]) cube([14,10,2]); }";
    let result = mcp(&state, "diff", json!({"designId": crate::scancommon::testkit::design_with_revisions(&state, from, to), "checks": ["supports"]}));
    let changes = &result["structuredContent"]["objects"][0]["printabilityChanges"];
    // Two islands side by side before, one after (plus whatever else
    // the slab has, unchanged on both sides).
    let resolved = changes["resolved"].as_array().unwrap();
    assert_eq!(resolved.len(), 1, "{changes}");
    assert_eq!(resolved[0]["kind"], json!("island"), "{changes}");
    assert!((resolved[0]["location"][0].as_f64().unwrap() - 10.5).abs() < 0.5, "{changes}");
    assert_eq!(changes["remaining"], changes["after"], "{changes}");
    assert_eq!(changes["introduced"], json!([]), "{changes}");
    let _ = fs::remove_dir_all(root);
}

fn ok_structured(result: &Value) -> &Value {
    assert_eq!(result["isError"], json!(false), "{result}");
    &result["structuredContent"]
}

#[test]
fn describe_geometry_finds_the_hole_of_a_drilled_cube() {
    let (state, root) = state();
    let code = "difference() { cube(20, center = true); cylinder(r = 4, h = 30, center = true, $fn = 96); }";
    let described = mcp(&state, "describe_geometry", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None)}));
    let object = &ok_structured(&described)["objects"][0];
    assert_eq!(object["id"], json!("object-1"), "{object}");
    assert_eq!(object["size"], json!([20.0, 20.0, 20.0]), "{object}");
    let hole = object["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|feature| feature["kind"] == json!("cylinder") && feature["hole"] == json!(true))
        .unwrap_or_else(|| panic!("no hole in {object}"));
    assert!(close(hole["radius"].as_f64().unwrap(), 4.0, 0.05), "{hole}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn check_insertion_reports_pass_collision_and_tolerance() {
    let (state, root) = state();
    let code = "peg = 9.6;\n\
             difference() { translate([-10,-10,0]) cube([20,20,20]); translate([-5,-5,-1]) cube([10,10,22]); }\n\
             translate([-peg/2,-peg/2,25]) cube([peg,peg,10]);";
    let passed_reply = mcp(
        &state,
        "check_insertion",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "moving": "object-2",
            "direction": [0, 0, -1],
            "distance": 40,
            "tolerance": 0.15,
        }));
    let passed = ok_structured(&passed_reply);
    assert_eq!(passed["passes"], json!(true), "{passed}");
    assert_eq!(passed["clearanceOk"], json!(true), "{passed}");
    assert_eq!(passed["fixed"], json!(["object-1"]));
    assert!(close(passed["minClearance"]["clearance"].as_f64().unwrap(), 0.2, 1e-6), "{passed}");
    assert!(passed.get("collision").is_none(), "nulls omitted: {passed}");
    let tight_reply = mcp(
        &state,
        "check_insertion",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "moving": "object-2",
            "path": [[0,0,0],[0,0,-40]],
            "tolerance": 0.3,
        }));
    let tight = ok_structured(&tight_reply);
    assert_eq!(tight["passes"], json!(true), "{tight}");
    assert_eq!(tight["clearanceOk"], json!(false), "{tight}");
    let travel = tight["firstViolation"]["travel"].as_f64().unwrap();
    assert!(travel > 4.6 && travel < 5.01, "{tight}");
    let blocked_reply = mcp(
        &state,
        "check_insertion",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), Some(&json!({"peg": 10.2}))),
            "moving": "object-2",
            "direction": [0, 0, -1],
            "distance": 40,
        }));
    let blocked = ok_structured(&blocked_reply);
    assert_eq!(blocked["passes"], json!(false), "{blocked}");
    assert_eq!(blocked["clearanceOk"], json!(false), "{blocked}");
    let collision = &blocked["collision"];
    assert!(close(collision["travel"].as_f64().unwrap(), 5.0, 1e-3), "{collision}");
    assert!(close(collision["pointB"][2].as_f64().unwrap(), 20.0, 1e-3), "{collision}");
    assert!(collision["penetration"]["depth"].as_f64().unwrap() > 0.09, "{collision}");
    let bad = mcp(&state, "check_insertion", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "moving": "object-9", "direction": [0,0,-1], "distance": 1}));
    assert_eq!(bad["isError"], json!(true), "{bad}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn probe_reads_a_cube_top_and_signed_distances() {
    let (state, root) = state();
    let code = "cube([10, 10, 7]); translate([20, 0, 0]) cube(2);";
    let probed = mcp(
        &state,
        "probe",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "rays": [
                {"origin": [5, 5, 50], "direction": [0, 0, -2]},
                {"origin": [5, 5, 50], "direction": [0, 0, 1]}
            ],
            "points": [[5, 5, 5], [5, 5, 9], [21, 1, 4]],
        }));
    let body = ok_structured(&probed);
    let hit = &body["rays"][0];
    assert_eq!(hit["hit"], json!(true), "{hit}");
    assert_eq!(hit["objectId"], json!("object-1"));
    assert!(close(hit["point"][2].as_f64().unwrap(), 7.0, 1e-9), "{hit}");
    assert!(close(hit["distance"].as_f64().unwrap(), 43.0, 1e-9), "{hit}");
    assert_eq!(hit["normal"], json!([0.0, 0.0, 1.0]));
    assert_eq!(hit["entering"], json!(true));
    assert_eq!(body["rays"][1]["hit"], json!(false));
    let inside = &body["points"][0];
    assert_eq!(inside["inside"], json!(true), "{inside}");
    assert!(close(inside["distance"].as_f64().unwrap(), -2.0, 1e-9), "{inside}");
    let above = &body["points"][1];
    assert!(close(above["distance"].as_f64().unwrap(), 2.0, 1e-9), "{above}");
    assert_eq!(above["closest"], json!([5.0, 5.0, 7.0]));
    assert_eq!(body["points"][2]["objectId"], json!("object-2"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn section_profile_of_a_cylinder_is_a_circle() {
    let (state, root) = state();
    let code = "cylinder(r = 5, h = 10, $fn = 128);";
    let cut = mcp(
        &state,
        "section_profile",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "plane": {"axis": "z", "offset": 5},
            "tolerance": 0,
            "scad": true,
        }));
    let body = ok_structured(&cut);
    let object = &body["objects"][0];
    let loops = object["loops"].as_array().unwrap();
    assert_eq!(loops.len(), 1, "{object}");
    assert_eq!(loops[0]["hole"], json!(false));
    let area = object["area"].as_f64().unwrap();
    assert!(close(area, std::f64::consts::PI * 25.0, 0.3), "{area}");
    assert!(body["scad"].as_str().unwrap().contains("polygon("), "{body}");
    // A tiny point budget forces coarser simplification, reported.
    let coarse = mcp(
        &state,
        "section_profile",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "plane": {"point": [0, 0, 5], "normal": [0, 0, 1]},
            "maxPoints": 20,
        }));
    let coarse = ok_structured(&coarse);
    assert!(coarse["totalPoints"].as_u64().unwrap() <= 20, "{coarse}");
    assert_eq!(coarse["simplified"], json!(true));
    let _ = fs::remove_dir_all(root);
}


fn vec_of(value: &Value) -> [f64; 3] {
    [0, 1, 2].map(|axis| value[axis].as_f64().unwrap())
}

fn apply_json(matrix: &Value, p: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|row| (0..3).map(|k| matrix[row][k].as_f64().unwrap() * p[k]).sum::<f64>() + matrix[row][3].as_f64().unwrap())
}

#[test]
fn section_profile_reports_centroids() {
    let (state, root) = state();
    let code = "translate([3, -2, 0]) cylinder(r = 5, h = 10, $fn = 64);";
    let cut = mcp(&state, "section_profile", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "plane": {"axis": "z", "offset": 5}}));
    let object = &ok_structured(&cut)["objects"][0];
    let first = &object["loops"][0];
    assert_eq!(first["centroid"].as_array().unwrap().len(), 2, "{object}");
    assert_eq!(first["centroid3d"].as_array().unwrap().len(), 3, "{object}");
    assert!(object["area"].as_f64().unwrap() > 70.0, "{object}");
    // The centroids sit on the cylinder axis.
    let centroid = vec_of(&object["centroid"]);
    assert!(close(centroid[0], 3.0, 0.05) && close(centroid[1], -2.0, 0.05) && close(centroid[2], 5.0, 1e-6), "{object}");
    assert!(close(first["centroid3d"][0].as_f64().unwrap(), 3.0, 0.05), "{object}");
    assert!(close(first["centroid3d"][1].as_f64().unwrap(), -2.0, 0.05), "{object}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn fit_axis_tool_is_wired() {
    let (state, root) = state();
    // A tapered body tilted in XZ: its centerline runs along (sin 30, 0, cos 30).
    let code = "rotate([0, 30, 0]) cylinder(r1 = 8, r2 = 3, h = 60, $fn = 48);";
    let result = mcp(&state, "fit_axis", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "sections": 16}));
    {
        let body = ok_structured(&result);
        let object = &body["objects"][0];
        let direction = vec_of(&object["direction"]);
        let expected = [0.5, 0.0, 3f64.sqrt() / 2.0];
        let alignment = (0..3).map(|axis| direction[axis] * expected[axis]).sum::<f64>().abs();
        assert!(alignment > 0.999, "{object}");
        assert!(object["alignMatrix"].as_array().unwrap().len() == 4, "{object}");
        assert!(object["scad"].as_str().unwrap().starts_with("multmatrix("), "{object}");
        assert!(body["hint"].as_str().unwrap().contains("align_to"));
    }
    // A body with no geometry to slice surfaces a clean tool error.
    let empty = mcp(&state, "fit_axis", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(0);"), None)}));
    assert_eq!(empty["isError"], json!(true), "{empty}");
    let invalid = mcp(&state, "fit_axis", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "sections": 500}));
    assert_eq!(invalid["isError"], json!(true));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn align_to_maps_directions() {
    // X onto Z exactly.
    let value = align_to_value(&json!({"pairs": [{"from": [1, 0, 0], "to": [0, 0, 1]}]})).unwrap();
    let mapped = apply_json(&value["matrix"], [1.0, 0.0, 0.0]);
    assert!(close(mapped[2], 1.0, 1e-9) && close(mapped[0], 0.0, 1e-9), "{value}");
    assert!(close(value["angleDeg"].as_f64().unwrap(), 90.0, 1e-6));
    assert_eq!(value["rotateVerified"], json!(true));
    assert!(value["scad"]["rotate"].as_str().unwrap().starts_with("rotate(["));

    // Two pairs: primary exact, a non-orthogonal secondary projected.
    let value = align_to_value(&json!({"pairs": [
        {"from": [0, 0, 1], "to": [1, 0, 0]},
        {"from": [1, 0, 1], "to": [0, 1, 0]},
    ]}))
    .unwrap();
    let primary = apply_json(&value["matrix"], [0.0, 0.0, 1.0]);
    assert!(close(primary[0], 1.0, 1e-9), "{value}");
    let secondary = apply_json(&value["matrix"], [1.0, 0.0, 0.0]);
    assert!(close(secondary[1], 1.0, 1e-9), "{value}");
    assert!(close(value["residualDeg"].as_f64().unwrap(), 45.0, 1e-6), "{value}");

    // Antiparallel: a 180 degree turn that still maps exactly.
    let value = align_to_value(&json!({"pairs": [{"from": [0, 0, 1], "to": [0, 0, -1]}]})).unwrap();
    let flipped = apply_json(&value["matrix"], [0.0, 0.0, 1.0]);
    assert!(close(flipped[2], -1.0, 1e-9), "{value}");
    assert!(close(value["angleDeg"].as_f64().unwrap(), 180.0, 1e-6));

    // The Euler angles reproduce the matrix for an arbitrary rotation, with a pivot.
    let value = align_to_value(&json!({
        "pairs": [{"from": [0.3, -0.5, 0.8], "to": [-0.2, 0.9, 0.1]}, {"from": [1, 0, 0], "to": [0, 0, 1]}],
        "pivot": [5, 6, 7],
    }))
    .unwrap();
    assert_eq!(value["rotateVerified"], json!(true), "{value}");
    let euler = vec_of(&value["rotate"]);
    let rebuilt = rotation_from_euler(euler);
    for row in 0..3 {
        for column in 0..3 {
            assert!(close(rebuilt[row][column], value["matrix"][row][column].as_f64().unwrap(), 1e-4), "{value}");
        }
    }
    let pivot = apply_json(&value["matrix"], [5.0, 6.0, 7.0]);
    assert!(close(pivot[0], 5.0, 1e-5) && close(pivot[1], 6.0, 1e-5) && close(pivot[2], 7.0, 1e-5), "{value}");
    // Gimbal lock (Y by 90 degrees) still round-trips.
    let value = align_to_value(&json!({"pairs": [{"from": [0, 0, 1], "to": [1, 0, 0]}, {"from": [0, 1, 0], "to": [0, 0, 1]}]})).unwrap();
    assert_eq!(value["rotateVerified"], json!(true), "{value}");

    // Around the singularity and at random: the angles, as serialised,
    // rebuild the rotation that was asked for.
    let mut seed = 0x1234_5678_9abc_def1u64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut cases: Vec<P> = Vec::new();
    for ay in [89.9, 89.99, 89.999, 89.9999999, 90.0, 90.000001, 90.001, 90.1, -89.999, -90.0, -90.001] {
        for (ax, az) in [(90.0, 0.0), (0.0, 0.0), (33.0, -71.0), (-120.0, 45.0)] {
            cases.push([ax, ay, az]);
        }
    }
    for _ in 0..500 {
        cases.push([random() * 360.0 - 180.0, random() * 180.0 - 90.0, random() * 360.0 - 180.0]);
    }
    for angles in cases {
        let rotation = rotation_from_euler(angles);
        let columns = [0, 1, 2].map(|column| [0, 1, 2].map(|row| rotation[row][column]));
        let value = align_to_value(&json!({"pairs": [
            {"from": [1, 0, 0], "to": columns[0]},
            {"from": [0, 1, 0], "to": columns[1]},
        ]}))
        .unwrap();
        assert_eq!(value["rotateVerified"], json!(true), "{angles:?}: {value}");
        let rebuilt = rotation_from_euler(vec_of(&value["rotate"]));
        assert!(rotation_error(&rebuilt, &rotation) < 1e-6, "{angles:?}: {value}");
        // The snippet carries the same angles.
        let snippet = value["scad"]["rotate"].as_str().unwrap();
        let inner = &snippet[snippet.find("rotate([").unwrap() + 8..];
        let written: Vec<f64> = inner[..inner.find(']').unwrap()].split(", ").map(|v| v.parse().unwrap()).collect();
        assert!(rotation_error(&rotation_from_euler([written[0], written[1], written[2]]), &rotation) < 1e-6, "{snippet}");
    }
    // The old extraction's failure, by name: not rotate([0, 89.999, -90]).
    let rotation = rotation_from_euler([90.0, 89.999, 0.0]);
    let euler = euler_xyz(&rotation);
    assert!(rotation_error(&rotation_from_euler(euler), &rotation) < 1e-9, "{euler:?}");

    assert!(align_to_value(&json!({"pairs": [{"from": [0, 0, 0], "to": [1, 0, 0]}]})).is_err());
    assert!(align_to_value(&json!({"pairs": [{"from": [1, 0, 0], "to": [1, 0, 0]}, {"from": [2, 0, 0], "to": [0, 1, 0]}]})).is_err());
    assert!(align_to_value(&json!({"pairs": []})).is_err());
}

#[test]
fn place_centres_and_grounds_a_rotated_cube_exactly() {
    let (state, root) = state();
    let code = "translate([30, -12, 7]) cube(10);";
    let turn = align_to_value(&json!({"pairs": [{"from": [1, 1, 1], "to": [0, 0, 1]}]})).unwrap();
    let result = mcp(&state, "place", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "matrix": turn["matrix"]}));
    let body = ok_structured(&result);
    let after = &body["boundsAfter"];
    let (low, high) = (vec_of(&after["min"]), vec_of(&after["max"]));
    assert!(close(low[2], 0.0, 1e-6), "{body}");
    // Rounded to 4 decimals in the reply.
    assert!(close(low[0] + high[0], 0.0, 2e-4) && close(low[1] + high[1], 0.0, 2e-4), "{body}");
    // The cube's diagonal now stands upright: its height is the full diagonal.
    assert!(close(high[2], 10.0 * 3f64.sqrt(), 1e-3), "{body}");
    // Exact vertex bounds, not a transformed box: every corner lands inside and touches them.
    let corners: Vec<[f64; 3]> = (0..8)
        .map(|index| [0, 1, 2].map(|axis| if index & (1 << axis) != 0 { 10.0 } else { 0.0 }))
        .map(|p| apply_json(&body["matrix"], [p[0] + 30.0, p[1] - 12.0, p[2] + 7.0]))
        .collect();
    let min_z = corners.iter().map(|p| p[2]).fold(f64::INFINITY, f64::min);
    assert!(close(min_z, 0.0, 1e-3), "{body}");
    assert!(body["scad"]["translate"].as_str().unwrap().starts_with("translate(["), "{body}");
    // centre: none keeps X/Y and only grounds.
    let grounded = mcp(&state, "place", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "center": "none"}));
    let grounded = ok_structured(&grounded);
    assert_eq!(grounded["translate"], json!([0.0, 0.0, -7.0]), "{grounded}");
    let moved = mcp(&state, "place", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None), "on": [100, 50], "ground": false}));
    assert_eq!(ok_structured(&moved)["translate"], json!([65.0, 57.0, 0.0]));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn render_image_views_returns_one_tiled_sheet() {
    let (state, root) = state();
    let result = mcp(
        &state,
        "render_image",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube([10, 6, 4]);"), None),
            "views": ["front", "right", "top", "iso"],
        }));
    assert_eq!(result["isError"], json!(false), "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.iter().filter(|block| block["type"] == json!("image")).count(), 1);
    let png = base64_decode(content[0]["data"].as_str().unwrap()).unwrap();
    let dimension = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
    assert_eq!((dimension(16), dimension(20)), (800, 640));
    let render = &result["structuredContent"]["render"];
    assert_eq!(render["columns"], json!(2), "{render}");
    let tiles = render["tiles"].as_array().unwrap();
    assert_eq!(tiles.len(), 4);
    assert_eq!(tiles[1]["view"], json!("right"));
    assert_eq!((tiles[3]["x"].clone(), tiles[3]["y"].clone()), (json!(400), json!(320)));
    // Too many views, or views with an explicit eye, are refused.
    let refused = mcp(
        &state,
        "render_image",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(1);"), None),
            "views": ["front", "back", "left", "right", "top", "bottom", "iso"],
        }));
    assert_eq!(refused["isError"], json!(true));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn check_intersections_locates_too_tight_regions() {
    let (state, root) = state();
    let code = "cube(10); translate([10.1, 0, 0]) cube(10);";
    let checked = mcp(
        &state,
        "check_intersections",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None),
            "tolerance": 0.2,
        }));
    let body = ok_structured(&checked);
    let pair = &body["intersections"][0];
    assert_eq!(pair["relation"], json!("clearanceViolation"), "{pair}");
    let regions = pair["clearanceRegions"].as_array().unwrap();
    assert_eq!(regions.len(), 1, "{pair}");
    let region = &regions[0];
    assert!(close(region["minClearance"].as_f64().unwrap(), 0.1, 1e-6), "{region}");
    assert!(close(region["center"][0].as_f64().unwrap(), 10.05, 0.06), "{region}");
    // The facing faces plus thin strips of the side faces near the gap.
    assert!(close(region["area"].as_f64().unwrap(), 104.0, 4.0), "{region}");
    assert_eq!(region["overlapping"], json!(false));
    // A pair that clears the tolerance carries no regions.
    let clear = mcp(
        &state,
        "check_intersections",
        json!({
            "designId": crate::scancommon::testkit::save_revision(&state, None, Some("cube(10); translate([11, 0, 0]) cube(10);"), None),
            "tolerance": 0.2,
        }));
    assert!(ok_structured(&clear)["intersections"][0].get("clearanceRegions").is_none());
    let _ = fs::remove_dir_all(root);
}

/// MCP replies stay lean: no per-reply semantics text, no null or
/// default fields on pairs, and compact writes.
#[test]
fn mcp_replies_are_lean() {
    let (state, root) = state();
    let code = "g = 1;\ncube(10);\ntranslate([9,0,0]) cube(10);\ntranslate([20+g,0,0]) cube(10);\n";
    let text = |value: &Value| value["content"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().to_string();
    let created = mcp(&state, "create_design", json!({"code": code, "revisionName": "Test revision"}));
    let id = created["structuredContent"]["designId"].as_str().unwrap().to_string();
    assert_eq!(created["structuredContent"]["objectsShown"], json!("all"), "{created}");
    assert!(created["structuredContent"]["objects"][0].get("color").is_none(), "{created}");

    let checked = mcp(&state, "check_intersections", json!({"designId": id}));
    let structured = &checked["structuredContent"];
    assert!(structured.get("semantics").is_none() && structured.get("method").is_none(), "{structured}");
    let pairs = structured["intersections"].as_array().unwrap();
    assert_eq!(pairs[1], json!({"a": "object-1", "b": "object-3", "relation": "clear", "clearance": 11.0}));
    assert!(pairs.iter().all(|pair| pair.as_object().unwrap().values().all(|value| !value.is_null())), "{structured}");
    assert!(structured["summary"].get("contacts").is_none() && structured["summary"]["toleranceViolations"] == json!(1));
    assert!(text(&checked).len() < 2500, "{}", text(&checked).len());
    let verbose = mcp(&state, "check_intersections", json!({"designId": id, "verbose": true}));
    assert!(verbose["structuredContent"]["semantics"].is_string());

    let evaluated = mcp(&state, "evaluate", json!({"designId": id}));
    assert!(evaluated["structuredContent"]["intersections"].get("semantics").is_none());
    assert_eq!(evaluated["structuredContent"]["intersections"]["intersections"][2]["relation"], json!("clear"));

    let updated = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": 1, "edits": [{"search": "g = 1;", "replace": "g = 2;"}], "revisionName": "Test revision"}));
    let reply = &updated["structuredContent"];
    assert_eq!(reply["revision"], json!(2), "{reply}");
    assert_eq!(reply["objectsShown"], json!("changed"), "{reply}");
    assert_eq!(reply["objectCount"], json!(3));
    assert_eq!(reply["objectsUnchanged"], json!(2));
    assert_eq!(reply["objects"].as_array().unwrap().len(), 1);
    assert_eq!(reply["objects"][0]["id"], json!("object-3"));
    assert_eq!(reply["objects"][0]["bounds"]["min"], json!([22.0, 0.0, 0.0]));
    let full = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": 2, "objects": "full", "edits": [{"search": "g = 2;", "replace": "g = 3;"}], "revisionName": "Test revision"}));
    assert_eq!(full["structuredContent"]["objects"].as_array().unwrap().len(), 3, "{full}");
    assert!(full["structuredContent"]["objects"][0].get("selectable").is_some());
    let removed = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": 3, "edits": [{"search": "translate([20+g,0,0]) cube(10);\n", "replace": ""}], "revisionName": "Test revision"}));
    assert_eq!(removed["structuredContent"]["objectsRemoved"], json!(["object-3"]), "{removed}");
    assert_eq!(removed["structuredContent"]["objects"], json!([]));

    let upload = mcp(&state, "create_asset_upload", json!({"designId": id, "name": "a.stl"}));
    assert!(upload["structuredContent"].get("chunkedScript").is_none() && upload["structuredContent"]["curl"].is_string());
    let big = mcp(&state, "create_asset_upload", json!({"designId": id, "name": "a.stl", "bytes": 60_000_000}));
    assert!(big["structuredContent"]["chunkedScript"].is_string());
    let asked = mcp(&state, "create_asset_upload", json!({"designId": id, "name": "a.stl", "includeScript": true}));
    assert!(asked["structuredContent"]["chunkedScript"].is_string());
    eprintln!(
        "SIZES check={} evaluate={} update={} upload={} uploadScript={}",
        text(&checked).len(), text(&evaluated).len(), text(&updated).len(), text(&upload).len(), text(&asked).len()
    );
    let _ = fs::remove_dir_all(root);
}

// ---- Revisions: the agent's surface only reaches the current revision ----

fn raw(state: &AppState, tool: &str, arguments: Value) -> Value {
    mcp_call_tool(state, &mcp_request(), tool, arguments)
}

fn error_text(result: &Value) -> String {
    assert_eq!(result["isError"], json!(true), "{result}");
    result["structuredContent"]["error"].as_str().unwrap().to_string()
}

fn structured(result: &Value) -> Value {
    assert_eq!(result["isError"], json!(false), "{result}");
    result["structuredContent"].clone()
}

fn named_design(state: &AppState, code: &str, name: &str) -> String {
    let created = structured(&raw(state, "create_design", json!({"code": code, "revisionName": name})));
    created["designId"].as_str().unwrap().to_string()
}

fn revision_of(state: &AppState, id: &str) -> u64 {
    state.workspaces.get(id).unwrap().unwrap().revision
}

#[test]
fn revision_names_are_trimmed_and_bounded() {
    assert_eq!(crate::validate_revision_name("  Hinge  ").unwrap(), "Hinge");
    assert_eq!(crate::validate_revision_name(&"é".repeat(40)).unwrap().chars().count(), 40);
    assert!(crate::validate_revision_name(&"é".repeat(41)).unwrap_err().contains("41"));
    assert!(crate::validate_revision_name(" \t\n ").is_err());
    assert_eq!(crate::validate_revision_name("a\nb").unwrap(), "a b");
    assert_eq!(crate::validate_revision_name("a\u{2028}b").unwrap(), "a b");
    assert_eq!(crate::validate_revision_name("\u{202e}abc\u{200b}").unwrap(), "abc");
    assert!(crate::validate_revision_name("\u{200b}\u{feff}").is_err());
}

#[test]
fn every_write_tool_requires_a_valid_name_and_saves_nothing_without_one() {
    let (state, root) = state();
    let id = named_design(&state, "w = 1;\ncube(w);\n", "First");
    let long = "x".repeat(41);
    let calls = [
        ("update_design_code", json!({"designId": id, "expectedRevision": 1, "code": "cube(2);\n"})),
        ("update_workspace_code", json!({"workspaceId": id, "expectedRevision": 1, "code": "cube(2);\n"})),
        ("set_parameters", json!({"designId": id, "expectedRevision": 1, "values": {"w": 2}})),
        ("set_text", json!({"designId": id, "expectedRevision": 1, "text": "Hi", "size": 10})),
        ("set_print_orientation", json!({"designId": id, "expectedRevision": 1, "object": "object-1", "face": "-z"})),
        ("restore_revision", json!({"designId": id, "expectedRevision": 1, "revision": 1})),
        ("evaluate", json!({"designId": id, "expectedRevision": 1, "code": "cube(2);\n"})),
        ("create_design", json!({"code": "cube(2);\n"})),
        ("create_workspace", json!({"code": "cube(2);\n"})),
    ];
    for (tool, arguments) in calls {
        assert!(error_text(&raw(&state, tool, arguments.clone())).contains("revisionName"), "{tool}");
        for bad in [json!("   "), json!(long), json!(7)] {
            let mut named = arguments.clone();
            named["revisionName"] = bad;
            assert!(error_text(&raw(&state, tool, named)).contains("revisionName"), "{tool}");
        }
        assert_eq!(revision_of(&state, &id), 1, "{tool} wrote without a name");
    }
    let accepted = structured(&raw(&state, "update_design_code", json!({"designId": id, "expectedRevision": 1, "code": "cube(2);\n", "revisionName": format!("  {}  ", "n".repeat(40))})));
    assert_eq!(accepted["revisionName"], json!("n".repeat(40)));
    let _ = fs::remove_dir_all(root);
}

/// What `tools/list` offers and what the server accepts are one list, and no
/// analysis, render or export tool has a way left to look at other geometry.
#[test]
fn no_tool_can_analyse_anything_but_the_current_revision() {
    let (state, root) = state();
    let id = named_design(&state, "w = 1;\ncube(w);\n", "First");
    // Who still takes which of those arguments, and why: writes take source,
    // get_design_code reads old text, restore_revision names what it copies,
    // and these two `sweep`s lay out section planes on the current revision.
    let allowed = [
        ("code", "create_design"),
        ("code", "update_design_code"),
        ("code", "evaluate"),
        ("revision", "get_design_code"),
        ("revision", "restore_revision"),
        ("sweep", "compare_sections"),
    ];
    let tools = crate::canonical_mcp_tool_definitions();
    assert!(tools.len() > 40, "{}", tools.len());
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        // Comments on the model read no geometry.
        if crate::feedback::is_feedback_tool(name) {
            continue;
        }
        for key in crate::REMOVED_ARGUMENTS {
            let listed = tool["inputSchema"]["properties"].get(key).is_some();
            assert_eq!(listed, allowed.contains(&(key, name)), "{name} and {key}");
            if listed {
                continue;
            }
            let value = match key {
                "overrides" => json!({"w": 2}),
                "revision" => json!(1),
                "sweep" => json!({"name": "w", "values": [1, 2]}),
                "variants" => json!([{"w": 2}]),
                _ => json!("cube(1);"),
            };
            let result = raw(&state, name, json!({"designId": id, key: value}));
            let error = error_text(&result);
            assert!(error.contains(key) && (error.contains("restore_revision") || error.contains("revision")), "{name} {key}: {error}");
        }
    }
    // An explicit null is the same as leaving it out.
    assert_eq!(raw(&state, "measure", json!({"designId": id, "overrides": null}))["isError"], json!(false));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn analysis_tools_need_a_design() {
    let (state, root) = state();
    for tool in ["evaluate", "measure", "probe", "render_image", "render_design", "check_intersections", "check_insertion",
        "describe_geometry", "section_profile", "fit_axis", "place", "diff", "get_parameters", "list_modules", "list_design_objects",
        "compare_surfaces", "compare_sections", "measure_between", "get_print_layout", "export_print_parts", "split_object"]
    {
        assert!(error_text(&raw(&state, tool, json!({}))).contains("designId"), "{tool}");
        let required = crate::mcp_tool_schema(tool).unwrap()["required"].as_array().unwrap().clone();
        assert!(required.contains(&json!("designId")), "{tool}");
    }
    assert_eq!(raw(&state, "align_to", json!({"pairs": [{"from": [1, 0, 0], "to": [0, 1, 0]}]}))["isError"], json!(false));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn evaluate_saves_a_named_revision_first_and_answers_about_it() {
    let (state, root) = state();
    let id = named_design(&state, "cube(10);\n", "First");
    let reply = structured(&raw(&state, "evaluate", json!({"designId": id, "code": "cube(20);\n", "revisionName": "Bigger", "expectedRevision": 1})));
    assert_eq!(reply["revision"], json!(2));
    assert_eq!(reply["saved"]["revision"], json!(2));
    assert_eq!(reply["saved"]["name"], json!("Bigger"));
    assert_eq!(reply["objects"][0]["bounds"]["max"][0], json!(20.0));
    let stored = state.workspaces.get(&id).unwrap().unwrap();
    assert_eq!(stored.code, "cube(20);\n");
    assert_eq!(stored.revision_name, "Bigger");
    assert_eq!(stored.revision_source, "ai");

    // edits, against the source just saved.
    let edited = structured(&raw(&state, "evaluate", json!({"designId": id, "edits": [{"search": "20", "replace": "30"}], "revisionName": "Biggest", "expectedRevision": 2})));
    assert_eq!(edited["revision"], json!(3));
    assert_eq!(edited["objects"][0]["bounds"]["max"][0], json!(30.0));

    // Without them it evaluates the current revision and saves nothing.
    let plain = structured(&raw(&state, "evaluate", json!({"designId": id})));
    assert_eq!(plain["revision"], json!(3));
    assert!(plain.get("saved").is_none());
    assert_eq!(revision_of(&state, &id), 3);

    // The same rules as update_design_code: all or nothing.
    assert!(error_text(&raw(&state, "evaluate", json!({"designId": id, "code": "cube(1);", "revisionName": "x"}))).contains("expectedRevision"));
    assert!(error_text(&raw(&state, "evaluate", json!({"designId": id, "code": "cube(1);", "edits": [], "revisionName": "x", "expectedRevision": 3}))).contains("not both"));
    assert!(error_text(&raw(&state, "evaluate", json!({"designId": id, "revisionName": "x"}))).contains("code or edits"));
    assert!(error_text(&raw(&state, "evaluate", json!({"designId": id, "code": "cube(1);", "revisionName": "x", "expectedRevision": 1}))).contains("conflict"));
    let rejected = structured(&raw(&state, "evaluate", json!({"designId": id, "code": "cube(;", "revisionName": "Broken", "expectedRevision": 3})));
    assert_eq!(rejected["valid"], json!(false));
    assert_eq!(revision_of(&state, &id), 3);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_retried_evaluate_write_replays_instead_of_conflicting() {
    let (state, root) = state();
    let id = named_design(&state, "cube(10);\n", "First");
    let call = json!({"designId": id, "code": "cube(20);\n", "revisionName": "Bigger", "expectedRevision": 1});
    structured(&raw(&state, "evaluate", call.clone()));
    let replay = structured(&raw(&state, "evaluate", call));
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(revision_of(&state, &id), 2);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn list_restore_read_and_diff_work_on_stored_revisions() {
    let (state, root) = state();
    let id = named_design(&state, "cube(10);\n", "First");
    structured(&raw(&state, "update_design_code", json!({"designId": id, "expectedRevision": 1, "code": "cube(20);\n", "revisionName": "Second"})));

    // Newest first, compact, with who wrote what.
    let listed = structured(&raw(&state, "list_revisions", json!({"designId": id})));
    assert_eq!(listed["current"], json!(2));
    assert_eq!(listed["total"], json!(2));
    let rows = listed["revisions"].as_array().unwrap();
    assert_eq!(rows[0]["revision"], json!(2));
    assert_eq!(rows[0]["name"], json!("Second"));
    assert_eq!(rows[0]["source"], json!("agent"));
    assert_eq!(rows[0]["current"], json!(true));
    assert_eq!(rows[1]["revision"], json!(1));
    assert_eq!(rows[1]["name"], json!("First"));
    assert_eq!(rows[1]["source"], json!("agent"));
    assert!(rows[1].get("current").is_none());
    assert!(rows[1]["timestamp"].as_u64().unwrap() > 0);
    let page = structured(&raw(&state, "list_revisions", json!({"designId": id, "limit": 1, "before": 2})));
    assert_eq!(page["revisions"].as_array().unwrap().len(), 1);
    assert_eq!(page["revisions"][0]["revision"], json!(1));

    // Old source reads as text, marked as not current.
    let old = structured(&raw(&state, "get_design_code", json!({"designId": id, "revision": 1})));
    assert_eq!(old["revision"], json!(1));
    assert_eq!(old["currentRevision"], json!(2));
    assert!(old["code"].as_str().unwrap().contains("cube(10)"));
    assert!(error_text(&raw(&state, "get_design_code", json!({"designId": id, "revision": 9}))).contains("does not exist"));

    // diff: defaults, and explicit revisions.
    let diff = structured(&raw(&state, "diff", json!({"designId": id})));
    assert_eq!(diff["from"]["revision"], json!(1));
    assert_eq!(diff["to"]["revision"], json!(2));
    assert_eq!(diff["summary"]["changed"], json!(1));
    let backwards = structured(&raw(&state, "diff", json!({"designId": id, "fromRevision": 2, "toRevision": 1})));
    assert_eq!(backwards["to"]["revision"], json!(1));
    assert!(error_text(&raw(&state, "diff", json!({"designId": id, "fromRevision": 7}))).contains("does not exist"));
    let lone = named_design(&state, "cube(1);\n", "Only");
    assert!(error_text(&raw(&state, "diff", json!({"designId": lone}))).contains("fromRevision"));

    // Restore copies on top, and history keeps both.
    let restored = structured(&raw(&state, "restore_revision", json!({"designId": id, "revision": 1, "revisionName": "Back to the first", "expectedRevision": 2})));
    assert_eq!(restored["revision"], json!(3));
    assert_eq!(restored["restoredFrom"], json!(1));
    assert_eq!(restored["revisionName"], json!("Back to the first"));
    let stored = state.workspaces.get(&id).unwrap().unwrap();
    assert_eq!(stored.code, "cube(10);\n");
    assert_eq!(state.workspaces.revision_code(&id, 2).unwrap().as_deref(), Some("cube(20);\n"));
    assert_eq!(structured(&raw(&state, "list_revisions", json!({"designId": id})))["total"], json!(3));
    assert!(error_text(&raw(&state, "restore_revision", json!({"designId": id, "revision": 2, "revisionName": "x", "expectedRevision": 1}))).contains("conflict"));
    assert!(error_text(&raw(&state, "restore_revision", json!({"designId": id, "revision": 99, "revisionName": "x", "expectedRevision": 3}))).contains("does not exist"));
    let _ = fs::remove_dir_all(root);
}

/// Stored data from before names existed, read as it lies on disk.
#[test]
fn revisions_stored_without_names_still_load_and_restore() {
    let (state, root) = state();
    let id = named_design(&state, "cube(30);\n", "First");
    let document = root.join(format!("{id}.json"));
    let mut stored: Value = serde_json::from_slice(&fs::read(&document).unwrap()).unwrap();
    let fields = stored.as_object_mut().unwrap();
    fields.remove("revisionName");
    fields.remove("revisionSource");
    fields.insert("revision".into(), json!(5));
    fields.insert("events".into(), json!([{"workspaceId": id, "revision": 4, "source": "ai", "start": 0, "end": 0, "insertedLength": 0, "timestamp": 40}]));
    fs::write(&document, serde_json::to_vec(&stored).unwrap()).unwrap();
    fs::write(
        root.join(format!("{id}.history")),
        serde_json::to_vec(&json!([
            {"revision": 3, "code": "cube(3);\n", "timestamp": 30},
            {"revision": 4, "code": "cube(4);\n", "timestamp": 40},
        ]))
        .unwrap(),
    )
    .unwrap();
    // A fresh store, as after a restart: nothing cached.
    let store = crate::WorkspaceStore::open(root.clone()).unwrap();
    let state = AppState { workspaces: crate::Store::Filesystem(store), ..state };
    let listed = structured(&raw(&state, "list_revisions", json!({"designId": id})));
    let rows = listed["revisions"].as_array().unwrap();
    assert_eq!(rows[0]["name"], json!("Revision 5"));
    assert_eq!(rows[0]["source"], json!("unknown"));
    assert_eq!(rows[1]["name"], json!("Revision 4"));
    assert_eq!(rows[1]["source"], json!("agent"), "from the edit log");
    assert_eq!(rows[2]["name"], json!("Revision 3"));
    assert_eq!(rows[2]["source"], json!("unknown"));
    let restored = structured(&raw(&state, "restore_revision", json!({"designId": id, "revision": 3, "revisionName": "Restore: Revision 3", "expectedRevision": 5})));
    assert_eq!(restored["revision"], json!(6));
    let rows = structured(&raw(&state, "list_revisions", json!({"designId": id})))["revisions"].clone();
    assert_eq!(rows[0]["name"], json!("Restore: Revision 3"));
    assert_eq!(rows[1]["revision"], json!(5));
    assert_eq!(rows[1]["name"], json!("Revision 5"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn history_is_bounded_by_count_and_bytes() {
    let (state, root) = state();
    let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
    let save = |code: String, revision: u64| {
        let update = serde_json::from_value(json!({"expectedRevision": revision, "code": code, "source": "ai", "revisionName": format!("r{revision}")})).unwrap();
        state.workspaces.update(&id, update).unwrap().workspace.revision
    };
    let mut revision = 1;
    for size in 0..(MAX_REVISION_HISTORY as u64 + 5) {
        revision = save(format!("cube({});\n", size + 2), revision);
    }
    let retained = state.workspaces.revision_list(&id).unwrap();
    assert_eq!(retained.len(), MAX_REVISION_HISTORY);
    assert_eq!(retained.last().unwrap().revision, revision - 1);
    assert_eq!(retained.last().unwrap().name, format!("r{}", revision - 2));
    // 256 KiB sources: the byte ceiling decides, well before the count.
    for round in 0..20u64 {
        revision = save(format!("// {}\ncube(1);\n", format!("{round}").repeat(1)).repeat(1) + &"/".repeat(256 * 1024), revision);
    }
    let retained = state.workspaces.revision_list(&id).unwrap();
    let bytes: usize = retained.iter().map(|info| info.bytes).sum();
    assert!(bytes <= MAX_REVISION_HISTORY_BYTES, "{bytes}");
    assert!(retained.len() < 20, "{}", retained.len());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_burst_of_browser_saves_is_one_revision_but_nothing_else_coalesces() {
    let (state, root) = state();
    let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
    let save = |code: &str, revision: u64, extra: Value| {
        let mut update = json!({"expectedRevision": revision, "code": code, "source": "browser"});
        merge_fields(&mut update, extra);
        state.workspaces.update(&id, serde_json::from_value(update).unwrap()).unwrap().workspace
    };
    let names = || state.workspaces.revision_list(&id).unwrap().iter().map(|info| info.revision).collect::<Vec<_>>();
    // "New design" (revision 1) is named, so it stays.
    assert_eq!(save("cube(2);\n", 1, json!({})).revision_name, BROWSER_REVISION_NAME);
    assert_eq!(names(), vec![1]);
    // The burst: every number moves, one entry stays, and it holds the latest text.
    save("cube(3);\n", 2, json!({}));
    let latest = save("cube(4);\n", 3, json!({}));
    assert_eq!(latest.revision, 4);
    assert_eq!(names(), vec![1]);
    assert_eq!(state.workspaces.revision_code(&id, 2).unwrap(), None);
    // A named revision and whatever follows it are kept.
    save("cube(5);\n", 4, json!({"revisionName": "Restore: x"}));
    assert_eq!(names(), vec![1, 4]);
    save("cube(6);\n", 5, json!({}));
    assert_eq!(names(), vec![1, 4, 5]);
    // An agent's revision never replaces the autosave before it.
    save("cube(7);\n", 6, json!({"source": "ai", "revisionName": "Agent"}));
    assert_eq!(names(), vec![1, 4, 5, 6]);
    // An old autosave, a minute or more back, starts a new entry.
    let mut stale = state.workspaces.get(&id).unwrap().unwrap();
    stale.revision_name = BROWSER_REVISION_NAME.into();
    stale.revision_source = "browser".into();
    let now = unix_millis();
    assert!(crate::coalesces_with_current(&Workspace { updated_at: now, events: vec![], ..stale.clone() }, &crate::RevisionMark { name: BROWSER_REVISION_NAME.into(), source: "browser".into() }, now));
    assert!(!crate::coalesces_with_current(&Workspace { updated_at: now - BROWSER_COALESCE_MILLIS, events: vec![], ..stale.clone() }, &crate::RevisionMark { name: BROWSER_REVISION_NAME.into(), source: "browser".into() }, now));
    // A burst that never pauses still leaves a checkpoint per period.
    let boundary = (now / crate::BROWSER_CHECKPOINT_MILLIS) * crate::BROWSER_CHECKPOINT_MILLIS;
    assert!(!crate::coalesces_with_current(&Workspace { updated_at: boundary - 1, events: vec![], ..stale }, &crate::RevisionMark { name: BROWSER_REVISION_NAME.into(), source: "browser".into() }, boundary + 1));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn routes_list_and_restore_revisions_and_refuse_removed_arguments() {
    let (state, root) = state();
    let id = named_design(&state, "cube(10);\n", "<b>First</b>");
    structured(&raw(&state, "update_design_code", json!({"designId": id, "expectedRevision": 1, "code": "cube(20);\n", "revisionName": "Second"})));
    let (status, body) = http_json(&state, "GET", &format!("/api/designs/{id}/revisions"), None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["current"], json!(2));
    assert_eq!(body["limit"], json!(MAX_REVISION_HISTORY));
    assert_eq!(body["revisions"][1]["name"], json!("<b>First</b>"), "escaped where it is shown, stored as written");

    // Restore: a new revision, named by the browser, on top.
    let restore = format!("/api/designs/{id}/revisions");
    let (status, body) = http_json(&state, "POST", &restore, Some(json!({"revision": 1, "revisionName": "Restore: First", "expectedRevision": 2})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["workspace"]["revision"], json!(3));
    assert_eq!(body["workspace"]["revisionName"], json!("Restore: First"));
    assert_eq!(body["workspace"]["code"], json!("cube(10);\n"));
    let (status, _) = http_json(&state, "POST", &restore, Some(json!({"revision": 1, "revisionName": "Stale", "expectedRevision": 2})));
    assert_eq!(status, 409);
    let (status, _) = http_json(&state, "POST", &restore, Some(json!({"revision": 1, "expectedRevision": 3})));
    assert_eq!(status, 400);
    let (status, _) = http_json(&state, "POST", &restore, Some(json!({"revision": 77, "revisionName": "x", "expectedRevision": 3})));
    assert_eq!(status, 404);
    assert_eq!(crate::route_cost("POST", &restore), crate::RouteCost::Evaluator);
    assert_eq!(crate::route_cost("GET", &restore), crate::RouteCost::Cheap);

    // A read-only link lists, without the design id, and cannot restore.
    let token = state.workspaces.share_token(&id).unwrap();
    let (status, body) = http_json(&state, "GET", &format!("/api/shared/{token}/revisions"), None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revisions"][0]["revision"], json!(3));
    assert!(!body.to_string().contains(&id), "{body}");
    let (status, _) = http_json(&state, "POST", &format!("/api/shared/{token}/revisions"), Some(json!({"revision": 1, "revisionName": "x", "expectedRevision": 3})));
    assert_eq!(status, 405);
    assert_eq!(revision_of(&state, &id), 3);

    // The agent routes refuse what the tools refuse, and the stateless ones are gone.
    for (path, body) in [
        (format!("/api/designs/{id}/measure"), json!({"overrides": {"w": 2}})),
        (format!("/api/designs/{id}/evaluate"), json!({"code": "cube(1);"})),
        (format!("/api/designs/{id}/evaluate"), json!({"edits": [{"search": "10", "replace": "11"}]})),
        (format!("/api/designs/{id}/intersections"), json!({"sweep": {"name": "w", "values": [1]}})),
        (format!("/api/designs/{id}/image"), json!({"revision": 1})),
        (format!("/api/designs/{id}/render"), json!({"overrides": {"w": 2}, "revision": 3, "requestId": "ovr00000003"})),
        (format!("/api/designs/{id}/export"), json!({"overrides": {"w": 2}, "revision": 3})),
        ("/api/render".to_string(), json!({"code": "cube(1);", "overrides": {"w": 2}, "requestId": "ovr00000004"})),
        ("/api/export".to_string(), json!({"code": "cube(1);", "overrides": {"w": 2}})),
    ] {
        let (status, _) = http(&state, "POST", &path, Some(body));
        assert_eq!(status, 400, "{path}");
    }
    for action in ["objects", "evaluate", "measure", "image", "diff", "parameters", "print-layout"] {
        let (status, _) = http(&state, "POST", &format!("/api/{action}"), Some(json!({"code": "cube(1);"})));
        assert_ne!(status, 200, "/api/{action} is gone");
    }
    // What the browser needs for a person's own text stays.
    let (status, body) = http_json(&state, "POST", "/api/intersections", Some(json!({"code": "cube(1);"})));
    assert_eq!(status, 200, "{body}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn instructions_and_descriptions_no_longer_offer_ephemeral_analysis() {
    let tools = serde_json::to_string(&crate::mcp_tool_definitions()).unwrap();
    let dictionary = crate::supported_scad_dictionary()["analysisTools"].to_string();
    for text in [crate::MCP_INSTRUCTIONS, tools.as_str(), dictionary.as_str()] {
        for stale in ["unsaved", "Unsaved", "never save", "overrides apply", "takes overrides", "variants", "dryRun", "last 20 revisions"] {
            assert!(!text.contains(stale), "{stale}");
        }
    }
    assert!(crate::MCP_INSTRUCTIONS.contains("last 200 revisions"));
    assert!(crate::MCP_INSTRUCTIONS.contains("revisionName"));
}

/// A write landing between the save and the evaluation must not be answered
/// with `saved` naming one revision and the geometry of another.
#[test]
fn evaluating_a_saved_revision_refuses_a_newer_one() {
    let (state, root) = state();
    let id = named_design(&state, "cube(10);\n", "First");
    structured(&raw(&state, "update_design_code", json!({"designId": id, "expectedRevision": 1, "code": "cube(20);\n", "revisionName": "Second"})));
    let input = json!({"designId": id});
    let error = evaluate_revision(&state, None, &input, Some(1)).unwrap_err();
    assert_eq!(error.status, 409);
    assert!(error.message.contains("revision 2 landed before revision 1"), "{}", error.message);
    let (_, value) = evaluate_revision(&state, None, &input, Some(2)).unwrap();
    assert_eq!(value["revision"], json!(2));
    assert_eq!(value["objects"][0]["bounds"]["max"][0], json!(20.0));
    let _ = fs::remove_dir_all(root);
}

/// A failed save has cost nothing: every revision it would have evicted is
/// still readable, the design is unchanged, and the retry trims as usual.
#[test]
fn a_failed_filesystem_save_evicts_nothing() {
    let (state, root) = state();
    let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
    // Full history: revisions 1..=200 on disk, current 201.
    let rows: Vec<Value> = (1..=MAX_REVISION_HISTORY as u64)
        .map(|revision| json!({"revision": revision, "code": format!("cube({revision});\n"), "timestamp": revision, "name": format!("r{revision}"), "source": "ai"}))
        .collect();
    fs::write(root.join(format!("{id}.history")), serde_json::to_vec(&rows).unwrap()).unwrap();
    let document = root.join(format!("{id}.json"));
    let mut stored: Value = serde_json::from_slice(&fs::read(&document).unwrap()).unwrap();
    stored["revision"] = json!(201);
    fs::write(&document, serde_json::to_vec(&stored).unwrap()).unwrap();
    let store = crate::WorkspaceStore::open(root.clone()).unwrap();
    let state = AppState { workspaces: crate::Store::Filesystem(store), ..state };
    let update = || serde_json::from_value::<WorkspaceUpdate>(json!({"expectedRevision": 201, "code": "cube(99);\n", "source": "ai", "revisionName": "Next"})).unwrap();

    crate::PERSIST_FAILS.with(|fails| fails.set(true));
    let failed = state.workspaces.update(&id, update());
    crate::PERSIST_FAILS.with(|fails| fails.set(false));
    assert!(matches!(failed, Err(UpdateError::Internal(_))), "{failed:?}");
    let current = state.workspaces.get(&id).unwrap().unwrap();
    assert_eq!(current.revision, 201);
    assert_eq!(current.code, "cube(1);\n");
    assert_eq!(state.workspaces.revision_code(&id, 1).unwrap().as_deref(), Some("cube(1);\n"), "the oldest is not evicted");
    let rows = revision_rows(&state, &current).unwrap();
    assert_eq!(rows.iter().filter(|row| row["revision"] == json!(201)).count(), 1, "the leftover snapshot of the current one is not listed twice");

    let saved = state.workspaces.update(&id, update()).unwrap();
    assert_eq!(saved.workspace.revision, 202);
    let retained = state.workspaces.revision_list(&id).unwrap();
    assert_eq!(retained.len(), MAX_REVISION_HISTORY);
    assert_eq!(retained[0].revision, 2);
    assert_eq!(retained.last().unwrap().revision, 201);
    let _ = fs::remove_dir_all(root);
}

/// A pin names the revision under the click: the next autosave keeps it.
#[test]
fn an_autosave_never_replaces_a_revision_a_comment_names() {
    let (state, root) = state();
    let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
    let save = |code: &str, revision: u64| {
        let update = serde_json::from_value(json!({"expectedRevision": revision, "code": code, "source": "browser"})).unwrap();
        state.workspaces.update(&id, update).unwrap();
    };
    save("cube(2);\n", 1);
    let (status, body) = http_json(&state, "POST", &format!("/api/designs/{id}/feedback"), Some(json!({"text": "here", "point": [1, 1, 1], "revision": 2})));
    assert_eq!(status, 200, "{body}");
    save("cube(3);\n", 2);
    assert_eq!(state.workspaces.revision_code(&id, 2).unwrap().as_deref(), Some("cube(2);\n"));
    // An unpinned autosave in the same burst is still replaced.
    save("cube(4);\n", 3);
    assert_eq!(state.workspaces.revision_code(&id, 3).unwrap(), None);
    // The agent can read and restore what the pin points at.
    let code = structured(&raw(&state, "get_design_code", json!({"designId": id, "revision": 2})));
    assert!(code["code"].as_str().unwrap().contains("cube(2)"));
    let restored = structured(&raw(&state, "restore_revision", json!({"designId": id, "revision": 2, "revisionName": "Back to the pin", "expectedRevision": 4})));
    assert_eq!(restored["restoredFrom"], json!(2));
    let _ = fs::remove_dir_all(root);
}

/// `scad` asks for polygon() text in the reply. It is no way to pass source:
/// the section always reads the current revision.
#[test]
fn section_profile_scad_is_an_output_switch_and_takes_no_source() {
    let (state, root) = state();
    let id = named_design(&state, "cube(10);\n", "First");
    let plane = json!({"axis": "z", "offset": 5});
    let refused = raw(&state, "section_profile", json!({"designId": id, "plane": plane, "scad": "cube(99);"}));
    assert_eq!(refused["isError"], json!(true), "{refused}");
    let code = raw(&state, "section_profile", json!({"designId": id, "plane": plane, "code": "cube(99);"}));
    assert!(error_text(&code).contains("no longer takes code"), "{code}");
    let reply = structured(&raw(&state, "section_profile", json!({"designId": id, "plane": plane, "scad": true})));
    assert_eq!(reply["revision"], json!(1), "{reply}");
    assert!(reply["scad"].as_str().unwrap().contains("polygon("), "{reply}");
    let _ = fs::remove_dir_all(root);
}
