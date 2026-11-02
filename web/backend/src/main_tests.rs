#[test]
fn media_byte_ranges() {
    assert_eq!(parse_byte_range("0-99", 1000), Some((0, 99)));
    assert_eq!(parse_byte_range("500-", 1000), Some((500, 999)));
    assert_eq!(parse_byte_range("-100", 1000), Some((900, 999)));
    assert_eq!(parse_byte_range("0-5000", 1000), Some((0, 999)));
    assert_eq!(parse_byte_range("1000-", 1000), None);
    assert_eq!(parse_byte_range("5-2", 1000), None);
    assert_eq!(parse_byte_range("0-1,5-9", 1000), None);
    assert_eq!(parse_byte_range("0-", 0), None);
}
use super::*;

/// A compile of `source` with its meshes resized to the given triangle
/// counts, so the size checks can be tested without building the geometry.
fn output_with_triangles(source: &str, combined: usize, parts: &[usize]) -> engine::MultipartCompileOutput {
    let mut output =
        engine::compile_parts(source, engine::Quality::Preview, None).unwrap();
    assert_eq!(output.parts.len(), parts.len());
    let triangle = output.parts[0].mesh.triangles[0];
    output.mesh.triangles = vec![triangle; combined];
    for (part, count) in output.parts.iter_mut().zip(parts) {
        part.mesh.triangles = vec![triangle; *count];
    }
    output
}

#[test]
fn render_size_checks_count_the_copies_the_response_sends() {
    let check = |output: &engine::MultipartCompileOutput, selected: bool| {
        let sends_combined = sends_combined_mesh(output, selected);
        multipart_binary_size(output, sends_combined).is_some_and(|size| size <= MAX_OUTPUT)
            && multipart_response_fits(output, &output.mesh, sends_combined)
    };

    // One object, everything requested: the part is the only copy sent.
    // The default asset budget and the largest one both fit...
    let one_object = "cube(1);";
    assert!(check(&output_with_triangles(one_object, 500_000, &[500_000]), false));
    assert!(check(
        &output_with_triangles(one_object, 1_000_000, &[1_000_000]),
        false
    ));
    // ...and the limit still holds on that copy.
    assert!(!check(
        &output_with_triangles(one_object, 1_100_000, &[1_100_000]),
        false
    ));

    // A selection sends the combined mesh as well as every object, so the
    // same geometry counts twice.
    assert!(check(&output_with_triangles(one_object, 500_000, &[500_000]), true));
    assert!(!check(&output_with_triangles(one_object, 600_000, &[600_000]), true));

    // Several objects add up.
    let two_objects = "cube(1); translate([5, 0, 0]) cube(1);";
    assert!(check(
        &output_with_triangles(two_objects, 800_000, &[400_000, 400_000]),
        false
    ));
    assert!(!check(
        &output_with_triangles(two_objects, 1_200_000, &[600_000, 600_000]),
        false
    ));
}

fn temporary_store() -> (WorkspaceStore, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = env::temp_dir().join(format!(
        "reopenscad-workspace-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let store = WorkspaceStore::open(root.clone()).unwrap();
    (store, root)
}

fn temporary_state(public_origin: Option<&str>) -> (AppState, PathBuf) {
    let (workspaces, root) = temporary_store();
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
            workspaces: Store::Filesystem(workspaces),
            public_origin: public_origin.map(str::to_string),
        },
        root,
    )
}

/// Every workspace file written before the build-plate feature existed lacks
/// `plate` and `tolerance`. Without the serde defaults `load` treats the
/// document as corrupt and answers 404, silently deleting real work, so this
/// deserializes the exact pre-change shape.
#[test]
fn workspace_files_written_before_the_viewer_settings_still_load() {
    let legacy = r#"{
            "id": "bouncy-badger-00000000000000000000000000aa",
            "name": "Untitled.scad",
            "code": "cube(10);",
            "revision": 3,
            "createdAt": 1756000000000,
            "updatedAt": 1756000000001,
            "events": []
        }"#;
    let workspace: Workspace = serde_json::from_str(legacy).unwrap();
    assert_eq!(workspace.code, "cube(10);");
    assert_eq!(workspace.revision, 3);
    assert_eq!(workspace.plate, "");
    assert_eq!(workspace.tolerance, DEFAULT_TOLERANCE);

    // And the same file round-trips through the store, which is the path the
    // HTTP GET actually takes.
    let (store, root) = temporary_store();
    let id = "bouncy-badger-00000000000000000000000000aa";
    fs::write(root.join(format!("{id}.json")), legacy).unwrap();
    let reopened = WorkspaceStore::open(root.clone()).unwrap();
    let loaded = reopened.get(id).unwrap().expect("legacy workspace loads");
    assert_eq!(loaded.plate, "");
    assert_eq!(loaded.tolerance, DEFAULT_TOLERANCE);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

/// Picking a build plate is a viewer preference, not an edit: it must persist
/// without bumping the revision (which would look like a remote edit to every
/// other tab) and without needing a `baseRevision` to race against.
#[test]
fn viewer_settings_persist_without_touching_the_revision() {
    let (store, root) = temporary_store();
    let created = store
        .create_with_settings(
            "cube(1);".into(),
            None,
            Some("prusa-mk4s".into()),
            Some(0.35),
        )
        .unwrap();
    assert_eq!(created.plate, "prusa-mk4s");
    assert_eq!(created.tolerance, 0.35);

    let updated = store
        .update_settings(&created.id, Some("bambu-x1"), Some(0.0))
        .unwrap();
    assert_eq!(updated.plate, "bambu-x1");
    assert_eq!(updated.tolerance, 0.0, "a deliberate 0 mm clearance survives");
    assert_eq!(updated.revision, created.revision, "no phantom edit");

    // A later code save must not clear the plate the user picked.
    let edited = store
        .update(
            &created.id,
            WorkspaceUpdate {
                code: Some("cube(2);".into()),
                name: None,
                start: None,
                end: None,
                text: None,
                expected_revision: Some(updated.revision),
                base_revision: None,
                plate: None,
                tolerance: None,
                source: "browser".into(),
                revision_name: None,
            },
        )
        .unwrap();
    assert_eq!(edited.workspace.plate, "bambu-x1");
    assert_eq!(edited.workspace.tolerance, 0.0);
    assert_eq!(edited.workspace.revision, updated.revision + 1);

    // Settings reach disk, not just the cache.
    let bytes = fs::read(root.join(format!("{}.json", created.id))).unwrap();
    let stored: Workspace = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(stored.plate, "bambu-x1");
    fs::remove_dir_all(root).unwrap();
}

/// Viewer settings are revisionless, so the poll's `changed` flag says
/// nothing about them: they have to ride every events payload, or a plate
/// picked in one tab (or by an MCP agent) would never reach a tab already
/// looking at the workspace.
#[test]
fn events_payload_always_carries_the_viewer_settings() {
    let workspace = Workspace {
        id: "sunny-otter-0011223344556677".into(),
        name: "Untitled model".into(),
        code: "cube(10);".into(),
        revision: 7,
        created_at: 1,
        updated_at: 2,
        plate: "prusa-mk4s".into(),
        tolerance: 0.35,
        preview: String::new(),
        share_token: String::new(),
        revision_name: String::new(),
        revision_source: String::new(),
        events: Vec::new(),
    };
    // Nothing changed: the settings still have to be there.
    let idle = workspace_events_payload(&workspace.id, &workspace, 7);
    assert_eq!(idle["changed"], json!(false));
    assert_eq!(idle["plate"], json!("prusa-mk4s"));
    assert_eq!(idle["tolerance"], json!(0.35));
    let changed = workspace_events_payload(&workspace.id, &workspace, 6);
    assert_eq!(changed["changed"], json!(true));
    assert_eq!(changed["plate"], json!("prusa-mk4s"));
}

/// Plate ids are client-chosen catalogue keys, so they are sanitized rather
/// than trusted — but a nonsense preference must degrade to "no plate", never
/// fail the request that carried it.
#[test]
fn plate_ids_and_tolerances_are_sanitized() {
    assert_eq!(sanitize_plate("prusa-mk4s"), "prusa-mk4s");
    assert_eq!(sanitize_plate("  Bambu_X1  "), "bambu_x1");
    assert_eq!(sanitize_plate("../../etc/passwd"), "");
    assert_eq!(sanitize_plate("<script>"), "");
    assert_eq!(sanitize_plate(&"a".repeat(MAX_PLATE_ID + 1)), "");
    assert_eq!(sanitize_plate(""), "");

    assert_eq!(sanitize_tolerance(0.0), 0.0);
    assert_eq!(sanitize_tolerance(0.45), 0.45);
    assert_eq!(sanitize_tolerance(-1.0), DEFAULT_TOLERANCE);
    assert_eq!(sanitize_tolerance(f64::NAN), DEFAULT_TOLERANCE);
    assert_eq!(sanitize_tolerance(f64::INFINITY), DEFAULT_TOLERANCE);
    assert_eq!(sanitize_tolerance(MAX_TOLERANCE + 1.0), DEFAULT_TOLERANCE);
}

#[test]
fn parses_json_source() {
    let json = r#"{"note":"a nested \"code\": marker","code":"cube([1, 2, 3]);\n// café"}"#;
    let input = parse_json_body(json.as_bytes()).unwrap();
    assert_eq!(
        input.get("code").and_then(Value::as_str).unwrap(),
        "cube([1, 2, 3]);\n// café"
    );
}

#[test]
fn encodes_base64() {
    assert_eq!(base64_encode(b"ReOpenSCAD"), "UmVPcGVuU0NBRA==");
}

#[test]
fn rejects_traversal_after_decode() {
    assert_eq!(percent_decode("..%2fsecret"), "../secret");
}

#[test]
fn native_compile_produces_binary_stl() {
    let compiled = compile_native(
        "difference() { cube(10, center=true); sphere(3); }",
        engine::Quality::Preview,
        None,
    )
    .unwrap();
    assert!(compiled.triangles > 100);
    assert_eq!(compiled.bytes.len(), 84 + compiled.triangles * 50);
    assert!(compiled.console.contains("Exact polyhedral kernel"));
}

#[test]
fn validates_render_request_ids() {
    assert!(valid_request_id("render-42_a"));
    assert!(!valid_request_id(""));
    assert!(!valid_request_id("render/../../cancel"));
}

#[test]
fn accepts_cli_and_same_origin_requests_for_deployment_hosts() {
    let (local_state, local_root) = temporary_state(None);
    let (public_state, public_root) = temporary_state(Some("https://cad.example"));
    let request = |host: &str, origin: Option<&str>| Request {
        method: "POST".into(),
        path: "/api/render".into(),
        query: String::new(),
        headers: [
            Some(("host".into(), host.into())),
            origin.map(|value| ("origin".into(), value.into())),
        ]
        .into_iter()
        .flatten()
        .collect(),
        body: Vec::new(),
    };
    assert!(request_is_allowed(
        &request("127.0.0.1:5173", Some("http://127.0.0.1:5173")),
        &local_state
    ));
    assert!(!request_is_allowed(
        &request("attacker.example", Some("http://attacker.example")),
        &local_state
    ));
    assert!(!request_is_allowed(
        &request("127.0.0.1:5173", Some("https://attacker.example")),
        &local_state
    ));
    assert!(request_is_allowed(
        &request("127.0.0.1:5173", None),
        &local_state
    ));
    assert!(!request_is_allowed(
        &request("bad host", None),
        &local_state
    ));
    assert!(request_is_allowed(
        &request("cad.example", Some("https://cad.example")),
        &public_state
    ));
    assert!(!request_is_allowed(
        &request("poison.example", Some("https://cad.example")),
        &public_state
    ));
    fs::remove_dir_all(local_root).unwrap();
    fs::remove_dir_all(public_root).unwrap();
}

#[test]
fn render_limiter_releases_capacity() {
    let limiter = JobLimiter::new(2);
    let first = limiter.try_acquire().expect("first slot");
    let second = limiter.try_acquire().expect("second slot");
    assert!(limiter.try_acquire().is_none());
    drop(first);
    assert!(limiter.try_acquire().is_some());
    drop(second);
}

// --- The render queue -------------------------------------------------
//
// Five properties, one test each: contention waits instead of failing, the
// deadline never charges for the wait, one client cannot starve another,
// a cancelled job never starts, and the whole thing stays bounded.

fn test_queue(capacity: usize, depth: usize, per_client: usize) -> RenderQueue {
    RenderQueue::new(capacity, depth, per_client, Duration::from_secs(5))
}

fn client(last_octet: u8) -> ClientKey {
    ClientKey::Ip(IpAddr::from([203, 0, 113, last_octet]))
}

/// Blocks until `count` jobs are parked, so a test can build an exact
/// queue order without sleeping and hoping.
fn await_depth(queue: &RenderQueue, count: usize) {
    for _ in 0..2_000 {
        if queue.inner.state.lock().unwrap().depth == count {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("queue never reached depth {count}");
}

#[test]
fn contended_jobs_queue_instead_of_being_rejected() {
    // The old behaviour: the third caller got 429 and lost its work. The
    // queue's whole point is that all of them finish.
    let queue = Arc::new(test_queue(1, 8, 8));
    let done = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..5)
        .map(|index| {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let permit = queue
                    .acquire(client(index), None, None)
                    .expect("every contended job must be admitted, not shed");
                thread::sleep(Duration::from_millis(10));
                done.fetch_add(1, Ordering::SeqCst);
                drop(permit);
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(done.load(Ordering::SeqCst), 5);
    let state = queue.inner.state.lock().unwrap();
    assert_eq!(state.active, 0, "permits must be returned");
    assert_eq!(state.depth, 0, "no waiter may be left behind");
}

/// Seconds of exact booleans even in release, mostly inside single
/// unions, face merges and worker threads, where a deadline used to go
/// unread until the boolean finished.
const HEAVY_UNION: &str = "union() { for (i = [0:3]) translate([i * 6, 0, 0]) \
         difference() { sphere(10, $fn = 120); sphere(9, $fn = 120); } }";

#[test]
fn a_render_past_its_deadline_stops_promptly_and_frees_its_slot() {
    let queue = test_queue(1, 4, 4);
    let permit = queue.acquire(client(1), None, None).unwrap();
    let started = Instant::now();
    let cancellation = Arc::new(AtomicBool::new(false));
    let deadline = RenderDeadline::start(&cancellation, Duration::from_millis(300));
    let error = engine::compile_parts(HEAVY_UNION, engine::Quality::Preview, Some(&cancellation))
        .map_err(|error| deadline.explain(error.to_string()))
        .expect_err("the render must not outrun its deadline");
    let stopped = started.elapsed();
    assert!(error.contains("time limit"), "{error}");
    assert!(stopped < Duration::from_millis(2300), "stopped after {stopped:?}");
    drop(deadline);
    drop(permit);
    let waited = Instant::now();
    let next = queue.acquire(client(2), None, None).expect("the slot must be free again");
    assert!(waited.elapsed() < Duration::from_millis(200));
    drop(next);
}

#[test]
fn a_client_that_hangs_up_cancels_its_compile() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let watch = ClientWatch::start(&server, true).expect("the watch starts");
    let hang_up = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        // A full close: FIN now, a reset once the probe reaches it.
        drop(caller);
        Instant::now()
    });
    // No flag of its own: only the watch's, installed on this thread.
    let error = engine::compile_parts(HEAVY_UNION, engine::Quality::Preview, None)
        .expect_err("an abandoned compile must stop");
    let overrun = Instant::now().saturating_duration_since(hang_up.join().unwrap());
    assert_eq!(error.to_string(), "Render cancelled.");
    assert!(overrun < Duration::from_secs(2), "ran {overrun:?} past the hang-up");
    drop(watch);
    assert!(csg::bsp::interrupt_message().is_none(), "the watch must uninstall its flag");
}

/// `SO_LINGER {on, 0}`: close sends a reset instead of a FIN.
#[cfg(unix)]
fn reset_on_close(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    #[repr(C)]
    struct Linger {
        on: i32,
        seconds: i32,
    }
    extern "C" {
        fn setsockopt(socket: i32, level: i32, name: i32, value: *const Linger, length: u32) -> i32;
    }
    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_LINGER: i32 = 13;
    #[cfg(not(target_os = "linux"))]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(not(target_os = "linux"))]
    const SO_LINGER: i32 = 0x80;
    let linger = Linger { on: 1, seconds: 0 };
    // SAFETY: a valid descriptor and a correctly sized option value.
    let result = unsafe {
        setsockopt(stream.as_raw_fd(), SOL_SOCKET, SO_LINGER, &linger, std::mem::size_of::<Linger>() as u32)
    };
    assert_eq!(result, 0, "SO_LINGER");
}

#[cfg(unix)]
#[test]
fn a_reset_cancels_without_a_probe() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    // Even an HTTP/1.0 client (no probe allowed) is cancelled by a reset.
    let watch = ClientWatch::start(&server, false).expect("the watch starts");
    thread::sleep(Duration::from_millis(300));
    reset_on_close(&caller);
    drop(caller);
    let deadline = Instant::now() + Duration::from_secs(2);
    while csg::bsp::interrupt_message().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(csg::bsp::interrupt_message().is_some(), "a reset is a client that left");
    drop(watch);
}

#[test]
fn an_http10_client_that_closed_is_left_to_finish() {
    // No probe may be sent, so end of stream is not proof: keep computing.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let watch = ClientWatch::start(&server, false).expect("the watch starts");
    caller.shutdown(std::net::Shutdown::Write).unwrap();
    thread::sleep(CLIENT_WATCH_INTERVAL * 4);
    assert!(csg::bsp::interrupt_message().is_none());
    drop(watch);
}

#[test]
fn a_client_that_stays_is_not_taken_for_gone() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let watch = ClientWatch::start(&server, true).expect("the watch starts");
    // A pipelined next request is bytes waiting, not a hang-up.
    caller.write_all(b"GET / HTTP/1.1\r\n").unwrap();
    thread::sleep(CLIENT_WATCH_INTERVAL * 4);
    assert!(csg::bsp::interrupt_message().is_none());
    drop(watch);
    drop(caller);
}

/// A client that half-closes (shutdown(SHUT_WR)) at any point, early or
/// long after its request, keeps its job and reads the full answer after
/// the interim 102.
#[test]
fn a_client_that_half_closed_still_gets_an_answer_at_any_delay() {
    for delay in [0u64, 50, 300, 700] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        // Before the close: macOS refuses socket options on a finished connection.
        caller.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let watch = ClientWatch::start(&server, true).expect("the watch starts");
        thread::sleep(Duration::from_millis(delay));
        caller.shutdown(std::net::Shutdown::Write).unwrap();
        thread::sleep(CLIENT_WATCH_INTERVAL * 5);
        assert!(csg::bsp::interrupt_message().is_none(), "half-closed after {delay} ms is not gone");
        respond_json(&mut server, 200, r#"{"ok":true}"#).unwrap();
        drop(watch);
        drop(server);
        let mut response = String::new();
        caller.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 102 Processing\r\n\r\nHTTP/1.1 200 OK\r\n"),
            "after {delay} ms: {response}"
        );
        assert!(response.ends_with(r#"{"ok":true}"#), "{response}");
    }
}

/// Once the response has started the watch writes nothing, so a probe can
/// never land inside a response.
#[test]
fn the_watch_never_writes_after_the_response_started() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    let watch = ClientWatch::start(&server, true).expect("the watch starts");
    begin_response();
    caller.shutdown(std::net::Shutdown::Write).unwrap();
    thread::sleep(CLIENT_WATCH_INTERVAL * 4);
    respond_json(&mut server, 200, "{}").unwrap();
    drop(watch);
    drop(server);
    let mut response = String::new();
    caller.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
}

#[test]
fn object_compute_reads_are_watched_like_any_compute() {
    let request = |query: &str| Request {
        method: "GET".into(),
        path: "/api/designs/jolly-wombat-00000000000000000000000000bb".into(),
        query: query.into(),
        headers: vec![],
        body: vec![],
    };
    let plain = request("");
    let compute = request("objects=compute");
    assert_eq!(effective_route_cost(&plain), route_cost("GET", &plain.path));
    assert_eq!(effective_route_cost(&compute), RouteCost::Evaluator);
    assert!(watches_client(&compute, effective_route_cost(&compute)), "admitted as compute, watched as compute");
    assert!(!watches_client(&plain, effective_route_cost(&plain)));
}

#[test]
fn the_render_deadline_starts_at_execution_not_at_arrival() {
    // The requirement in one assertion: a job that spent longer waiting
    // than its entire compute budget still gets that budget in full.
    let queue = test_queue(1, 4, 4);
    let budget = Duration::from_millis(200);
    let blocker = queue.acquire(client(1), None, None).unwrap();
    let queued_at = Instant::now();
    let waiter = thread::spawn(move || {
        let queue = queue;
        let permit = queue.acquire(client(2), None, None).unwrap();
        let admitted = Instant::now();
        let cancellation = Arc::new(AtomicBool::new(false));
        let deadline = RenderDeadline::start(&cancellation, budget);
        // Nothing spent in the queue may have been charged already.
        assert!(
            !deadline.expired(),
            "a job that waited must not start out of time"
        );
        thread::sleep(budget / 2);
        assert!(!deadline.expired(), "half the budget must still be left");
        thread::sleep(budget);
        assert!(deadline.expired(), "the budget must still be enforced");
        drop(permit);
        admitted
    });
    // Hold the only slot for twice the compute budget.
    thread::sleep(budget * 2);
    drop(blocker);
    let admitted = waiter.join().unwrap();
    let waited = admitted.duration_since(queued_at);
    assert!(
        waited >= budget,
        "the test is meaningless unless the job really waited (waited {waited:?})"
    );
}

#[test]
fn one_client_flooding_the_queue_cannot_starve_another() {
    // Round-robin, stated as a schedule: a backlog delays the newcomer by
    // one job, not by the whole backlog. Plain FIFO would serve
    // A, A, A, B — the starvation this exists to prevent.
    let queue = Arc::new(test_queue(1, 8, 8));
    let served = Arc::new(Mutex::new(Vec::new()));
    let gate = queue.acquire(client(9), None, None).unwrap();
    let mut workers = Vec::new();
    // Enqueue one at a time so the arrival order is exact, not raced.
    for (index, key) in [(0, client(1)), (1, client(1)), (2, client(1)), (3, client(2))] {
        let worker_queue = Arc::clone(&queue);
        let served = Arc::clone(&served);
        let label = if key == client(2) { "B" } else { "A" };
        workers.push(thread::spawn(move || {
            let permit = worker_queue.acquire(key, None, None).unwrap();
            served.lock().unwrap().push(format!("{label}{index}"));
            drop(permit);
        }));
        await_depth(&queue, index + 1);
    }
    drop(gate);
    for worker in workers {
        worker.join().unwrap();
    }
    let order = served.lock().unwrap().clone();
    assert_eq!(
        order,
        vec!["A0", "B3", "A1", "A2"],
        "the newcomer must be served after one queued job, not after all three"
    );
}

#[test]
fn cancelling_a_queued_job_removes_it_without_ever_running_it() {
    let queue = Arc::new(test_queue(1, 4, 4));
    let cancellation = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let gate = queue.acquire(client(1), None, None).unwrap();
    let waiter = {
        let queue = Arc::clone(&queue);
        let cancellation = Arc::clone(&cancellation);
        let started = Arc::clone(&started);
        thread::spawn(move || {
            let outcome = queue.acquire(client(2), Some("render-1"), Some(&cancellation));
            if outcome.is_ok() {
                started.store(true, Ordering::SeqCst);
            }
            outcome.err()
        })
    };
    await_depth(&queue, 1);
    // Exactly what `POST /api/cancel` does to a registered request.
    cancellation.store(true, Ordering::Release);
    let rejection = waiter.join().unwrap().expect("cancel must not hand out a permit");
    assert_eq!(rejection.error, QueueError::Cancelled);
    assert_eq!(rejection.status(), 409);
    assert!(
        !started.load(Ordering::SeqCst),
        "a cancelled job must never begin"
    );
    assert_eq!(
        queue.inner.state.lock().unwrap().depth,
        0,
        "the cancelled job must leave the queue"
    );
    drop(gate);
}

#[test]
fn the_queue_stays_bounded_and_says_so_in_a_way_a_client_can_act_on() {
    let queue = Arc::new(test_queue(1, 3, 2));
    let gate = queue.acquire(client(9), None, None).unwrap();
    let mut workers = Vec::new();
    let mut enqueue = |key: ClientKey, depth: usize| {
        let worker_queue = Arc::clone(&queue);
        workers.push(thread::spawn(move || {
            drop(worker_queue.acquire(key, None, None).unwrap());
        }));
        await_depth(&queue, depth);
    };
    // One client fills its own allowance of two.
    enqueue(client(1), 1);
    enqueue(client(1), 2);
    // Its third is refused even though the queue still has a place free:
    // that free place belongs to somebody else, which is the whole point.
    let capped = queue.acquire(client(1), None, None).unwrap_err();
    assert_eq!(capped.error, QueueError::ClientLimit);
    // And the place really was available to another client.
    enqueue(client(2), 3);
    // Now the global cap bites, for everyone.
    let full = queue.acquire(client(3), None, None).unwrap_err();
    assert_eq!(full.error, QueueError::Full);
    for rejection in [capped, full] {
        assert_eq!(rejection.status(), 429);
        assert!(rejection.retry_after_seconds() > 0, "429 without a schedule");
        let detail = rejection.detail();
        assert_eq!(detail["retryable"], json!(true));
        // The distinction an agent must not get wrong.
        assert_eq!(detail["codeProblem"], json!(false));
        assert!(detail["error"].as_str().unwrap().contains("retry"));
    }
    drop(gate);
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn queue_status_names_the_position_a_waiting_caller_is_in() {
    let queue = Arc::new(test_queue(1, 4, 4));
    let gate = queue.acquire(client(9), Some("running-1"), None).unwrap();
    let mut workers = Vec::new();
    for (index, id) in ["queued-1", "queued-2"].iter().enumerate() {
        let worker_queue = Arc::clone(&queue);
        let id = (*id).to_string();
        workers.push(thread::spawn(move || {
            drop(worker_queue.acquire(client(1), Some(&id), None).unwrap());
        }));
        await_depth(&queue, index + 1);
    }
    let running = queue.status("running-1");
    assert_eq!(running["state"], json!("running"));
    let first = queue.status("queued-1");
    assert_eq!(first["state"], json!("queued"));
    assert_eq!(first["position"], json!(1));
    let second = queue.status("queued-2");
    assert_eq!(second["position"], json!(2));
    assert_eq!(second["waiting"], json!(2));
    assert_eq!(queue.status("never-heard-of-it")["state"], json!("unknown"));
    drop(gate);
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn a_shed_mcp_tool_call_reads_as_transient_capacity_not_as_bad_input() {
    // What an AI client actually receives. It must be a tool error (so the
    // model sees it at all), it must say retry, and it must rule out the
    // interpretation that sends an agent rewriting working geometry.
    let queue = test_queue(1, 1, 1);
    let gate = queue.acquire(client(1), None, None).unwrap();
    let waiter = thread::spawn({
        let queue = queue.clone();
        move || drop(queue.acquire(client(2), None, None).unwrap())
    });
    // Fill the single waiting place, then shed the next call.
    for _ in 0..2_000 {
        if queue.inner.state.lock().unwrap().depth == 1 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let rejection = queue.acquire(client(3), None, None).unwrap_err();
    let result = mcp_queue_rejection(rejection);
    assert_eq!(result["isError"], json!(true));
    let structured = &result["structuredContent"];
    assert_eq!(structured["reason"], json!("queue_full"));
    assert_eq!(structured["retryable"], json!(true));
    assert_eq!(structured["codeProblem"], json!(false));
    assert!(structured["retryAfterSeconds"].as_u64().unwrap() > 0);
    assert_eq!(structured["queue"]["maxQueued"], json!(1));
    let prose = structured["error"].as_str().unwrap();
    assert!(prose.contains("transient"), "{prose}");
    assert!(prose.contains("nothing about the SCAD code is wrong"), "{prose}");
    assert!(
        structured["nextStep"]
            .as_str()
            .unwrap()
            .contains("Do not modify the SCAD source"),
        "an agent must be told explicitly not to 'fix' the model"
    );
    // The text content mirrors the structured payload, for clients that
    // only read `content`.
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("retryable"));
    drop(gate);
    waiter.join().unwrap();
}

#[test]
fn cancellable_jobs_reject_duplicate_ids_and_unregister_on_drop() {
    let (state, root) = temporary_state(None);
    let (signal, registration) = register_cancellation(&state, "workspace-render-1").unwrap();
    assert!(register_cancellation(&state, "workspace-render-1").is_err());
    let registered = state
        .cancellations
        .lock()
        .unwrap()
        .get("workspace-render-1")
        .cloned()
        .unwrap();
    registered.store(true, Ordering::Release);
    assert!(signal.load(Ordering::Acquire));
    drop(registration);
    assert!(!state
        .cancellations
        .lock()
        .unwrap()
        .contains_key("workspace-render-1"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspaces_have_funny_unpredictable_capability_ids_and_persist() {
    let (store, root) = temporary_store();
    let first = store
        .create("cube(1);".into(), Some("Fixture".into()))
        .unwrap();
    let second = store.create("sphere(2);".into(), None).unwrap();
    assert_ne!(first.id, second.id);
    assert!(valid_workspace_id(&first.id));
    assert!(first.id.split('-').count() >= 3);
    drop(store);

    let reopened = WorkspaceStore::open(root.clone()).unwrap();
    let loaded = reopened.get(&first.id).unwrap().unwrap();
    assert_eq!(loaded.code, "cube(1);");
    assert_eq!(loaded.name, "Fixture");
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join(format!("{}.json", first.id)))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn range_updates_are_validated_atomic_and_revision_checked() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    let updated = store
        .update(
            &workspace.id,
            WorkspaceUpdate {
                code: None,
                name: None,
                start: Some(5),
                end: Some(6),
                text: Some("12".into()),
                expected_revision: Some(1),
                base_revision: None,
                plate: None,
                tolerance: None,
                source: "ai".into(),
                revision_name: None,
            },
        )
        .unwrap();
    assert_eq!(updated.workspace.code, "cube(12);");
    assert_eq!(updated.workspace.revision, 2);
    assert_eq!(updated.events[0].source, "ai");
    assert_eq!(updated.events[0].workspace_id, workspace.id);
    assert!(matches!(
        store.update(
            &workspace.id,
            WorkspaceUpdate {
                code: Some("sphere(3);".into()),
                name: None,
                start: None,
                end: None,
                text: None,
                expected_revision: Some(1),
                base_revision: None,
                plate: None,
                tolerance: None,
                source: "browser".into(),
                revision_name: None,
            }
        ),
        Err(UpdateError::Conflict(_))
    ));
    assert_eq!(store.get(&workspace.id).unwrap().unwrap().code, "cube(12);");
    fs::remove_dir_all(root).unwrap();
}

fn code_update(code: &str, expected: u64, source: &str) -> WorkspaceUpdate {
    WorkspaceUpdate {
        code: Some(code.into()),
        name: None,
        start: None,
        end: None,
        text: None,
        expected_revision: Some(expected),
        base_revision: None,
        plate: None,
        tolerance: None,
        source: source.into(),
        revision_name: None,
    }
}

/// A full-code save that changes two distant lines records two one-line
/// regions, not the whole file, and every region applies cleanly: the
/// browser highlights only what changed.
#[test]
fn a_full_code_save_records_only_the_changed_lines() {
    let (state, root) = temporary_state(None);
    let old: String = (0..150).map(|line| format!("// line {line}\n")).collect::<String>() + "cube(1);\n";
    let workspace = state.workspaces.create(old.clone(), None).unwrap();
    let new = old
        .replace("// line 10\n", "// line ten\n")
        .replace("// line 120\n", "// line one-twenty\n");
    let updated = state
        .workspaces
        .update(&workspace.id, code_update(&new, 1, "ai"))
        .unwrap();
    assert_eq!(updated.events.len(), 2, "{:?}", updated.events);
    let mut rebuilt = old.clone();
    for event in &updated.events {
        assert_eq!(event.revision, 2);
        assert_eq!(event.source, "ai");
        rebuilt.replace_range(
            event.start..event.end,
            &new[event.start..event.start + event.inserted_length],
        );
        let first = scadsource::line_of(&new, event.start);
        let last = scadsource::line_of(&new, event.start + event.inserted_length.max(1) - 1);
        assert_eq!(first, last, "one line per region");
    }
    assert_eq!(rebuilt, new);
    assert_eq!(
        [11, 121],
        [
            scadsource::line_of(&new, updated.events[0].start),
            scadsource::line_of(&new, updated.events[1].start)
        ]
    );
    // The span is the single equivalent edit, in old-source bytes.
    let (start, end, inserted) = updated.span();
    let mut spanned = old.clone();
    spanned.replace_range(start..end, &new[start..start + inserted]);
    assert_eq!(spanned, new);
    // The /events poll replays both regions of the new revision.
    let events = workspace_events_payload(&workspace.id, &updated.workspace, 1);
    assert_eq!(events["updatedRanges"].as_array().unwrap().len(), 2);
    // An unchanged re-save still records one (empty) event.
    let same = state
        .workspaces
        .update(&workspace.id, code_update(&new, 2, "browser"))
        .unwrap();
    assert_eq!(same.events.len(), 1);
    assert_eq!(same.events[0].inserted_length, 0);
    fs::remove_dir_all(root).unwrap();
}

/// Every saved revision has its predecessor in the history, timestamped
/// when that revision was written rather than when a settings write last
/// touched the workspace.
#[test]
fn revision_history_is_written_with_the_save_and_timed_by_the_edit() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    let second = store
        .update(&workspace.id, code_update("cube(2);", 1, "ai"))
        .unwrap();
    let written = second.events[0].timestamp;
    thread::sleep(Duration::from_millis(5));
    let settled = store.update_settings(&workspace.id, Some("bambu-x1"), None).unwrap();
    assert!(settled.updated_at > written);
    store
        .update(&workspace.id, code_update("cube(3);", 2, "ai"))
        .unwrap();
    let history = store.revision_list(&workspace.id).unwrap();
    assert_eq!(
        history.iter().map(|entry| entry.revision).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(history[0].timestamp, workspace.created_at);
    assert_eq!(history[1].timestamp, written, "revision 2 is timed by its edit");
    assert_eq!(store.revision_code(&workspace.id, 2).unwrap().as_deref(), Some("cube(2);"));
    // The settings write survived the save that followed it.
    assert_eq!(store.get(&workspace.id).unwrap().unwrap().plate, "bambu-x1");
    fs::remove_dir_all(root).unwrap();
}

/// History sidecars and share pointers of workspaces that are gone (an
/// eviction racing an edit, a crash between writes) are swept at boot.
#[test]
fn orphaned_sidecars_are_swept_at_boot() {
    let (store, root) = temporary_store();
    let kept = store.create("cube(1);".into(), None).unwrap();
    store
        .update(&kept.id, code_update("cube(2);", 1, "browser"))
        .unwrap();
    let token = store.share_token(&kept.id).unwrap();
    let ghost = "jolly-otter-00000000000000000000000000ff";
    assert!(valid_workspace_id(ghost));
    fs::write(root.join(format!("{ghost}.history")), b"[]").unwrap();
    fs::write(root.join("0123456789abcdef0123456789abcdef.share"), ghost).unwrap();
    fs::write(root.join(format!(".{ghost}-7.tmp")), b"{").unwrap();
    drop(store);
    let reopened = WorkspaceStore::open(root.clone()).unwrap();
    assert!(!root.join(format!("{ghost}.history")).exists());
    assert!(!root.join("0123456789abcdef0123456789abcdef.share").exists());
    assert!(!root.join(format!(".{ghost}-7.tmp")).exists());
    assert!(root.join(format!("{}.history", kept.id)).exists());
    assert_eq!(reopened.resolve_share(&token).unwrap().unwrap().id, kept.id);
    fs::remove_dir_all(root).unwrap();
}

/// A save of a workspace that is evicted while it writes its history
/// leaves no sidecar behind.
#[test]
fn an_edit_racing_eviction_leaves_no_history_behind() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    // Evict between the history write and the document write: hold the
    // edit lock, let the update block on it, evict, then release.
    let guard = store.edit_lock(&workspace.id);
    let racing = {
        let store = store.clone();
        let id = workspace.id.clone();
        thread::spawn(move || store.update(&id, code_update("cube(2);", 1, "browser")))
    };
    thread::sleep(Duration::from_millis(50));
    {
        let mut inner = store.inner.lock().unwrap();
        store.forget(&mut inner, &workspace.id);
    }
    drop(guard);
    assert!(matches!(racing.join().unwrap(), Err(UpdateError::NotFound)));
    assert!(!root.join(format!("{}.history", workspace.id)).exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn every_update_requires_an_optimistic_revision_token() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    let result = store.update(
        &workspace.id,
        WorkspaceUpdate {
            code: Some("cube(2);".into()),
            name: None,
            start: None,
            end: None,
            text: None,
            expected_revision: None,
            base_revision: None,
            plate: None,
            tolerance: None,
            source: "browser".into(),
            revision_name: None,
        },
    );
    assert!(
        matches!(result, Err(UpdateError::Invalid(message)) if message.contains("required"))
    );
    assert_eq!(store.get(&workspace.id).unwrap().unwrap().revision, 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_updates_do_not_mutate_persisted_source() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    let result = store.update(
        &workspace.id,
        WorkspaceUpdate {
            code: Some("cube(;".into()),
            name: None,
            start: None,
            end: None,
            text: None,
            expected_revision: Some(1),
            base_revision: None,
            plate: None,
            tolerance: None,
            source: "ai".into(),
            revision_name: None,
        },
    );
    assert!(matches!(result, Err(UpdateError::Validation(_))));
    assert_eq!(store.get(&workspace.id).unwrap().unwrap().revision, 1);
    drop(store);
    let loaded = WorkspaceStore::open(root.clone())
        .unwrap()
        .get(&workspace.id)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.code, "cube(1);");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn initialize_negotiates_down_instead_of_rejecting_newer_clients() {
    // Versions we implement are echoed back verbatim, per the lifecycle spec.
    for supported in SUPPORTED_MCP_PROTOCOLS {
        assert_eq!(negotiated_mcp_protocol(supported), *supported);
    }
    // Anything newer — or older, or nonsense — gets our newest revision rather
    // than an error, so the client keeps its chance to fall back. Rejecting is
    // what made real clients (Claude Code) fail to connect at all.
    let newest = SUPPORTED_MCP_PROTOCOLS[SUPPORTED_MCP_PROTOCOLS.len() - 1];
    for requested in ["2026-07-28", "2025-11-25", "2024-11-05", "not-a-version", ""] {
        assert_eq!(negotiated_mcp_protocol(requested), newest);
    }
    // The list stays newest-last, or the line above hands out the oldest revision.
    let mut sorted = SUPPORTED_MCP_PROTOCOLS.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, SUPPORTED_MCP_PROTOCOLS.to_vec());
}

#[test]
fn absent_protocol_version_header_is_legal_but_an_unsupported_one_is_not() {
    // `initialize` is the request that *decides* the version, so no header
    // rule can apply to it — not even a nonsense one.
    assert_eq!(unsupported_legacy_header("initialize", None), None);
    assert_eq!(
        unsupported_legacy_header("initialize", Some("not-a-version")),
        None
    );

    // Absent header: clients older than 2025-06-18 never send it, and the
    // spec says to assume 2025-03-26 rather than reject. Rejecting here is
    // the bug this test exists to keep fixed.
    assert_eq!(unsupported_legacy_header("tools/list", None), None);

    // Every revision we negotiate must be accepted back as a header, or a
    // client that negotiated down would be cut off on its very next call.
    for supported in SUPPORTED_MCP_PROTOCOLS {
        assert_eq!(unsupported_legacy_header("tools/list", Some(supported)), None);
    }

    // Present but unspeakable: refused, and the caller turns this into the
    // 400 the spec mandates. `2026-07-28` counts as unsupported *here*
    // because a bare legacy request cannot be served under modern rules.
    for bad in ["2024-11-05", "2026-07-28", "not-a-version", ""] {
        assert_eq!(
            unsupported_legacy_header("tools/list", Some(bad)),
            Some(bad),
            "header {bad:?} should be refused"
        );
    }
}

#[test]
fn era_follows_how_the_client_opens_not_which_method_it_calls() {
    let modern = json!({"_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}});
    let bare = json!({});

    // `initialize` is the legacy opener and stays legacy even if a confused
    // client decorates it with modern metadata.
    assert_eq!(mcp_era("initialize", &bare), McpEra::Legacy);
    assert_eq!(mcp_era("initialize", &modern), McpEra::Legacy);
    assert_eq!(mcp_era("notifications/initialized", &bare), McpEra::Legacy);

    // A legacy post-handshake call carries no modern `_meta`, so the browser
    // UI's `tools/list` keeps its byte-compatible legacy responses.
    assert_eq!(mcp_era("tools/list", &bare), McpEra::Legacy);
    assert_eq!(
        mcp_era("tools/call", &json!({"_meta": {"progressToken": 7}})),
        McpEra::Legacy
    );

    // The modern `_meta` protocol version is the modern opener, and
    // `server/discover` exists only in the modern era, so a bare probe for
    // it must still be answered with a modern error rather than -32601.
    assert_eq!(mcp_era("tools/list", &modern), McpEra::Modern);
    assert_eq!(mcp_era("server/discover", &bare), McpEra::Modern);
    assert_eq!(mcp_era("server/discover", &modern), McpEra::Modern);
}

#[test]
fn discover_advertises_versions_capabilities_and_identity() {
    let result = mcp_discover_result();

    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"], json!(["2026-07-28"]));
    // Only modern revisions are listed: a client picking one of these keeps
    // sending modern `_meta`, which the legacy revisions cannot serve.
    for version in SUPPORTED_MCP_PROTOCOLS {
        assert!(!result["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!(version)));
    }
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(result["capabilities"]["resources"]["subscribe"], false);
    assert_eq!(result["_meta"][MCP_META_SERVER_INFO]["name"], "reopenscad");
    assert!(result["instructions"].as_str().unwrap().contains("SCAD"));
    // `server/discover` is a cacheable result like the lists.
    assert_eq!(result["ttlMs"], json!(MCP_STATIC_CACHE_TTL_MS));
    assert_eq!(result["cacheScope"], "public");
}

#[test]
fn unsupported_version_reports_what_the_client_can_retry_with() {
    assert_eq!(MCP_UNSUPPORTED_PROTOCOL_VERSION, -32022);
    let data = unsupported_protocol_version_data("1900-01-01");
    assert_eq!(
        data,
        json!({"supported": ["2026-07-28"], "requested": "1900-01-01"})
    );
    // Both MCP codes live in the range the spec reserves for itself.
    for code in [MCP_HEADER_MISMATCH, MCP_UNSUPPORTED_PROTOCOL_VERSION] {
        assert!((-32099..=-32020).contains(&code));
    }
}

#[test]
fn header_mirroring_is_validated_against_the_body() {
    let call = json!({
        "name": "render_workspace",
        "_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}
    });
    let ok = |params: &Value, protocol, method, name| {
        validate_modern_mcp_headers(
            "tools/call",
            params,
            Some(protocol),
            Some(method),
            Some(name),
        )
    };

    assert!(ok(&call, "2026-07-28", "tools/call", "render_workspace").is_ok());

    // Every standard header is required.
    assert!(validate_modern_mcp_headers(
        "tools/call",
        &call,
        None,
        Some("tools/call"),
        Some("render_workspace")
    )
    .is_err());
    assert!(validate_modern_mcp_headers(
        "tools/call",
        &call,
        Some("2026-07-28"),
        None,
        Some("render_workspace")
    )
    .is_err());
    assert!(validate_modern_mcp_headers(
        "tools/call",
        &call,
        Some("2026-07-28"),
        Some("tools/call"),
        None
    )
    .is_err());

    // A header that disagrees with the body is the case the rule exists for:
    // a gateway routing on the header must not reach a different decision
    // than the server executing the body.
    assert!(ok(&call, "2025-06-18", "tools/call", "render_workspace").is_err());
    assert!(ok(&call, "2026-07-28", "tools/list", "render_workspace").is_err());
    assert!(ok(&call, "2026-07-28", "tools/call", "create_workspace").is_err());

    // `Mcp-Name` mirrors `params.uri` for reads and is absent for the lists.
    let read = json!({
        "uri": SCAD_DICTIONARY_URI,
        "_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}
    });
    assert_eq!(
        mcp_name_body_value("resources/read", &read),
        Some(SCAD_DICTIONARY_URI)
    );
    assert_eq!(mcp_name_body_value("tools/list", &read), None);
    assert!(validate_modern_mcp_headers(
        "resources/read",
        &read,
        Some("2026-07-28"),
        Some("resources/read"),
        Some(SCAD_DICTIONARY_URI)
    )
    .is_ok());
    assert!(validate_modern_mcp_headers(
        "tools/list",
        &json!({"_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}}),
        Some("2026-07-28"),
        Some("tools/list"),
        None
    )
    .is_ok());

    // A name outside the header-safe ASCII set travels base64-wrapped, and
    // must be decoded before the comparison.
    let unicode = json!({
        "uri": "scad://reopenscad/wörld",
        "_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}
    });
    let encoded = format!("=?base64?{}?=", base64_encode("scad://reopenscad/wörld".as_bytes()));
    assert!(validate_modern_mcp_headers(
        "resources/read",
        &unicode,
        Some("2026-07-28"),
        Some("resources/read"),
        Some(&encoded)
    )
    .is_ok());
    assert!(validate_modern_mcp_headers(
        "resources/read",
        &unicode,
        Some("2026-07-28"),
        Some("resources/read"),
        Some("=?base64?not base64?=")
    )
    .is_err());
}

#[test]
fn decodes_the_base64_header_sentinel() {
    assert_eq!(
        decode_mcp_header_value("render_workspace").as_deref(),
        Some("render_workspace")
    );
    assert_eq!(
        decode_mcp_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
        Some("Hello, 世界")
    );
    assert_eq!(decode_mcp_header_value("=?base64?%%%%?="), None);
    for original in ["", "a", "ab", "abc", "ReOpenSCAD", "scad://x/ü"] {
        let round_tripped = base64_decode(&base64_encode(original.as_bytes())).unwrap();
        assert_eq!(round_tripped, original.as_bytes());
    }
}

#[test]
fn modern_results_carry_result_type_and_cache_hints() {
    // Ordinary results only gain `resultType` plus the server identity that
    // used to arrive once, at `initialize`.
    let call = modern_mcp_result(json!({"content": [], "isError": false}));
    assert_eq!(call["resultType"], "complete");
    assert_eq!(
        call["_meta"][MCP_META_SERVER_INFO]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(call.get("ttlMs").is_none());
    assert!(call.get("cacheScope").is_none());

    // The cacheable ones additionally MUST carry both hints.
    let list = cacheable_mcp_result(
        json!({"tools": mcp_tool_definitions()}),
        MCP_STATIC_CACHE_TTL_MS,
        "public",
    );
    assert_eq!(list["resultType"], "complete");
    assert_eq!(list["ttlMs"], json!(MCP_STATIC_CACHE_TTL_MS));
    assert_eq!(list["cacheScope"], "public");
    assert!(list["ttlMs"].as_u64().is_some(), "ttlMs must be >= 0");

    // A workspace read is per-workspace and changes on every edit, so it is
    // never shared across callers and never considered fresh.
    let read = cacheable_mcp_result(json!({"contents": []}), 0, "private");
    assert_eq!(read["ttlMs"], json!(0));
    assert_eq!(read["cacheScope"], "private");
    for scope in [&list["cacheScope"], &read["cacheScope"]] {
        assert!(matches!(scope.as_str(), Some("public" | "private")));
    }

    // Legacy results are untouched: no `resultType`, no `_meta`, no hints.
    let legacy = json!({"tools": mcp_tool_definitions()});
    assert!(legacy.get("resultType").is_none());
    assert!(legacy.get("_meta").is_none());
}

#[test]
fn memory_watchdog_reads_limits_and_resident_size() {
    assert_eq!(parse_statm_resident("123456 7890 12 1 0 4567 0\n"), Some(7890));
    assert_eq!(parse_statm_resident(""), None);
    assert_eq!(parse_cgroup_limit("2147483648\n"), Some(2_147_483_648));
    assert_eq!(parse_cgroup_limit("max\n"), None);
    assert_eq!(parse_cgroup_limit("9223372036854771712"), None);
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert!(resident_bytes().is_some_and(|bytes| bytes > 0));
    }
}

fn mcp_test_request() -> Request {
    Request {
        method: "POST".into(),
        path: "/mcp".into(),
        query: String::new(),
        headers: vec![("host".into(), "127.0.0.1:5173".into())],
        body: vec![],
    }
}

#[test]
fn identical_mcp_calls_reuse_one_job_and_the_jobs_tool_lists_it() {
    let (state, root) = temporary_state(None);
    let request = mcp_test_request();
    let workspace = state.workspaces.create("cube(2); // jobs-reuse\n".into(), None).unwrap();
    let arguments = json!({"designId": workspace.id, "checks": []});
    let first = crate::mcp_call_tool(&state, &request, "evaluate", arguments.clone());
    assert_eq!(first["isError"], false, "{first}");
    let mut second = crate::mcp_call_tool(&state, &request, "evaluate", arguments);
    assert_eq!(second["structuredContent"]["replayed"], true, "a cached reply says so");
    second["structuredContent"].as_object_mut().unwrap().remove("replayed");
    assert_eq!(first, second);
    let listed = crate::mcp_call_tool(&state, &request, "jobs", json!({"designId": workspace.id}));
    let jobs = listed["structuredContent"]["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "the second call reused the first job: {listed}");
    assert_eq!(jobs[0]["tool"], "evaluate");
    assert_eq!(jobs[0]["state"], "done");
    assert_eq!(jobs[0]["resultCached"], true);
    // A new revision is a new key.
    let updated = crate::mcp_call_tool(
        &state,
        &request,
        "update_design_code",
        json!({
            "designId": workspace.id,
            "expectedRevision": 1,
            "code": "cube(3); // jobs-reuse\n",
            "revisionName": "Test revision",
        }));
    assert_eq!(updated["structuredContent"]["revision"], 2, "{updated}");
    crate::mcp_call_tool(&state, &request, "evaluate", json!({"designId": workspace.id, "checks": []}));
    let listed = crate::mcp_call_tool(&state, &request, "jobs", json!({"designId": workspace.id}));
    let tools: Vec<_> = listed["structuredContent"]["jobs"].as_array().unwrap().iter().map(|job| job["tool"].clone()).collect();
    assert_eq!(tools, vec![json!("evaluate"), json!("update_design_code"), json!("evaluate")]);
    // Cancel needs a live job of this design.
    let refused = crate::mcp_call_tool(&state, &request, "jobs", json!({"designId": workspace.id, "cancel": "job-0-0000"}));
    assert_eq!(refused["isError"], true);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_replayed_write_returns_its_original_reply_instead_of_a_conflict() {
    let (state, root) = temporary_state(None);
    let request = mcp_test_request();
    let workspace = state.workspaces.create("cube(1);\n".into(), None).unwrap();
    let write = json!({"designId": workspace.id, "expectedRevision": 1, "code": "cube(5); // replay\n", "revisionName": "Test revision"});
    let first = crate::mcp_call_tool(&state, &request, "update_design_code", write.clone());
    assert_eq!(first["structuredContent"]["revision"], 2, "{first}");
    let mut replay = crate::mcp_call_tool(&state, &request, "update_design_code", write.clone());
    assert_eq!(replay["structuredContent"]["replayed"], true, "{replay}");
    assert!(replay["structuredContent"].get("currentRevision").is_none(), "still current: {replay}");
    replay["structuredContent"].as_object_mut().unwrap().remove("replayed");
    assert_eq!(first, replay, "a retry after a lost reply is idempotent");
    assert_eq!(state.workspaces.get(&workspace.id).unwrap().unwrap().revision, 2, "applied once");
    // A different write against the old revision still conflicts.
    let stale = crate::mcp_call_tool(
        &state,
        &request,
        "update_design_code",
        json!({
            "designId": workspace.id,
            "expectedRevision": 1,
            "code": "cube(6);\n",
            "revisionName": "Test revision",
        }));
    assert_eq!(stale["isError"], true, "{stale}");
    // Another write lands; replaying the first now says it is history.
    let next = crate::mcp_call_tool(
        &state,
        &request,
        "update_design_code",
        json!({
            "designId": workspace.id,
            "expectedRevision": 2,
            "code": "cube(7);\n",
            "revisionName": "Test revision",
        }));
    assert_eq!(next["structuredContent"]["revision"], 3, "{next}");
    let late = crate::mcp_call_tool(&state, &request, "update_design_code", write);
    let structured = &late["structuredContent"];
    assert_eq!(structured["replayed"], true, "{late}");
    assert_eq!(structured["revision"], 2);
    assert_eq!(structured["currentRevision"], 3);
    assert_ne!(structured["objectsStatus"], "current", "{late}");
    assert!(structured["replayNote"].as_str().unwrap().contains("revision 3"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_cached_read_is_keyed_by_the_designs_settings() {
    let (state, root) = temporary_state(None);
    let request = mcp_test_request();
    let workspace = state.workspaces.create("cube(2); // settings-key\n".into(), None).unwrap();
    let arguments = json!({"designId": workspace.id, "checks": []});
    let before = mcp_job_spec(&state, &request, "evaluate", &arguments).unwrap().key;
    state.workspaces.update_settings(&workspace.id, None, Some(0.5)).unwrap();
    assert_eq!(state.workspaces.get(&workspace.id).unwrap().unwrap().revision, 1, "no revision bump");
    let after = mcp_job_spec(&state, &request, "evaluate", &arguments).unwrap().key;
    assert_ne!(before, after, "the default tolerance is part of the key");
    state.workspaces.update_settings(&workspace.id, Some("bambu-x1c"), None).unwrap();
    assert_ne!(mcp_job_spec(&state, &request, "evaluate", &arguments).unwrap().key, after, "so is the plate");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn partial_and_retryable_replies_are_not_cacheable() {
    let ok = |structured: Value| json!({"content": [], "structuredContent": structured, "isError": false});
    assert!(mcp_result_cacheable(&ok(json!({"ok": true, "complete": true, "objectsStatus": "current", "skipped": 0, "truncated": 3}))));
    for partial in [
        json!({"complete": false}),
        json!({"intersections": {"pairs": [{"a": "x", "relation": "skipped"}]}}),
        json!({"variants": [{"ok": false, "skipped": true, "reason": "rate_limited"}]}),
        json!({"summary": {"skipped": 2}}),
        json!({"verdict": {"incomplete": ["supports"]}}),
        json!({"objectsStatus": "pending"}),
        json!({"objectsStatus": "failed"}),
        json!({"pair": {"clearanceSampling": {"truncated": true}}}),
        json!({"retryable": true}),
        json!({"printability": "incomplete"}),
        json!({"objects": [{"id": "a", "complete": false}]}),
        json!({"changes": {"skipped": "the 20 s time budget was used up"}}),
    ] {
        assert!(!mcp_result_cacheable(&ok(partial.clone())), "{partial}");
    }
    assert!(!mcp_result_cacheable(&json!({"isError": true})));
}

#[test]
fn mcp_cancellation_is_scoped_by_an_unguessable_session() {
    let with_session = |session: Option<&str>| {
        let mut request = mcp_test_request();
        if let Some(session) = session {
            request.headers.push(("mcp-session-id".into(), session.into()));
        }
        request
    };
    let id = json!(1);
    assert_eq!(mcp_rpc_scope(&with_session(None), &id), None, "no session, no notification scope");
    assert_eq!(mcp_rpc_scope(&with_session(Some("short")), &id), None, "a guessable id is no scope");
    let a = mcp_rpc_scope(&with_session(Some("0123456789abcdef0123456789abcdef")), &id).unwrap();
    let b = mcp_rpc_scope(&with_session(Some("fedcba9876543210fedcba9876543210")), &id).unwrap();
    assert_ne!(a, b, "two clients behind one address do not share a scope");
    // Another session's notification for the same id withdraws nothing.
    let registry = Arc::new(jobs::JobRegistry::new(Duration::from_secs(30), jobs::RESULT_TTL));
    let started = Arc::new(AtomicBool::new(false));
    let caller = {
        let (registry, started, a) = (Arc::clone(&registry), Arc::clone(&started), a.clone());
        thread::spawn(move || {
            registry.run(
                jobs::JobSpec { key: [7; 32], tool: "evaluate".into(), design: Some("d".into()) },
                jobs::Caller { gone: None, rpc: Some(a) },
                |_| {
                    started.store(true, Ordering::Release);
                    while csg::bsp::interrupt_message().is_none() {
                        thread::sleep(Duration::from_millis(5));
                    }
                    mcp_tool_error("Render cancelled.")
                },
                |_| true,
                mcp_tool_error,
            )
        })
    };
    while !started.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(registry.withdraw_rpc(&b), 0);
    assert_eq!(registry.withdraw_rpc(&a), 1);
    let (value, _) = caller.join().unwrap();
    assert_eq!(value["isError"], true);
}

#[test]
fn initialize_issues_a_session_id_and_calls_work_without_one() {
    let (state, root) = temporary_state(None);
    let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
    let response = serve_with_state(&state, post_json("/mcp", &initialize));
    let session = response
        .lines()
        .find_map(|line| line.strip_prefix("Mcp-Session-Id: "))
        .unwrap_or_else(|| panic!("no session header: {response}"))
        .trim()
        .to_string();
    assert!(valid_mcp_session_id(&session) && session.len() == 32, "{session}");
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    let response = serve_with_state(&state, post_json("/mcp", &list));
    assert!(response.starts_with("HTTP/1.1 200"), "a stateless client still works: {response}");
    let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2}});
    let response = serve_with_state(&state, post_json("/mcp", &cancel));
    assert!(response.starts_with("HTTP/1.1 202"), "{response}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_write_whose_objects_were_not_computed_reports_no_removals() {
    let (state, root) = temporary_state(None);
    let request = mcp_test_request();
    let workspace = state.workspaces.create("cube(1);\ntranslate([5,0,0]) cube(1);\n".into(), None).unwrap();
    let listed = crate::mcp_call_tool(&state, &request, "list_design_objects", json!({"designId": workspace.id}));
    assert_eq!(listed["isError"], false, "{listed}");
    WRITE_OBJECTS_BUDGET_OVERRIDE.with(|budget| budget.set(Some(Duration::ZERO)));
    let code = "difference() { sphere(20, $fn = 96); for (i = [0:20]) rotate([0, i * 9, i * 17]) cylinder(h = 50, r = 1.5, center = true, $fn = 32); } // removed\n";
    let reply = crate::mcp_call_tool(
        &state,
        &request,
        "update_design_code",
        json!({
            "designId": workspace.id,
            "expectedRevision": 1,
            "code": code,
            "revisionName": "Test revision",
        }));
    WRITE_OBJECTS_BUDGET_OVERRIDE.with(|budget| budget.set(None));
    let structured = &reply["structuredContent"];
    assert_eq!(structured["objectsStatus"], "pending", "{reply}");
    for key in ["objectCount", "objects", "objectsUnchanged", "objectsRemoved", "objectsShown"] {
        assert!(structured.get(key).is_none(), "{key} must be omitted: {reply}");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_object_keeps_unchanged_only_when_its_identity_does() {
    let object = json!({"id": "a", "name": "a", "triangles": 12, "bounds": {"min": [0, 0, 0], "max": [1, 1, 1]}, "sourceRange": {"start": 0, "end": 8}});
    let mut moved = object.clone();
    moved["sourceRange"] = json!({"start": 20, "end": 28});
    assert!(same_object(&object, &moved), "an edit above it only shifts its span");
    let mut renamed = object.clone();
    renamed["name"] = json!("lid");
    assert!(!same_object(&object, &renamed));
    let mut swapped = object.clone();
    swapped["sourceRange"] = json!({"start": 0, "end": 30});
    assert!(!same_object(&object, &swapped), "another call now owns the id");
    let mut recoloured = object.clone();
    recoloured["color"] = json!("red");
    assert!(!same_object(&object, &recoloured));
    let (_, value) = mcp_reply_objects(vec![renamed], Some(vec![object]), &json!({}));
    assert_eq!(value["objectsUnchanged"], 0, "{value}");
    assert_eq!(value["objects"][0]["name"], "lid");
}

#[test]
fn a_write_answers_without_meshing_past_its_object_budget() {
    let (state, root) = temporary_state(None);
    let request = mcp_test_request();
    let workspace = state.workspaces.create("cube(1);\n".into(), None).unwrap();
    WRITE_OBJECTS_BUDGET_OVERRIDE.with(|budget| budget.set(Some(Duration::ZERO)));
    let code = "difference() { sphere(20, $fn = 96); for (i = [0:20]) rotate([0, i * 9, i * 17]) cylinder(h = 50, r = 1.5, center = true, $fn = 32); } // budget\n";
    let started = Instant::now();
    let reply = crate::mcp_call_tool(
        &state,
        &request,
        "update_design_code",
        json!({
            "designId": workspace.id,
            "expectedRevision": 1,
            "code": code,
            "revisionName": "Test revision",
        }));
    WRITE_OBJECTS_BUDGET_OVERRIDE.with(|budget| budget.set(None));
    let structured = &reply["structuredContent"];
    assert_eq!(structured["ok"], true, "{reply}");
    assert_eq!(structured["revision"], 2);
    assert_eq!(structured["objectsStatus"], "pending", "{reply}");
    assert!(structured["objectsHint"].as_str().unwrap().contains("evaluate"));
    assert!(started.elapsed() < Duration::from_secs(2), "validation alone is fast: {:?}", started.elapsed());
    assert_eq!(state.workspaces.get(&workspace.id).unwrap().unwrap().code, code, "the write is saved");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn notifications_cancelled_is_accepted_without_a_matching_call() {
    // Unknown ids are a no-op, never an error.
    let mut request = mcp_test_request();
    withdraw_cancelled_mcp_request(&request, &json!({"requestId": 12345}));
    request.headers.push(("mcp-session-id".into(), "0123456789abcdef0123456789abcdef".into()));
    withdraw_cancelled_mcp_request(&request, &json!({"requestId": 12345}));
    withdraw_cancelled_mcp_request(&request, &json!({"requestId": {"nested": true}}));
}

#[test]
fn exposes_complete_mcp_workspace_tool_contract() {
    let definitions = mcp_tool_definitions();
    let names: Vec<_> = definitions
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(
        names,
        vec![
            "create_design",
            "get_supported_scad",
            "get_reverse_engineering_guide",
            "get_design_code",
            "update_design_code",
            "get_parameters",
            "set_text",
            "list_modules",
            "set_parameters",
            "render_design",
            "render_image",
            "list_design_objects",
            "measure",
            "evaluate",
            "diff",
            "list_revisions",
            "restore_revision",
            "get_design_link",
            "create_share_link",
            "check_intersections",
            "check_insertion",
            "describe_geometry",
            "probe",
            "section_profile",
            "fit_axis",
            "align_to",
            "place",
            "list_feedback",
            "request_pointing",
            "resolve_feedback",
            "upload_asset",
            "create_asset_upload",
            "list_assets",
            "delete_asset",
            "split_object",
            "set_print_orientation",
            "get_print_layout",
            "export_print_parts",
            "extract_features",
            "compare_surfaces",
            "measure_between",
            "compare_sections",
            "fit_curve",
            "fit_surface",
            "scan_report",
            "jobs",
            // Pre-rename names, still listed so existing agents keep working.
            "create_workspace",
            "get_workspace_code",
            "update_workspace_code",
            "render_workspace",
            "list_workspace_objects",
            "get_workspace_link"
        ]
    );
    // Canonical tools speak designId; their deprecated aliases workspaceId.
    let tool = |name: &str| definitions.iter().find(|tool| tool["name"] == name).unwrap().clone();
    assert_eq!(tool("get_design_code")["inputSchema"]["required"], json!(["designId"]));
    assert_eq!(tool("get_workspace_code")["inputSchema"]["required"], json!(["workspaceId"]));
    assert!(tool("render_workspace")["description"]
        .as_str()
        .unwrap()
        .starts_with("Deprecated alias of render_design"));
    for (alias, canonical) in MCP_TOOL_ALIASES {
        assert_eq!(canonical_mcp_tool_name(alias), *canonical);
        assert_eq!(mcp_tool_runs_evaluator(alias), mcp_tool_runs_evaluator(canonical));
    }
    for tool in &definitions {
        let text = tool["description"].as_str().unwrap();
        if !text.starts_with("Deprecated alias") {
            assert!(!text.to_lowercase().contains("workspace"), "{}: {text}", tool["name"]);
        }
    }
    assert!(definitions
        .iter()
        .all(|tool| tool.get("inputSchema").is_some()));
    let update = definitions
        .iter()
        .find(|tool| tool["name"] == "update_design_code")
        .unwrap();
    assert!(update["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .contains(&json!("expectedRevision")));
    let render = definitions
        .iter()
        .find(|tool| tool["name"] == "render_design")
        .unwrap();
    assert!(render["inputSchema"]["properties"].get("revision").is_none());
    assert!(update["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .contains(&json!("revisionName")));
    // The design records its own brief: the server instructions say so,
    // and the two tools that write code repeat it where the code is written.
    for marker in ["// Requirements:", "// Decisions:", "Never drop"] {
        assert!(MCP_INSTRUCTIONS.contains(marker), "{marker}");
    }
    for name in ["create_design", "update_design_code"] {
        let text = tool(name)["description"].as_str().unwrap().to_string();
        assert!(text.contains("// Requirements:") && text.contains("// Decisions:"), "{name}: {text}");
    }
    // Aliases point at their canonical tool instead of repeating it.
    assert!(tool("update_workspace_code")["description"].as_str().unwrap().len() < 200);
}

#[test]
fn parses_workspace_routes_and_query_ranges() {
    let id = "fuzzy-wombat-00112233445566778899aabbccdd";
    assert_eq!(
        workspace_api_route(&format!("/api/workspaces/{id}/events")),
        Some((id, "events"))
    );
    assert_eq!(
        workspace_api_route(&format!("/api/designs/{id}/events")),
        Some((id, "events"))
    );
    assert!(workspace_api_route(&format!("/api/designsx/{id}/events")).is_none());
    assert!(is_design_collection_route("/api/designs"));
    assert!(is_design_collection_route("/api/workspaces"));
    let parameters = query_parameters("since=42&format=stl");
    assert_eq!(query_u64(&parameters, "since"), Some(42));
    assert_eq!(parameters.get("format").map(String::as_str), Some("stl"));
    assert_eq!(
        workspace_shell_id("/designs/tidy-rocket-jy8p"),
        Some("tidy-rocket-jy8p")
    );
    assert_eq!(
        legacy_workspace_shell_id("/workspaces/tidy-rocket-jy8p"),
        Some("tidy-rocket-jy8p")
    );
    assert!(workspace_shell_id("/workspaces/tidy-rocket-jy8p").is_none());
    assert!(workspace_shell_id("/designs/../../backend/src/main.rs").is_none());
    assert!(legacy_workspace_shell_id("/workspaces/../../backend/src/main.rs").is_none());
    let request = Request {
        method: "GET".into(),
        path: format!("/api/workspaces/{id}"),
        query: "allowMissing=1".into(),
        headers: vec![],
        body: vec![],
    };
    assert!(allow_missing_workspace(&request));
}

#[test]
fn a_kernel_abort_in_the_selected_object_union_is_an_error_not_a_panic() {
    // The union runs after the compile, outside its guard: an expired
    // kernel deadline used to unwind through the connection thread.
    let source = "sphere(10, $fn = 64); translate([6, 0, 0]) sphere(10, $fn = 64);";
    let output = engine::compile_parts(source, engine::Quality::Render, None).unwrap();
    let ids = vec!["object-1".to_string(), "object-2".to_string()];
    let _expired = csg::bsp::deadline_scope(Instant::now() - Duration::from_secs(1));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        selected_parts_mesh(&output, Some(&ids), engine::Quality::Render, None)
    }));
    let error = result
        .expect("the kernel abort must not escape as a panic")
        .expect_err("an expired deadline cannot produce the union");
    assert!(error.contains("time budget"), "{error}");
}

#[test]
fn a_union_export_of_selected_objects_still_works_under_its_deadline() {
    let source = "cube(2); translate([1, 0, 0]) cube(2); translate([10, 0, 0]) cube(1);";
    let ids = vec!["object-1".to_string(), "object-2".to_string()];
    let mesh = export_mesh(source, Some(&ids)).unwrap();
    let max_x = mesh
        .triangles
        .iter()
        .flat_map(|t| t.vertices)
        .map(|v| v.x)
        .fold(f64::MIN, f64::max);
    assert!((max_x - 3.0).abs() < 1e-9, "{max_x}");
}

#[test]
fn exporting_only_degenerate_geometry_is_a_clear_error() {
    let options = ExportOptions::from_request("stl", &json!({}), &HashMap::new()).unwrap();
    let error = export_source("cube(0.0005);", None, options, "x").unwrap_err();
    assert_eq!(error, EXPORT_NOTHING_SOLID);
}

#[test]
fn export_mesh_filters_by_object_id_on_both_export_endpoints() {
    let source = "cube(2); translate([4,0,0]) cube(2);";
    // Triangle counts cannot separate the two: the mesher adapts its grid to the bounds it is
    // given, so a single small part can carry more facets than the whole model.
    let lowest_x = |mesh: &engine::Mesh| {
        mesh.triangles
            .iter()
            .flat_map(|triangle| triangle.vertices)
            .fold(f64::INFINITY, |lowest, vertex| lowest.min(vertex.x))
    };
    let whole = export_mesh(source, None).unwrap();
    let subset = export_mesh(source, Some(&["object-2".into()])).unwrap();
    assert!(!whole.triangles.is_empty());
    assert!(lowest_x(&whole) < 1.0);
    assert!(lowest_x(&subset) > 3.0);
    assert!(export_mesh(source, Some(&["model".into()]))
        .unwrap_err()
        .contains("Unknown"));
}

#[test]
fn every_advertised_export_format_encodes_and_3mf_is_a_zip_package() {
    let mesh = export_mesh("cube(2);", None).unwrap();
    assert!(is_supported_export_format("3mf"));
    assert!(!is_supported_export_format("amf"));
    for format in EXPORT_FORMATS {
        let data = encode_mesh(&mesh, format, "unit test").expect("encodes");
        assert!(!data.is_empty(), "{format} produced no bytes");
    }
    // The tool schema and the validator must not drift apart: a format the
    // schema offers has to be one `encode_mesh` can actually produce.
    let render_tool = mcp_tool_definitions()
        .into_iter()
        .find(|tool| tool["name"] == "render_design")
        .expect("render_design tool");
    let advertised = render_tool["inputSchema"]["properties"]["format"]["enum"]
        .as_array()
        .expect("format enum")
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(advertised, EXPORT_FORMATS.map(String::from).to_vec());

    let package = encode_mesh(&mesh, "3mf", "unit test").expect("encodes");
    assert_eq!(&package[..4], b"PK\x03\x04", "3MF must be a ZIP container");
    assert_eq!(export_content_type("3mf"), "model/3mf");

    // Every format's magic is what tells a consumer what it received, and
    // the content type is what tells the browser. Both are easy to wire to
    // the wrong encoder, so both are pinned here rather than only inside
    // the writers' own tests.
    let step = encode_mesh(&mesh, "step", "unit test").expect("encodes");
    assert!(step.starts_with(b"ISO-10303-21;"), "STEP must be Part 21");
    assert_eq!(export_content_type("step"), "model/step");
    let glb = encode_mesh(&mesh, "glb", "unit test").expect("encodes");
    assert_eq!(&glb[..4], b"glTF", "GLB must carry the glTF magic");
    assert_eq!(export_content_type("glb"), "model/gltf-binary");
    // Format ids are also file extensions, so they have to be safe in a
    // `Content-Disposition` filename without quoting.
    for format in EXPORT_FORMATS {
        assert!(
            format.chars().all(|c| c.is_ascii_alphanumeric()),
            "{format} is used verbatim as model.{format}"
        );
    }
}

#[test]
fn mcp_exposes_object_discovery_and_the_embedded_app() {
    let names: Vec<_> = mcp_tool_definitions()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"list_design_objects".to_string()),
        "an MCP client has no other way to learn object IDs"
    );
    let template = mcp_resource_templates();
    assert_eq!(
        template[0]["uriTemplate"],
        json!("ui://reopenscad/design/{designId}")
    );
    assert_eq!(
        template[1]["uriTemplate"],
        json!("ui://reopenscad/workspace/{workspaceId}")
    );
    assert_eq!(template[0]["mimeType"], json!("text/html"));
    // Workspace URLs are access capabilities, so the concrete listing must
    // never enumerate them.
    for resource in mcp_resource_definitions() {
        assert!(!resource["uri"]
            .as_str()
            .unwrap()
            .starts_with(WORKSPACE_APP_URI_PREFIX));
    }
}

#[test]
fn selected_exports_validate_ids_and_preserve_union_semantics() {
    let output = engine::compile_parts(
        "cube(2); translate([4,0,0]) cube(2);",
        engine::Quality::Preview,
        None,
    )
    .unwrap();
    assert!(selected_parts_mesh(&output, Some(&[]), engine::Quality::Preview, None).is_err());
    assert!(selected_parts_mesh(
        &output,
        Some(&["missing".into()]),
        engine::Quality::Preview,
        None
    )
    .unwrap_err()
    .contains("Unknown"));
    assert!(selected_parts_mesh(
        &output,
        Some(&["object-1".into(), "object-1".into()]),
        engine::Quality::Preview,
        None
    )
    .is_err());
    let subset = selected_parts_mesh(
        &output,
        Some(&["object-1".into()]),
        engine::Quality::Preview,
        None,
    )
    .unwrap();
    assert!(!subset.triangles.is_empty());
    assert_ne!(subset.binary_stl(), output.mesh.binary_stl());
    let all = vec!["object-1".into(), "object-2".into()];
    assert_eq!(
        selected_parts_mesh(&output, Some(&all), engine::Quality::Preview, None)
            .unwrap()
            .binary_stl(),
        output.mesh.binary_stl()
    );
    let (state, root) = temporary_state(None);
    let request = Request {
        method: "POST".into(),
        path: "/mcp".into(),
        query: String::new(),
        headers: vec![("host".into(), "127.0.0.1:5173".into())],
        body: vec![],
    };
    assert!(workspace_export_url(
        &state,
        &request,
        "fuzzy-wombat-00112233445566778899aabbccdd",
        "stl",
        7,
        Some(&["object-1".into(), "object-2".into()])
    )
    .ends_with("format=stl&revision=7&objectIds=object-1,object-2"));
    fs::remove_dir_all(root).unwrap();
}

/// Two touching cubes, one red and one blue: the minimal repro of "export
/// merges everything into one body".
const TWO_TOUCHING_CUBES: &str =
    "color([1,0,0]) cube(10); color([0,0,1]) translate([10,0,0]) cube(10);";

fn export_test(source: &str, format: &str, extra: Value, ids: Option<&[&str]>) -> Vec<u8> {
    let options = ExportOptions::from_request(format, &extra, &HashMap::new()).unwrap();
    let ids: Option<Vec<String>> = ids.map(|ids| ids.iter().map(|id| id.to_string()).collect());
    export_source(source, ids.as_deref(), options, "pair").unwrap().0
}

/// Agent gap 11: the export cleanup report reaches the client instead of
/// being discarded — as headers on HTTP exports.
#[test]
fn exports_report_remaining_slivers() {
    let options = ExportOptions::from_request("stl", &json!({}), &HashMap::new()).unwrap();
    let (_, report) = export_source("cube(2);", None, options, "x").unwrap();
    assert_eq!(report.remaining, 0);
    let body = json!({"code": "cube(2);", "format": "stl"}).to_string();
    let response = serve_over_socket(&format!(
        "POST /api/export HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    let headers = response.split_once("\r\n\r\n").unwrap().0.to_ascii_lowercase();
    assert!(headers.starts_with("http/1.1 200"), "{headers}");
    assert!(headers.contains("x-reopenscad-slivers: 0"), "{headers}");
    assert!(headers.contains("x-reopenscad-sliver-repairs: "), "{headers}");
    let value = cleanup_value(&CleanupReport { collapses: 1, flips: 2, remaining: 3 });
    assert_eq!(value["remainingSlivers"], 3);
}

/// Facets of a binary STL as (min x, max x) per facet plus the count.
fn stl_facets(data: &[u8]) -> Vec<[[f32; 3]; 3]> {
    let count = u32::from_le_bytes(data[80..84].try_into().unwrap()) as usize;
    assert_eq!(data.len(), 84 + count * 50);
    (0..count)
        .map(|index| {
            let base = 84 + index * 50 + 12;
            std::array::from_fn(|corner| {
                std::array::from_fn(|axis| {
                    let at = base + corner * 12 + axis * 4;
                    f32::from_le_bytes(data[at..at + 4].try_into().unwrap())
                })
            })
        })
        .collect()
}

/// Every directed edge of a facet list has its twin: a closed shell.
fn closed(facets: &[[[f32; 3]; 3]]) -> bool {
    let key = |p: [f32; 3]| p.map(f32::to_bits);
    let mut edges = HashMap::new();
    for facet in facets {
        for corner in 0..3 {
            *edges
                .entry((key(facet[corner]), key(facet[(corner + 1) % 3])))
                .or_insert(0i32) += 1;
        }
    }
    edges
        .iter()
        .all(|((from, to), count)| *count == 1 && edges.get(&(*to, *from)) == Some(&1))
}

/// Stored entries of a ZIP package, read through the local headers.
fn stored_entries(package: &[u8]) -> HashMap<String, String> {
    let mut entries = HashMap::new();
    let mut cursor = 0;
    while package[cursor..cursor + 4] == *b"PK\x03\x04" {
        let size =
            u32::from_le_bytes(package[cursor + 18..cursor + 22].try_into().unwrap()) as usize;
        let name_length =
            u16::from_le_bytes(package[cursor + 26..cursor + 28].try_into().unwrap()) as usize;
        let extra_length =
            u16::from_le_bytes(package[cursor + 28..cursor + 30].try_into().unwrap()) as usize;
        let name = String::from_utf8(package[cursor + 30..cursor + 30 + name_length].to_vec())
            .unwrap();
        let start = cursor + 30 + name_length + extra_length;
        entries.insert(
            name,
            String::from_utf8_lossy(&package[start..start + size]).into_owned(),
        );
        cursor = start + size;
    }
    entries
}

fn glb_json(glb: &[u8]) -> Value {
    assert_eq!(&glb[..4], b"glTF");
    let length = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    serde_json::from_slice(&glb[20..20 + length]).unwrap()
}

#[test]
fn touching_objects_export_as_separate_shells_in_one_stl() {
    let facets = stl_facets(&export_test(TWO_TOUCHING_CUBES, "stl", json!({}), None));
    assert_eq!(facets.len(), 24, "two cubes, not one fused 20x10x10 box");
    let (first, second) = facets.split_at(12);
    assert!(closed(first) && closed(second), "each body is its own closed shell");
    let max_x = |facets: &[[[f32; 3]; 3]]| {
        facets.iter().flatten().map(|p| p[0]).fold(f32::MIN, f32::max)
    };
    let min_x = |facets: &[[[f32; 3]; 3]]| {
        facets.iter().flatten().map(|p| p[0]).fold(f32::MAX, f32::min)
    };
    // World coordinates are kept, so the shells still touch at x = 10.
    assert_eq!((min_x(first), max_x(first)), (0.0, 10.0));
    assert_eq!((min_x(second), max_x(second)), (10.0, 20.0));

    // `union: true` is the historical single body.
    let fused = stl_facets(&export_test(TWO_TOUCHING_CUBES, "stl", json!({"union": true}), None));
    assert_eq!(fused.len(), 12);
    assert!(closed(&fused));

    // One selected object is exactly that object.
    let one = stl_facets(&export_test(TWO_TOUCHING_CUBES, "stl", json!({}), Some(&["object-2"])));
    assert_eq!(one.len(), 12);
    assert_eq!(min_x(&one), 10.0);
}

#[test]
fn ascii_stl_writes_one_named_solid_per_object() {
    let parts = engine::compile_parts(TWO_TOUCHING_CUBES, engine::Quality::Render, None).unwrap();
    let text = String::from_utf8(export_test(
        TWO_TOUCHING_CUBES,
        "stl",
        json!({"encoding": "ascii"}),
        None,
    ))
    .unwrap();
    assert_eq!(text.matches("\nendsolid ").count() + usize::from(text.starts_with("endsolid")), 2);
    assert_eq!(text.matches("facet normal").count(), 24);
    for part in &parts.parts {
        let name = part.name.replace(' ', "_");
        assert!(text.contains(&format!("solid {name}\n")), "{name}");
    }
    assert!(ExportOptions::from_request("3mf", &json!({"encoding": "ascii"}), &HashMap::new())
        .is_err());
}

#[test]
fn three_mf_has_one_named_coloured_part_per_object_in_one_model() {
    let parts = engine::compile_parts(TWO_TOUCHING_CUBES, engine::Quality::Render, None).unwrap();
    let entries = stored_entries(&export_test(TWO_TOUCHING_CUBES, "3mf", json!({}), None));
    let model = &entries["3D/3dmodel.model"];
    assert_eq!(model.matches("<object ").count(), 3, "two parts plus their assembly");
    assert_eq!(model.matches("<mesh>").count(), 2);
    assert_eq!(model.matches("<triangle ").count(), 24);
    for part in &parts.parts {
        assert!(model.contains(&format!("name=\"{}\"", part.name)), "{}", part.name);
    }
    assert!(model.contains("<basematerials id=\"1\">"));
    assert!(model.contains("displaycolor=\"#FF0000\""));
    assert!(model.contains("displaycolor=\"#0000FF\""));
    assert!(model.contains("<object id=\"2\" type=\"model\" name=") && model.contains("pid=\"1\" pindex=\"0\""));
    assert!(model.contains("pid=\"1\" pindex=\"1\""));
    assert!(model.contains("<component objectid=\"2\" />"));
    assert!(model.contains("<component objectid=\"3\" />"));
    assert!(model.contains("<item objectid=\"4\" />"));
    assert_eq!(model.matches("<item ").count(), 1);
    // Bambu Studio / OrcaSlicer: one filament per colour.
    let settings = &entries["Metadata/model_settings.config"];
    assert!(settings.contains("<object id=\"4\">"));
    assert!(settings.contains("<part id=\"2\" subtype=\"normal_part\">"));
    assert!(settings.contains("<metadata key=\"extruder\" value=\"2\"/>"));
    assert!(entries["[Content_Types].xml"].contains("Extension=\"config\""));

    // A single object keeps the historical one-object package.
    let single = stored_entries(&export_test("cube(2);", "3mf", json!({}), None));
    assert_eq!(single.len(), 3);
    assert!(single["3D/3dmodel.model"].contains("<item objectid=\"1\" />"));
    assert!(!single["3D/3dmodel.model"].contains("<components>"));
}

#[test]
fn glb_has_one_named_node_mesh_and_material_per_object() {
    let json = glb_json(&export_test(TWO_TOUCHING_CUBES, "glb", json!({}), None));
    assert_eq!(json["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(json["nodes"][0]["name"], "pair");
    assert_eq!(json["nodes"][0]["children"], json!([1, 2]));
    assert_eq!(json["meshes"].as_array().unwrap().len(), 2);
    assert_eq!(json["materials"].as_array().unwrap().len(), 2);
    let factor: Vec<f64> = json["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_f64().unwrap())
        .collect();
    assert_eq!(factor, vec![1.0, 0.0, 0.0, 1.0]);
    assert_eq!(json["meshes"][1]["primitives"][0]["material"], 1);
    assert_eq!(json["accessors"][2]["count"], 36);
    assert_eq!(json["accessors"][5]["count"], 36);
}

#[test]
fn step_obj_and_off_keep_objects_apart() {
    let step = String::from_utf8(export_test(TWO_TOUCHING_CUBES, "step", json!({}), None)).unwrap();
    let parts = engine::compile_parts(TWO_TOUCHING_CUBES, engine::Quality::Render, None).unwrap();
    for part in &parts.parts {
        let solid = format!("MANIFOLD_SOLID_BREP('{}'", part.name);
        assert_eq!(step.matches(&solid).count(), 1, "{solid}");
    }
    assert_eq!(step.matches("STYLED_ITEM(").count(), 2);
    assert!(step.contains("COLOUR_RGB('',1.,0.,0.)"));
    assert_eq!(step.matches("ADVANCED_FACE(").count(), 12, "6 faces per cube, none merged");

    let obj = String::from_utf8(export_test(TWO_TOUCHING_CUBES, "obj", json!({}), None)).unwrap();
    assert_eq!(obj.lines().filter(|line| line.starts_with("o ")).count(), 2);
    // 8 corners per cube, never welded across the two.
    assert_eq!(obj.lines().filter(|line| line.starts_with("v ")).count(), 16);
    assert!(obj.contains(" 1 0 0\n") && obj.contains(" 0 0 1\n"), "vertex colours");
    assert_eq!(obj.lines().filter(|line| line.starts_with("f ")).count(), 24);

    let off = String::from_utf8(export_test(TWO_TOUCHING_CUBES, "off", json!({}), None)).unwrap();
    assert!(off.starts_with("OFF\n16 24 0\n"));
    assert!(off.contains(" 255 0 0 255\n") && off.contains(" 0 0 255 255\n"));
}

#[test]
fn single_object_exports_keep_their_format() {
    let obj = String::from_utf8(export_test("cube(2);", "obj", json!({}), None)).unwrap();
    assert!(!obj.contains("\no "), "a lone uncoloured body needs no group");
    let off = String::from_utf8(export_test("cube(2);", "off", json!({}), None)).unwrap();
    assert!(off.starts_with("OFF\n8 12 0\n"));
    assert!(off.lines().last().unwrap().split(' ').count() == 4, "no colour columns");
    let json = glb_json(&export_test("cube(2);", "glb", json!({}), None));
    assert_eq!(json["nodes"].as_array().unwrap().len(), 1);
    // The lone object keeps its own name, not the document title.
    assert_eq!(json["nodes"][0]["name"], "cube 1");
    assert_eq!(json["meshes"][0]["name"], "cube 1");
    assert_eq!(json["materials"][0]["name"], "cube 1");
    for format in EXPORT_FORMATS {
        assert!(!export_test("cube(2);", format, json!({}), None).is_empty());
    }
}

#[test]
fn export_options_validate_their_fields() {
    let none = HashMap::new();
    assert!(ExportOptions::from_request("amf", &json!({}), &none).is_err());
    assert!(ExportOptions::from_request("stl", &json!({"union": "yes"}), &none).is_err());
    assert!(ExportOptions::from_request("stl", &json!({"encoding": "utf8"}), &none).is_err());
    let query = HashMap::from([
        ("union".to_string(), "true".to_string()),
        ("encoding".to_string(), "ascii".to_string()),
    ]);
    let options = ExportOptions::from_request("stl", &json!({}), &query).unwrap();
    assert!(options.union);
    assert_eq!(options.format, export::Format::StlAscii);
    assert!(export_source("cube(1);", Some(&[]), options, "x").is_err());
    assert!(export_source("cube(1);", Some(&["object-9".into()]), options, "x")
        .unwrap_err()
        .contains("Unknown"));
    let separate = ExportOptions::from_request("stl", &json!({}), &none).unwrap();
    assert!(export_source(
        TWO_TOUCHING_CUBES,
        Some(&["object-1".into(), "object-1".into()]),
        separate,
        "x"
    )
    .unwrap_err()
    .contains("Duplicate"));
}

#[test]
fn public_origins_are_explicit_and_path_free() {
    assert_eq!(
        validate_public_origin("https://cad.example/").unwrap(),
        "https://cad.example"
    );
    assert!(validate_public_origin("https://cad.example/path").is_err());
    assert!(validate_public_origin("javascript:alert(1)").is_err());
}

// -----------------------------------------------------------------------
// Denial-of-service guard rails.
// -----------------------------------------------------------------------

/// Feeds `payload` to `read_request` over a real loopback socket.
fn parse_over_socket(payload: &[u8]) -> Result<Request, RequestError> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let payload = payload.to_vec();
    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        // The server may bail mid-stream; a broken pipe is expected then.
        let _ = stream.write_all(&payload);
        let _ = stream.flush();
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    });
    let (stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let parsed = read_request(&stream, &mut |_, _| Ok(()));
    drop(stream);
    let _ = client.join();
    parsed
}

#[test]
fn accepts_an_ordinary_request_after_hardening() {
    let request =
        parse_over_socket(b"POST /api/render HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/render");
    assert_eq!(request.body, b"hello");
}

#[test]
fn rejects_oversized_header_blocks() {
    let mut payload = b"GET /api/health HTTP/1.1\r\nHost: x\r\n".to_vec();
    // Comfortably past MAX_HEADER_BYTES but far below what the old
    // unbounded loop would happily buffer.
    for index in 0..64 {
        payload.extend_from_slice(format!("X-Pad-{index}: {}\r\n", "A".repeat(1024)).as_bytes());
    }
    payload.extend_from_slice(b"\r\n");
    let error = parse_over_socket(&payload).unwrap_err();
    assert_eq!(error.status, 431);
    assert!(error.message.contains("16 KiB"), "{}", error.message);
}

#[test]
fn rejects_a_single_oversized_header_line() {
    let mut payload = b"GET /api/health HTTP/1.1\r\n".to_vec();
    payload.extend_from_slice(b"X-Pad: ");
    payload.extend_from_slice(&vec![b'A'; MAX_HEADER_BYTES + 4096]);
    payload.extend_from_slice(b"\r\n\r\n");
    assert_eq!(parse_over_socket(&payload).unwrap_err().status, 431);
}

#[test]
fn rejects_too_many_headers() {
    let mut payload = b"GET /api/health HTTP/1.1\r\n".to_vec();
    for index in 0..(MAX_HEADERS + 10) {
        payload.extend_from_slice(format!("X-N-{index}: 1\r\n").as_bytes());
    }
    payload.extend_from_slice(b"\r\n");
    let error = parse_over_socket(&payload).unwrap_err();
    assert_eq!(error.status, 431);
    assert!(error.message.contains("Too many"), "{}", error.message);
}

#[test]
fn rejects_an_oversized_request_line() {
    let mut payload = b"GET /".to_vec();
    payload.extend_from_slice(&vec![b'a'; MAX_REQUEST_LINE + 1024]);
    payload.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(parse_over_socket(&payload).unwrap_err().status, 414);
}

#[test]
fn rejects_an_oversized_declared_body() {
    let payload = format!(
        "POST /api/render HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        MAX_BODY + 1
    );
    // Rejected from the header itself: no bytes of the body are buffered.
    assert_eq!(
        parse_over_socket(payload.as_bytes()).unwrap_err().status,
        413
    );
}

#[test]
fn connection_limiter_sheds_instead_of_growing() {
    let limiter = JobLimiter::new(3);
    let held: Vec<_> = (0..3).map(|_| limiter.try_acquire().unwrap()).collect();
    assert!(
        limiter.try_acquire().is_none(),
        "accept loop must shed past MAX_CONNECTIONS"
    );
    drop(held);
    assert!(limiter.try_acquire().is_some(), "slots must be reusable");
}

#[test]
fn rate_limiter_throttles_request_floods() {
    let limiter = RateLimiter::new();
    let address: IpAddr = "203.0.113.7".parse().unwrap();
    let now = Instant::now();
    for _ in 0..(IP_REQUEST_BURST as usize) {
        assert!(limiter.admit_at(address, RouteCost::Cheap, now));
    }
    assert!(!limiter.admit_at(address, RouteCost::Cheap, now));
    // A different source is unaffected.
    let other: IpAddr = "203.0.113.8".parse().unwrap();
    assert!(limiter.admit_at(other, RouteCost::Cheap, now));
    // Tokens come back with time.
    let later = now + Duration::from_secs(2);
    assert!(limiter.admit_at(address, RouteCost::Cheap, later));
}

#[test]
fn rate_limiter_is_stricter_on_expensive_routes() {
    let limiter = RateLimiter::new();
    let address: IpAddr = "198.51.100.4".parse().unwrap();
    let now = Instant::now();
    let allowed = (IP_HEAVY_BURST / HEAVY_COST_EVALUATOR) as usize;
    for _ in 0..allowed {
        assert!(limiter.admit_at(address, RouteCost::Evaluator, now));
    }
    assert!(!limiter.admit_at(address, RouteCost::Evaluator, now));
    // Cheap routes still work: the heavy bucket is separate.
    assert!(limiter.admit_at(address, RouteCost::Cheap, now));
}

#[test]
fn rate_limiter_bills_bytes_served() {
    let limiter = RateLimiter::new();
    let address: IpAddr = "198.51.100.9".parse().unwrap();
    let now = Instant::now();
    assert!(limiter.admit_at(address, RouteCost::Evaluator, now));
    // One amplified response drains the whole byte budget.
    limiter.charge_bytes(address, IP_BYTE_BURST as u64 + 1);
    assert!(
        !limiter.admit_at(address, RouteCost::Cheap, now),
        "byte budget must gate the next request"
    );
}

#[test]
fn rate_limits_are_configurable_and_validated() {
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    assert_eq!(RateLimits::from_lookup(|_| None).unwrap(), RateLimits::default());
    let limits = RateLimits::from_lookup(lookup(&[
        ("REOPENSCAD_IP_HEAVY_BURST", "600"),
        ("REOPENSCAD_IP_HEAVY_REFILL_PER_SEC", " 60 "),
        ("REOPENSCAD_IP_REQUEST_BURST", "1000"),
        ("REOPENSCAD_IP_REQUEST_REFILL_PER_SEC", "200"),
        ("REOPENSCAD_IP_BYTE_BURST_MIB", "512"),
        ("REOPENSCAD_IP_BYTE_REFILL_MIB_PER_SEC", "64"),
    ]))
    .unwrap();
    assert_eq!(limits.heavy_burst, 600.0);
    assert_eq!(limits.heavy_refill_per_sec, 60.0);
    assert_eq!(limits.request_burst, 1000.0);
    assert_eq!(limits.request_refill_per_sec, 200.0);
    assert_eq!(limits.byte_burst, 512.0 * MIB);
    assert_eq!(limits.byte_refill_per_sec, 64.0 * MIB);
    for (name, value) in [
        ("REOPENSCAD_IP_HEAVY_BURST", "lots"),
        // Below one evaluator request: nothing could ever be evaluated.
        ("REOPENSCAD_IP_HEAVY_BURST", "4"),
        ("REOPENSCAD_IP_HEAVY_REFILL_PER_SEC", "0"),
        ("REOPENSCAD_IP_REQUEST_BURST", "NaN"),
        ("REOPENSCAD_IP_BYTE_BURST_MIB", "1e9"),
    ] {
        let error = RateLimits::from_lookup(|key| (key == name).then(|| value.to_string()))
            .expect_err(value);
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn rate_limited_answers_name_the_bucket_and_the_exact_wait() {
    let limits = RateLimits {
        heavy_burst: 10.0,
        heavy_refill_per_sec: 2.0,
        ..RateLimits::default()
    };
    let limiter = RateLimiter::with_limits(limits);
    let address: IpAddr = "198.51.100.20".parse().unwrap();
    let now = Instant::now();
    assert!(limiter.check_at(address, RouteCost::Evaluator, now).is_ok());
    assert!(limiter.check_at(address, RouteCost::Evaluator, now).is_ok());
    let limited = limiter
        .check_at(address, RouteCost::Evaluator, now)
        .unwrap_err();
    assert_eq!(limited.bucket, "compute");
    // Empty bucket, 5 tokens needed at 2 per second.
    assert!((limited.retry_after - 2.5).abs() < 1e-9, "{limited:?}");
    assert_eq!(limited.retry_after_seconds(), 3);
    let detail = limited.detail();
    assert_eq!(detail["bucket"], json!("compute"));
    assert_eq!(detail["retryAfterSeconds"], json!(3));
    assert_eq!(detail["retryAfterMs"], json!(2500));
    assert!(limited.explanation().contains("compute budget"));
    assert!(limited.explanation().contains("Wait 3 s"));
    // Half a second later the wait has shrunk by exactly that much.
    let later = limiter
        .check_at(address, RouteCost::Evaluator, now + Duration::from_millis(500))
        .unwrap_err();
    assert!((later.retry_after - 2.0).abs() < 1e-9, "{later:?}");
    assert!(limiter
        .check_at(address, RouteCost::Evaluator, now + Duration::from_millis(2500))
        .is_ok());

    // The request bucket is named as such.
    let limiter = RateLimiter::with_limits(RateLimits {
        request_burst: 1.0,
        request_refill_per_sec: 4.0,
        ..RateLimits::default()
    });
    assert!(limiter.check_at(address, RouteCost::Cheap, now).is_ok());
    let limited = limiter.check_at(address, RouteCost::Cheap, now).unwrap_err();
    assert_eq!(limited.bucket, "requests");
    assert!((limited.retry_after - 0.25).abs() < 1e-9);
    assert_eq!(limited.retry_after_seconds(), 1);
}

#[test]
fn rate_limited_http_response_carries_the_bucket_and_retry_after() {
    let (mut state, root) = temporary_state(None);
    state.rate_limiter = RateLimiter::with_limits(RateLimits {
        heavy_burst: 5.0,
        heavy_refill_per_sec: 0.5,
        ..RateLimits::default()
    });
    let body = r#"{"code":"cube(1);","requestId":"rate0000001"}"#;
    let raw = format!(
        "POST /api/render HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let send = |state: &AppState| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let raw = raw.clone();
        let client = thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            String::from_utf8_lossy(&response).into_owned()
        });
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut peer = Some("192.0.2.200".parse().unwrap());
        serve_request(&mut stream, state, &mut peer).unwrap();
        drop(stream);
        client.join().unwrap()
    };
    assert!(send(&state).starts_with("HTTP/1.1 200"));
    let response = send(&state);
    assert!(response.starts_with("HTTP/1.1 429"), "{response}");
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    // 5 tokens at 0.5 per second: 10 s, give or take the time elapsed.
    assert!(head.contains("Retry-After: 10"), "{head}");
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["bucket"], json!("compute"));
    assert_eq!(body["reason"], json!("rate_limited"));
    assert_eq!(body["codeProblem"], json!(false));
    assert_eq!(body["retryAfterSeconds"], json!(10));
    assert_eq!(body["limit"]["refillPerSecond"], json!(0.5));
    set_client_key(None);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rate_limiter_memory_is_bounded() {
    let limiter = RateLimiter::new();
    let now = Instant::now();
    for index in 0..(MAX_RATE_BUCKETS as u32 + 500) {
        let address = IpAddr::V4(std::net::Ipv4Addr::from(index));
        limiter.admit_at(address, RouteCost::Cheap, now);
    }
    assert!(
        limiter.tracked() <= MAX_RATE_BUCKETS,
        "tracked {} buckets",
        limiter.tracked()
    );
}

#[test]
fn classifies_route_costs() {
    assert_eq!(route_cost("GET", "/index.html"), RouteCost::Cheap);
    assert_eq!(route_cost("GET", "/api/health"), RouteCost::Cheap);
    assert_eq!(route_cost("POST", "/api/render"), RouteCost::Evaluator);
    assert_eq!(route_cost("POST", "/api/export"), RouteCost::Evaluator);
    assert_eq!(route_cost("POST", "/mcp"), RouteCost::Evaluator);
    assert_eq!(route_cost("POST", "/api/workspaces"), RouteCost::Evaluator);
    assert_eq!(route_cost("POST", "/api/designs"), RouteCost::Evaluator);
    let id = "sunny-otter-0011223344556677";
    // The pre-rename alias must be metered exactly like the canonical
    // route, or it would be a way around the limits.
    for (method, action) in [
        ("GET", ""), ("PATCH", ""), ("PUT", ""), ("POST", ""), ("GET", "/events"),
        ("POST", "/render"), ("GET", "/export"), ("POST", "/export"), ("PUT", "/preview"),
        ("GET", "/preview"), ("GET", "/code"), ("POST", "/share"), ("GET", "/revisions"),
        ("POST", "/objects"), ("POST", "/measure"), ("POST", "/intersections"),
        ("POST", "/image"), ("POST", "/diff"), ("GET", "/parameters"),
        ("POST", "/parameters"), ("POST", "/evaluate"),
    ] {
        assert_eq!(
            route_cost(method, &format!("/api/designs/{id}{action}")),
            route_cost(method, &format!("/api/workspaces/{id}{action}")),
            "{method} {action}"
        );
    }
    assert_eq!(
        route_cost("POST", &format!("/api/designs/{id}/evaluate")),
        RouteCost::Evaluator
    );
    assert_eq!(
        route_cost("GET", &format!("/api/workspaces/{id}/events")),
        RouteCost::Poll
    );
    assert_eq!(
        route_cost("POST", &format!("/api/workspaces/{id}/render")),
        RouteCost::Evaluator
    );
    assert_eq!(
        route_cost("PATCH", &format!("/api/workspaces/{id}")),
        RouteCost::Evaluator
    );
    assert_eq!(
        route_cost("GET", &format!("/api/workspaces/{id}")),
        RouteCost::Cheap
    );
}

#[test]
fn events_payload_omits_source_when_nothing_changed() {
    let workspace = Workspace {
        id: "sunny-otter-0011223344556677".into(),
        name: "Untitled model".into(),
        code: "cube(10);".repeat(4096),
        revision: 7,
        created_at: 1,
        updated_at: 2,
        plate: String::new(),
        tolerance: DEFAULT_TOLERANCE,
        preview: String::new(),
        share_token: String::new(),
        revision_name: String::new(),
        revision_source: String::new(),
        events: Vec::new(),
    };
    let idle = workspace_events_payload(&workspace.id, &workspace, 7);
    assert_eq!(idle["changed"], json!(false));
    assert!(
        idle.get("code").is_none(),
        "idle polls must not echo the workspace source"
    );
    assert_eq!(idle["revision"], json!(7));
    let changed = workspace_events_payload(&workspace.id, &workspace, 6);
    assert_eq!(changed["changed"], json!(true));
    assert_eq!(changed["code"], json!(workspace.code));
}

/// The whole security property of a read-only link, stated as a test: it
/// resolves to the source, and it never yields the workspace id. Anything
/// that leaked the id would make the read-only link an editable one with
/// an extra step.
#[test]
fn a_read_only_link_resolves_without_revealing_the_workspace_it_points_at() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(7);".into(), None).unwrap();
    let token = store.share_token(&workspace.id).unwrap();
    assert!(valid_share_token(&token), "{token:?} is not a share token");
    assert!(
        !token.contains(&workspace.id) && !workspace.id.contains(&token),
        "the token and the id must not contain one another"
    );

    let shared = store.resolve_share(&token).unwrap().expect("resolves");
    assert_eq!(shared.code, "cube(7);");

    // The payload the browser is handed, built exactly as the handler
    // builds it: source and settings, and no `id` anywhere in it.
    let payload = json!({
        "ok": true,
        "readOnly": true,
        "code": shared.code,
        "revision": shared.revision,
        "plate": shared.plate,
        "tolerance": shared.tolerance,
    });
    let serialized = serde_json::to_string(&payload).unwrap();
    assert!(
        !serialized.contains(&workspace.id),
        "the shared payload leaks the workspace id: {serialized}"
    );
    assert!(payload.get("workspaceId").is_none());
    assert!(payload.get("id").is_none());

    // Sharing twice is the same link, so one already sent keeps working.
    assert_eq!(store.share_token(&workspace.id).unwrap(), token);
    // And a token nobody minted resolves to nothing.
    assert!(store
        .resolve_share("00000000000000000000000000000000")
        .unwrap()
        .is_none());
    assert!(store.resolve_share("not-a-token").unwrap().is_none());
    let _ = fs::remove_dir_all(root);
}

/// Evicting a workspace takes its read-only pointer with it, or the store
/// would slowly fill with files naming workspaces that no longer exist.
/// The lifetime total survives eviction and restarts; a store that
/// predates the counter starts from what it holds, not from zero.
#[test]
fn the_creation_counter_is_durable_and_never_decreases() {
    let (store, root) = temporary_store();
    let first = store.create("cube(1);".into(), None).unwrap();
    store.create("cube(2);".into(), None).unwrap();
    assert_eq!(
        store.creation_stats().unwrap(),
        CreationStats { total: 2, last_24h: 2, last_7d: 2 }
    );
    {
        let mut inner = store.inner.lock().unwrap();
        store.forget(&mut inner, &first.id);
    }
    assert_eq!(store.len(), 1);
    assert_eq!(store.creation_stats().unwrap().total, 2, "eviction lowered it");
    drop(store);
    let reopened = WorkspaceStore::open(root.clone()).unwrap();
    assert_eq!(
        reopened.creation_stats().unwrap(),
        CreationStats { total: 2, last_24h: 2, last_7d: 2 }
    );
    reopened.create("cube(3);".into(), None).unwrap();
    assert_eq!(reopened.creation_stats().unwrap().total, 3);
    // Upgrade path: no counter file yet, two workspaces on disk.
    drop(reopened);
    fs::remove_file(root.join(WORKSPACES_CREATED_FILE)).unwrap();
    let upgraded = WorkspaceStore::open(root.clone()).unwrap();
    assert_eq!(upgraded.creation_stats().unwrap().total, 2);
    assert!(root.join(WORKSPACES_CREATED_FILE).exists());
    let _ = fs::remove_dir_all(root);
}

/// A true rolling 24-hour window at one-minute resolution: a creation
/// 23 h 59 min ago still counts, one 24 h ago does not, and the ring never
/// holds more than a week of minutes.
#[test]
fn the_last_24h_window_rolls_by_the_minute() {
    let minute = CREATION_BUCKET_MILLIS;
    let day = 24 * 60 * minute;
    let start = 1_000_000 * minute + 30_000;
    let mut counter = CreationCounter::default();
    counter.record(start);
    counter.record(start + 10);
    counter.record(start + 5 * 60 * minute);
    assert_eq!(counter.total, 3);
    assert_eq!(counter.last_24h(start + 5 * 60 * minute), 3);
    // The old hourly ring dropped the first creation here, almost an hour
    // before it was 24 hours old.
    assert_eq!(counter.last_24h(start + day - minute), 3);
    assert_eq!(counter.last_24h(start + day), 1, "first minute aged out");
    assert_eq!(counter.last_24h(start + day + 5 * 60 * minute), 0);
    // The week keeps counting what the day let go of, to the minute.
    assert_eq!(counter.last_7d(start + day + 5 * 60 * minute), 3);
    assert_eq!(counter.last_7d(start + 7 * day - minute), 3);
    assert_eq!(counter.last_7d(start + 7 * day), 1, "first minute aged out");
    for step in 0..3000 {
        counter.record(start + step * minute);
    }
    assert!(counter.minutes.len() <= CREATION_WEEK_BUCKETS as usize);
    assert_eq!(counter.total, 3003);
}

/// Counter files written with hourly buckets keep counting after the
/// upgrade instead of resetting the 24-hour number.
#[test]
fn legacy_hourly_counters_upgrade_to_minutes() {
    let now = 500_000 * LEGACY_HOUR_MILLIS + 10 * CREATION_BUCKET_MILLIS;
    let hour = now / LEGACY_HOUR_MILLIS;
    let legacy = format!("{{\"total\":9,\"hours\":[[{},2],[{},3]]}}", hour - 30, hour - 1);
    let mut counter: CreationCounter = serde_json::from_str(&legacy).unwrap();
    assert_eq!(counter.last_24h(now), 3);
    counter.upgrade();
    assert!(counter.hours.is_empty());
    assert_eq!(counter.last_24h(now), 3);
    let written = serde_json::to_string(&counter).unwrap();
    assert!(!written.contains("hours"), "{written}");
    counter.record(now);
    assert_eq!((counter.total, counter.last_24h(now)), (10, 4));
}

#[test]
fn evicting_a_workspace_also_drops_its_read_only_link() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    let token = store.share_token(&workspace.id).unwrap();
    assert!(store.share_path(&token).exists());
    {
        let mut inner = store.inner.lock().unwrap();
        store.forget(&mut inner, &workspace.id);
    }
    assert!(
        !store.share_path(&token).exists(),
        "the share pointer outlived the workspace"
    );
    assert!(store.resolve_share(&token).unwrap().is_none());
    let _ = fs::remove_dir_all(root);
}

/// The `.share` sidecars must not be mistaken for workspaces by the boot
/// scan, which would make them show up as (unreadable) workspaces.
#[test]
fn share_pointers_are_not_counted_as_workspaces() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    store.share_token(&workspace.id).unwrap();
    assert_eq!(store.len(), 1);
    let reopened = WorkspaceStore::open(root.clone()).unwrap();
    assert_eq!(reopened.len(), 1, "a sidecar was indexed as a workspace");
    let _ = fs::remove_dir_all(root);
}

/// A workspace URL is the only handle anyone has on their work, so age
/// alone must never take one away. This used to be
/// `workspace_store_evicts_expired_entries`, asserting the opposite.
#[test]
fn workspace_store_keeps_an_untouched_workspace_indefinitely() {
    let (store, root) = temporary_store();
    let workspace = store.create("cube(1);".into(), None).unwrap();
    let path = root.join(format!("{}.json", workspace.id));
    {
        let mut inner = store.inner.lock().unwrap();
        // A year idle, well past the fourteen days that used to delete it.
        let ancient = unix_millis() - 365 * 24 * 60 * 60 * 1000;
        inner
            .index
            .insert(workspace.id.clone(), WorkspaceMeta { updated_at: ancient });
        store.prune(&mut inner, 0);
    }
    assert_eq!(store.len(), 1, "nothing expires");
    assert!(path.exists(), "an idle workspace file must survive");
    assert!(store.get(&workspace.id).unwrap().is_some());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_store_caps_the_total_count() {
    let (store, root) = temporary_store();
    // Synthesise more workspaces than the cap without paying for a full
    // validate + fsync per entry.
    {
        let mut inner = store.inner.lock().unwrap();
        for index in 0..(MAX_WORKSPACES + 32) {
            let id = format!("sunny-otter-{index:016x}0000");
            assert!(valid_workspace_id(&id), "{id}");
            fs::write(root.join(format!("{id}.json")), b"{}").unwrap();
            inner.index.insert(
                id,
                WorkspaceMeta {
                    updated_at: unix_millis() + index as u64,
                },
            );
        }
        store.prune(&mut inner, 0);
    }
    assert_eq!(store.len(), MAX_WORKSPACES);
    // Workspace documents only: the store root also holds the creation
    // counter, which eviction deliberately leaves alone.
    let survivors = fs::read_dir(&root)
        .unwrap()
        .filter(|entry| {
            entry.as_ref().unwrap().path().extension().and_then(|value| value.to_str())
                == Some("json")
        })
        .count();
    assert_eq!(survivors, MAX_WORKSPACES, "oldest files must be unlinked");
    // Creating one more still leaves headroom rather than overflowing.
    store.create("cube(1);".into(), None).unwrap();
    assert!(store.len() <= MAX_WORKSPACES);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_store_keeps_only_a_bounded_cache_resident() {
    let (store, root) = temporary_store();
    let mut ids = Vec::new();
    for _ in 0..(MAX_CACHED_WORKSPACES + 8) {
        ids.push(store.create("cube(1);".into(), None).unwrap().id);
    }
    {
        let inner = store.inner.lock().unwrap();
        assert_eq!(inner.index.len(), ids.len());
        assert!(
            inner.cache.len() <= MAX_CACHED_WORKSPACES,
            "resident cache grew to {}",
            inner.cache.len()
        );
        assert!(!inner.cache.contains_key(&ids[0]), "coldest must be evicted");
    }
    // Evicted from RAM but still readable from disk.
    let reloaded = store.get(&ids[0]).unwrap().expect("workspace on disk");
    assert_eq!(reloaded.code, "cube(1);");
    assert_eq!(reloaded.revision, 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_store_survives_a_restart_without_loading_every_source() {
    let (store, root) = temporary_store();
    let created = store.create("cube(3);".into(), None).unwrap();
    let reopened = WorkspaceStore::open(root.clone()).unwrap();
    {
        let inner = reopened.inner.lock().unwrap();
        assert_eq!(inner.index.len(), 1);
        assert!(inner.cache.is_empty(), "boot must not load sources into RAM");
    }
    let loaded = reopened.get(&created.id).unwrap().expect("persisted");
    assert_eq!(loaded.code, "cube(3);");
    let _ = fs::remove_dir_all(root);
}

/// The web root doubles as the document root, so the deployment files that
/// now live beside `index.html` would otherwise be downloadable — and
/// `docker-compose.yml` carries a database password.
#[test]
fn deployment_files_are_not_served_from_the_web_root() {
    for private in [
        "Dockerfile",
        "dockerfile",
        "docker-compose.yml",
        "docker-compose.yaml",
        ".dockerignore",
        "DEPLOY.md",
        ".env",
        ".env.production",
        "package.json",
    ] {
        assert!(is_private_web_root_file(private), "{private} was servable");
    }
    // Whatever the shell actually needs must still resolve.
    for public in [
        "index.html",
        "app.js",
        "styles.css",
        "legacy-assets/icons/reopenscad.svg",
        "README.md",
    ] {
        assert!(!is_private_web_root_file(public), "{public} was blocked");
    }
}

fn forwarded(values: &[&str]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|value| ("x-forwarded-for".to_string(), value.to_string()))
        .collect()
}

/// The default deployment has no proxy, so the header must not be able to
/// influence which rate-limit bucket a caller lands in.
#[test]
fn forwarded_headers_are_ignored_without_a_configured_hop_count() {
    let headers = forwarded(&["203.0.113.9"]);
    assert_eq!(client_ip_from_chain(&headers, 0), None);
}

/// A client can prepend anything it likes to `X-Forwarded-For`; only the
/// entries the trusted proxies appended, counted from the right, are real.
/// Without this the header is an unlimited supply of rate-limit buckets.
#[test]
fn a_spoofed_forwarded_prefix_cannot_choose_the_rate_limit_bucket() {
    // What Cloud Run passes on when the caller sent its own header: the
    // forged value first, the address the front end observed appended last.
    let headers = forwarded(&["1.1.1.1, 9.9.9.9, 198.51.100.4"]);
    assert_eq!(
        client_ip_from_chain(&headers, 1),
        Some("198.51.100.4".parse().unwrap())
    );
}

/// Behind an external HTTPS load balancer the chain gains one more hop, so
/// the client address moves one place left.
#[test]
fn two_hops_reads_past_the_load_balancer_entry() {
    let headers = forwarded(&["198.51.100.4, 130.211.0.1"]);
    assert_eq!(
        client_ip_from_chain(&headers, 2),
        Some("198.51.100.4".parse().unwrap())
    );
}

/// Repeated headers are one logical list, and a chain shorter than the
/// configured hop count means the request did not arrive the way the
/// deployment claims, so it falls back to the socket address.
#[test]
fn split_headers_join_and_a_short_chain_falls_back() {
    let headers = forwarded(&["198.51.100.4", "130.211.0.1"]);
    assert_eq!(
        client_ip_from_chain(&headers, 2),
        Some("198.51.100.4".parse().unwrap())
    );
    assert_eq!(client_ip_from_chain(&headers, 5), None);
}

/// Proxies emit ports and bracketed IPv6 literals; both have to collapse
/// to the same bucket key as the bare address would.
#[test]
fn forwarded_entries_normalise_ports_and_ipv6_forms() {
    assert_eq!(
        client_ip_from_chain(&forwarded(&["198.51.100.4:41234"]), 1),
        Some("198.51.100.4".parse().unwrap())
    );
    assert_eq!(
        client_ip_from_chain(&forwarded(&["[2001:db8::1]:443"]), 1),
        Some("2001:db8::1".parse().unwrap())
    );
    assert_eq!(
        client_ip_from_chain(&forwarded(&["2001:db8::1"]), 1),
        Some("2001:db8::1".parse().unwrap())
    );
    // IPv4-mapped IPv6 collapses, so one client cannot hold two buckets.
    assert_eq!(
        client_ip_from_chain(&forwarded(&["::ffff:198.51.100.4"]), 1),
        Some("198.51.100.4".parse().unwrap())
    );
}

/// Garbage in the trusted position must not be silently promoted to some
/// other address; it degrades to the socket address instead.
#[test]
fn an_unparseable_forwarded_entry_falls_back() {
    assert_eq!(client_ip_from_chain(&forwarded(&["unknown"]), 1), None);
    assert_eq!(client_ip_from_chain(&[], 1), None);
}

// -----------------------------------------------------------------------
// Workspace URL confidentiality.
//
// The URL is the credential, so the headers that stop it being republished
// are load-bearing security controls, not hygiene. They are also exactly
// the kind of thing that disappears in a refactor without any test going
// red, so these drive a real socket through `serve_request`.
// -----------------------------------------------------------------------

/// The repository's `web/` directory: the document root the deployed
/// binary is pointed at, so the shell under test is the shipped one.
fn web_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("backend/ has a parent")
        .to_path_buf()
}

/// Runs one raw request through the real request pipeline and returns the
/// whole response, headers included.
fn serve_over_socket(raw: &str) -> String {
    serve_over_socket_with_origin(None, raw)
}

/// The same pipeline with `REOPENSCAD_PUBLIC_ORIGIN` set to `public_origin`.
fn serve_over_socket_with_origin(public_origin: Option<&str>, raw: &str) -> String {
    let (mut state, root) = temporary_state(public_origin);
    state.web_root = web_root();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let raw = raw.to_string();
    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    });
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    serve_request(&mut stream, &state, &mut None).unwrap();
    drop(stream);
    let response = client.join().unwrap();
    let _ = fs::remove_dir_all(root);
    response
}

fn get(path: &str) -> String {
    serve_over_socket(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\n\r\n"))
}

/// Header block only. The shell's own text talks about these headers, so a
/// whole-response substring search would find them in the body.
fn get_headers(path: &str) -> String {
    let response = get(path);
    response
        .split_once("\r\n\r\n")
        .map(|(headers, _)| headers.to_string())
        .unwrap_or(response)
}

/// The header that stops a workspace path travelling to a third party in
/// `Referer`. It has to be on *every* response, because the shell's own
/// subresources and any future outbound link inherit it from the document.
#[test]
fn every_response_suppresses_the_referrer() {
    for path in ["/", "/app.js", "/api/health", "/robots.txt", "/nope"] {
        assert!(
            get_headers(path).contains("\r\nReferrer-Policy: no-referrer"),
            "{path} did not suppress the referrer"
        );
    }
}

/// Deny-by-default: a route has to be named indexable to lose the tag, so
/// a route added later is hidden rather than exposed.
#[test]
fn only_the_landing_page_is_indexable() {
    let tag = format!("\r\nX-Robots-Tag: {ROBOTS_TAG}");
    for path in [
        "/designs/wobbly-wombat-00000000000000000000000000cc",
        "/workspaces/wobbly-wombat-00000000000000000000000000cc",
        "/api/health",
        "/?workspace=wobbly-wombat-00000000000000000000000000cc",
        "/?design=wobbly-wombat-00000000000000000000000000cc",
        "/app.js",
        "/nope",
    ] {
        assert!(get_headers(path).contains(&tag), "{path} was left indexable");
    }
    assert!(!get_headers("/").contains("X-Robots-Tag"));
    assert!(!get_headers("/index.html").contains("X-Robots-Tag"));
}

/// Both halves of the same predicate: the shell carries `noindex` on the
/// workspace route and only the landing page has it removed.
#[test]
fn the_shell_hides_workspaces_and_shows_the_front_door() {
    assert!(
        get("/designs/wobbly-wombat-00000000000000000000000000cc").contains(ROBOTS_META)
    );
    assert!(get("/?workspace=wobbly-wombat-00000000000000000000000000cc").contains(ROBOTS_META));
    // A read-only link is a capability too: it reveals the source to
    // whoever holds it, so it must not be indexed either.
    assert!(
        get("/shared/0f1e2d3c4b5a69788796a5b4c3d2e1f0").contains(ROBOTS_META),
        "a shared view must be noindex"
    );
    let landing = get("/");
    assert!(landing.contains("<title>ReOpenSCAD</title>"));
    assert!(!landing.contains("name=\"robots\""));
}

/// A shell served by anything other than this binary — a static host, a
/// mirror, a copy of the file — must still hide workspaces.
#[test]
fn the_shell_file_on_disk_is_noindex() {
    let shell = fs::read_to_string(web_root().join("index.html")).unwrap();
    assert!(shell.contains(ROBOTS_META));
}

/// The workspace routes, the API that serves workspace source and the MCP
/// endpoint are closed; the landing page stays open so the project is
/// findable at all.
#[test]
fn robots_txt_closes_workspaces_and_leaves_the_landing_page_open() {
    let response = get("/robots.txt");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains("Content-Type: text/plain; charset=utf-8"));
    for rule in [
        "User-agent: *",
        "Disallow: /designs/",
        "Disallow: /workspaces/",
        "Disallow: /shared/",
        "Disallow: /api/",
        "Disallow: /mcp",
    ] {
        assert!(response.contains(rule), "robots.txt is missing {rule:?}");
    }
    assert!(!response.contains("Disallow: /\r\n"));
    assert!(!response.contains("Disallow: /\n"));
}

/// A shared cache holding a workspace response would hand one visitor's
/// work to the next.
#[test]
fn nothing_may_be_cached() {
    for path in [
        "/",
        "/designs/wobbly-wombat-00000000000000000000000000cc",
        "/workspaces/wobbly-wombat-00000000000000000000000000cc",
        "/api/health",
    ] {
        assert!(
            get_headers(path).contains("\r\nCache-Control: no-store"),
            "{path} was cacheable"
        );
    }
}

/// The landing page's counters are the one deliberate exception to
/// `no-store`: aggregate numbers, nothing about any workspace.
#[test]
fn stats_are_public_cheap_and_briefly_cacheable() {
    assert_eq!(route_cost("GET", "/api/stats"), RouteCost::Cheap);
    let response = get("/api/stats");
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
    assert!(headers.contains("\r\nCache-Control: public, max-age=30"), "{headers}");
    assert!(!headers.contains("no-store"), "{headers}");
    assert_eq!(headers.matches("Cache-Control").count(), 1, "{headers}");
    let value: Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        value,
        json!({
            "designsCreated": 0,
            "designsCreatedLast24h": 0,
            "designsCreatedLast7d": 0,
            "workspacesCreated": 0,
            "workspacesCreatedLast24h": 0
        })
    );
}

/// The route reads the store it is given, and leaks no workspace id.
#[test]
fn stats_count_workspaces_created_through_the_store() {
    let (mut state, root) = temporary_state(None);
    state.web_root = web_root();
    let created = state.workspaces.create("cube(1);".into(), None).unwrap();
    state.workspaces.create("cube(2);".into(), None).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream
            .write_all(b"GET /api/stats HTTP/1.1\r\nHost: 127.0.0.1:5173\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    });
    let (mut stream, _) = listener.accept().unwrap();
    serve_request(&mut stream, &state, &mut None).unwrap();
    drop(stream);
    let response = client.join().unwrap();
    let _ = fs::remove_dir_all(root);
    let body = response.split_once("\r\n\r\n").unwrap().1;
    let value: Value = serde_json::from_str(body).unwrap();
    assert_eq!(value["designsCreated"], 2);
    assert_eq!(value["designsCreatedLast24h"], 2);
    assert_eq!(value["designsCreatedLast7d"], 2);
    assert_eq!(value["workspacesCreated"], 2);
    assert_eq!(value["workspacesCreatedLast24h"], 2);
    assert!(!response.contains(&created.id));
}

/// Sends one request through the real dispatcher against `state`.
fn serve_with_state(state: &AppState, raw: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    });
    let (mut stream, _) = listener.accept().unwrap();
    serve_request(&mut stream, state, &mut None).unwrap();
    drop(stream);
    client.join().unwrap()
}

fn post_json(path: &str, body: &Value) -> String {
    let body = body.to_string();
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// An analysis reply is reused only while the design's assets are the same:
/// uploading, replacing and deleting each change the job's key through the digest.
#[test]
fn a_scan_tools_job_is_not_reused_across_an_asset_change() {
    let (state, root) = temporary_state(None);
    let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
    let arguments = json!({"designId": id, "reference": {"asset": "a.stl"}, "candidate": "object-1"});
    let request = crate::scancommon::testkit::request();
    let key = || {
        crate::assetstore::with_design_assets(&state, Some(&id), || mcp_job_spec(&state, &request, "compare_surfaces", &arguments).unwrap().key)
    };
    let triangle = |z: f64| crate::assets::TriMesh { points: vec![[0.0, 0.0, z], [1.0, 0.0, z], [0.0, 1.0, z]], triangles: vec![[0, 1, 2]] };
    let empty = key();
    assert_eq!(empty, key(), "nothing changed, nothing to redo");
    crate::scancommon::testkit::upload(&state, &id, "a.stl", &triangle(0.0));
    let first = key();
    assert_ne!(first, empty, "an upload");
    assert_eq!(first, key());
    let deleted = crate::scancommon::testkit::call(&state, "delete_asset", json!({"designId": id, "name": "a.stl"}));
    assert_eq!(deleted["isError"], json!(false), "{deleted}");
    let after_delete = key();
    assert_ne!(after_delete, first, "a delete");
    crate::scancommon::testkit::upload(&state, &id, "a.stl", &triangle(5.0));
    let replaced = key();
    assert_ne!(replaced, first, "other bytes under the same name");
    assert_ne!(replaced, after_delete);
    let _ = fs::remove_dir_all(root);
}

/// The Compare parts panel's route: metered like a render, two parts in, an
/// activity entry with both heatmaps out, in neutral words.
#[test]
fn the_compare_route_answers_an_entry_with_both_heatmaps() {
    let (mut state, root) = temporary_state(None);
    state.web_root = web_root();
    let code = "cube(4);\ntranslate([10,0,0]) cube(4.2);\n";
    let workspace = state.workspaces.create(code.into(), None).unwrap();
    let path = format!("/api/designs/{}/compare", workspace.id);
    assert_eq!(route_cost("POST", &path), RouteCost::Evaluator);
    let compare = |body: &Value| serve_with_state(&state, post_json(&path, body));
    let compared = compare(&json!({"a": "object-1", "b": "object-2"}));
    let same = compare(&json!({"a": "object-1", "b": "object-1"}));
    let missing = compare(&json!({"a": "object-1"}));
    let unknown = compare(&json!({"a": "object-1", "b": "object-9"}));
    let _ = fs::remove_dir_all(root);
    assert!(compared.starts_with("HTTP/1.1 200"), "{compared}");
    let value: Value = serde_json::from_str(compared.split_once("\r\n\r\n").unwrap().1).unwrap();
    let entry = &value["entry"];
    assert_eq!(entry["tool"], json!("compare_surfaces"));
    assert_eq!(entry["fromBrowser"], json!(true));
    assert_eq!(entry["revision"], json!(1), "the browser drops a reply whose revision has moved");
    let meshes = entry["meshes"].as_array().unwrap();
    assert_eq!(meshes.len(), 2, "{entry}");
    assert_eq!(meshes[0]["label"], json!("object-1"));
    assert_eq!(meshes[1]["label"], json!("object-2"));
    assert!(meshes[0]["url"].is_string());
    assert!(meshes[0]["legend"].get("title").is_none());
    assert!(same.starts_with("HTTP/1.1 400"), "{same}");
    assert!(missing.starts_with("HTTP/1.1 400"), "{missing}");
    assert!(unknown.starts_with("HTTP/1.1 422"), "{unknown}");
}

/// A read-only link renders the stored design per part, with colours,
/// exactly as the editable route does, and never names the design id.
/// It used to fall back to `/api/render`: one merged mesh, no parts.
#[test]
fn a_shared_render_answers_coloured_parts_without_the_design_id() {
    let (mut state, root) = temporary_state(None);
    state.web_root = web_root();
    let code = "color(\"red\") cube(4);\ncolor(\"blue\") translate([10,0,0]) cube(4);\n";
    let workspace = state.workspaces.create(code.into(), None).unwrap();
    let token = state.workspaces.share_token(&workspace.id).unwrap();
    let path = format!("/api/shared/{token}/render");
    assert_eq!(route_cost("POST", &path), RouteCost::Evaluator);
    assert_eq!(route_cost("GET", &format!("/api/shared/{token}")), RouteCost::Cheap);

    // The body's source is ignored: only the stored design is compiled.
    let request = |request_id: &str, revision: u64| {
        post_json(
            &path,
            &json!({"code": "sphere(1);", "mode": "preview", "requestId": request_id, "revision": revision}),
        )
    };
    let shared = serve_with_state(&state, request("shared-render-1", workspace.revision));
    let editable = serve_with_state(
        &state,
        post_json(
            &format!("/api/designs/{}/render", workspace.id),
            &json!({"mode": "preview", "requestId": "design-render-1", "revision": workspace.revision}),
        ),
    );
    // Served again, from the render cache this time.
    let cached = serve_with_state(&state, request("shared-render-2", workspace.revision));
    let stale = serve_with_state(&state, request("shared-render-3", workspace.revision + 1));
    let missing = serve_with_state(
        &state,
        post_json(
            "/api/shared/00000000000000000000000000000000/render",
            &json!({"requestId": "shared-render-4", "revision": 1}),
        ),
    );
    let _ = fs::remove_dir_all(root);

    for response in [&shared, &cached] {
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains(&workspace.id), "the shared render leaks the design id");
        let value: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(value.get("designId").is_none() && value.get("workspaceId").is_none());
        let objects = value["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 2, "{value}");
        assert_ne!(objects[0]["color"], objects[1]["color"], "each part keeps its colour");
        assert!(!objects[0]["color"].is_null());
        let editable: Value = serde_json::from_str(editable.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(value["objects"], editable["objects"], "same parts as the editable view");
        assert_eq!(editable["designId"], json!(workspace.id));
    }
    assert!(cached.contains("X-Render-Cache: hit"), "{cached}");
    assert!(stale.starts_with("HTTP/1.1 409"), "{stale}");
    assert!(!stale.contains(&workspace.id));
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
}

/// Pre-rename browser links redirect to the canonical design route,
/// query and all, and the redirect is as hidden from crawlers as the page.
#[test]
fn legacy_workspace_links_redirect_to_designs() {
    let response = get("/workspaces/wobbly-wombat-00000000000000000000000000cc?plate=x");
    let (headers, _) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 308 Permanent Redirect\r\n"), "{headers}");
    assert!(headers.lines().any(|line| line == "Location: /designs/wobbly-wombat-00000000000000000000000000cc?plate=x"), "{headers}");
    assert!(headers.contains("X-Robots-Tag"), "{headers}");
    assert!(headers.contains("Cache-Control: no-store"), "{headers}");
    let headers = get_headers("/workspaces/wobbly-wombat-00000000000000000000000000cc/");
    assert!(headers.lines().any(|line| line == "Location: /designs/wobbly-wombat-00000000000000000000000000cc"), "{headers}");
    // An invalid id is not redirected anywhere.
    assert!(get("/workspaces/..%2fbackend").starts_with("HTTP/1.1 404"));
    let page = get("/designs/wobbly-wombat-00000000000000000000000000cc");
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(page.contains(ROBOTS_META));
}

/// Connection threads are pooled, so the landing page must not leave the
/// next request on that thread indexable.
#[test]
fn indexability_does_not_leak_between_requests_on_a_thread() {
    // Stand in for "the previous request on this thread was the landing
    // page"; the reset inside `serve_request` is what has to undo it.
    set_indexable(true);
    let response = get("/designs/wobbly-wombat-00000000000000000000000000cc");
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.contains("X-Robots-Tag"));
    assert!(body.contains(ROBOTS_META));
    set_indexable(false);
}

// -----------------------------------------------------------------------
// What the 403 tells the operator.
//
// `request_is_allowed` accepts exactly one authority, which is the whole
// point of it -- that stays. The body used to be a fixed literal naming
// localhost, so a deployment with `REOPENSCAD_PUBLIC_ORIGIN` set answered
// every Host mismatch with advice about a machine that is not involved.
// The wording is the only thing under test here, and it is only reachable
// through the real response path, so these drive a socket too.
// -----------------------------------------------------------------------

fn body_of(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default()
}

/// Parses the response as the JSON error envelope every client expects,
/// asserting the status line and `ok: false` on the way through.
fn forbidden_error(response: &str) -> String {
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "expected a 403, got {response}"
    );
    assert!(
        response.contains("Content-Type: application/json; charset=utf-8"),
        "403 was not sent as JSON: {response}"
    );
    let body: Value =
        serde_json::from_str(body_of(response)).expect("the 403 body is valid JSON");
    assert_eq!(body["ok"], json!(false));
    body["error"]
        .as_str()
        .expect("the 403 body carries an error string")
        .to_string()
}

/// A public deployment rejecting a Host mismatch has to name the origin it
/// does accept -- and must not send the operator to localhost.
#[test]
fn the_public_origin_rejection_names_the_configured_origin() {
    let error = forbidden_error(&serve_over_socket_with_origin(
        Some("https://reopenscad.com"),
        "GET /api/health HTTP/1.1\r\nHost: reopenscad-354887056155.us-central1.run.app\r\n\r\n",
    ));
    assert!(error.contains("https://reopenscad.com"), "{error}");
    assert!(error.contains("REOPENSCAD_PUBLIC_ORIGIN"), "{error}");
    assert!(
        !error.to_ascii_lowercase().contains("localhost"),
        "a configured deployment still blamed localhost: {error}"
    );
}

/// The other half of the same predicate: the configured origin is served,
/// so the message above is diagnosing a real Host mismatch and nothing else.
#[test]
fn the_public_origin_serves_its_own_host() {
    let response = serve_over_socket_with_origin(
        Some("https://reopenscad.com"),
        "GET /api/health HTTP/1.1\r\nHost: reopenscad.com\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "the configured origin was refused: {response}"
    );
}

/// With no public origin configured the localhost wording is the correct
/// diagnosis, so it stays -- now pointing at the env var that changes it.
#[test]
fn the_unconfigured_rejection_still_points_at_localhost() {
    let error = forbidden_error(&serve_over_socket(
        "GET /api/health HTTP/1.1\r\nHost: attacker.example\r\n\r\n",
    ));
    assert!(error.contains("localhost"), "{error}");
    assert!(error.contains("REOPENSCAD_PUBLIC_ORIGIN"), "{error}");
}

/// The origin is reflected into the body, so it has to be serialized rather
/// than pasted: a quote in the configured value must escape, not close the
/// string and add fields of its own.
#[test]
fn a_quoted_origin_cannot_forge_the_403_body() {
    let response = serve_over_socket_with_origin(
        Some("https://evil\",\"ok\":true,\"injected\":\"a.example"),
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:5173\r\n\r\n",
    );
    let error = forbidden_error(&response);
    assert!(
        error.contains("\",\"ok\":true"),
        "the origin was mangled: {error}"
    );
    let body: Value = serde_json::from_str(body_of(&response)).unwrap();
    let fields = body.as_object().expect("a JSON object");
    assert_eq!(
        fields.len(),
        2,
        "the origin smuggled a field in: {fields:?}"
    );
    assert!(!fields.contains_key("injected"), "{fields:?}");
}
