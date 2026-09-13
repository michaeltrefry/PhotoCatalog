use photocatalog::{
    application::{
        I64, U64,
        lightroom::{Action, Config, Limits, OpenMode, OriginalInspection, Phase, Query},
    },
    lightroom::{capture, discovery},
    storage_volume::NativePath,
};
use rusqlite::Connection;
use std::{
    fs,
    io::Write,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use photocatalog::application::{self as app, lightroom_bridge as wire};
use wire::Status;
struct Workbench {
    bridge: app::Bridge,
}
impl Workbench {
    fn call(&self, r: wire::Request) -> anyhow::Result<wire::Response> {
        match self
            .bridge
            .submit(app::Request::Lightroom {
                request: Box::new(r),
            })?
            .recv()
        {
            app::Reply::Ok {
                value: app::Response::Lightroom(r),
            } => Ok(*r),
            app::Reply::Error { error } => Err(error.into()),
            _ => anyhow::bail!("unexpected bridge response"),
        }
    }
    fn spawn(c: Config) -> anyhow::Result<Self> {
        let bridge = app::Bridge::spawn(app::Config {
            worker_executable: c.capture_executable.to_path()?,
            cache_root: None,
            original_roots: vec![],
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
        })?;
        let w = Self { bridge };
        w.call(wire::Request::Open {
            attempt: uuid::Uuid::new_v4().to_string(),
            root: c.root,
            mode: c.mode,
            capture_staging: c.capture_staging,
            limits: c.limits.into(),
        })?;
        Ok(w)
    }
    fn status(&self) -> Status {
        let wire::Response::Status(Some(s)) = self
            .call(wire::Request::Status {
                workbench: None,
                attempt: None,
            })
            .unwrap()
        else {
            panic!()
        };
        s
    }
    fn guard(&self) -> wire::Guard {
        let s = self.status();
        wire::Guard {
            workbench: s.workbench,
            generation: s.generation,
            operation: s.operation,
        }
    }
    fn upload(&self, json: String) -> anyhow::Result<String> {
        let guard = self.guard();
        let wire::Response::Input(Some(u)) = self.call(wire::Request::InputBegin {
            guard: guard.clone(),
            purpose: wire::InputPurpose::Inventory,
            total_bytes: U64(json.len() as u64),
            expected_blake3: None,
        })?
        else {
            panic!()
        };
        let mut offset = 0;
        while offset < json.len() {
            let mut end = (offset + 16384).min(json.len());
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            self.call(wire::Request::InputAppend {
                guard: guard.clone(),
                input: u.input.clone(),
                offset: U64(offset as u64),
                fragment: json[offset..end].into(),
            })?;
            offset = end;
        }
        let wire::Response::Input(Some(finished)) = self.call(wire::Request::InputFinish {
            guard,
            input: u.input.clone(),
        })?
        else {
            panic!()
        };
        assert_eq!(
            finished.blake3,
            Some(blake3::hash(json.as_bytes()).to_hex().to_string())
        );
        Ok(u.input)
    }
    fn start(&self, generation: &str, a: Action) -> anyhow::Result<String> {
        let mut guard = self.guard();
        guard.generation = generation.into();
        let action = match a {
            Action::Discover { root, limits } => wire::Action::Discover {
                root,
                limits: limits.into(),
            },
            Action::RegisterInventory { inventory } => wire::Action::RegisterInventory {
                input: self.upload(serde_json::to_string(&inventory)?)?,
            },
            Action::Capture { request: r } => wire::Action::Capture {
                source: r.source,
                output: r.output,
                include_auxiliary: r.include_auxiliary,
                closed_application_evidence: r.closed_application_evidence,
                limits: r.limits.into(),
            },
            Action::AddCapture { directory } => wire::Action::AddCapture { directory },
            Action::Resume { revision, max_rows } => wire::Action::Resume { revision, max_rows },
            Action::InspectOriginals {
                revision,
                limit,
                inspection,
            } => wire::Action::InspectOriginals {
                revision,
                limit,
                inspection,
            },
            _ => anyhow::bail!("fixture action mapping missing"),
        };
        let wire::Response::Status(Some(s)) = self.call(wire::Request::Action { guard, action })?
        else {
            panic!()
        };
        Ok(s.operation)
    }
    fn read(&self, generation: &str, query: Query) -> anyhow::Result<String> {
        let mut guard = self.guard();
        guard.generation = generation.into();
        let wire::Response::Status(Some(s)) = self.call(wire::Request::Read {
            guard,
            query: query.into(),
        })?
        else {
            panic!()
        };
        Ok(s.operation)
    }
    fn result(
        &self,
        generation: &str,
        operation: &str,
        token: &str,
        offset: U64,
        limit: U64,
    ) -> anyhow::Result<photocatalog::application::lightroom::ResultPage> {
        let mut guard = self.guard();
        guard.generation = generation.into();
        guard.operation = operation.into();
        let wire::Response::Result(page) = self.call(wire::Request::Result {
            guard,
            token: token.into(),
            offset,
            limit,
        })?
        else {
            panic!()
        };
        assert!(serde_json::to_vec(&page)?.len() <= 128 * 1024);
        Ok(page.page)
    }
    fn cancel(&self, operation: &str) -> anyhow::Result<()> {
        let mut guard = self.guard();
        guard.operation = operation.into();
        self.call(wire::Request::Cancel { guard })?;
        Ok(())
    }
    fn request_close(&self) {
        self.call(wire::Request::Close {
            workbench: self.status().workbench,
        })
        .unwrap();
    }
    fn poll_closed(&mut self) -> anyhow::Result<bool> {
        Ok(self.status().closed)
    }
}
impl Drop for Workbench {
    fn drop(&mut self) {
        self.bridge.shutdown();
    }
}

fn wait(w: &Workbench) -> Status {
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let s = w.status();
        if matches!(
            s.phase,
            Phase::Complete | Phase::Failed | Phase::Canceled | Phase::Closed
        ) {
            return s;
        }
        assert!(Instant::now() < until, "workbench timeout: {s:?}");
        thread::sleep(Duration::from_millis(1));
    }
}
fn result(w: &Workbench) -> serde_json::Value {
    let s = wait(w);
    assert_eq!(s.phase, Phase::Complete, "{s:?}");
    let token = s.result_token.as_deref().unwrap();
    let mut offset = U64(0);
    let mut bytes = String::new();
    loop {
        let page = w
            .result(&s.generation, &s.operation, token, offset, U64(4096))
            .unwrap();
        bytes.push_str(&page.json_fragment);
        if let Some(next) = page.next {
            offset = next;
        } else {
            break;
        }
    }
    serde_json::from_str(&bytes).unwrap()
}
fn action(w: &Workbench, a: Action) -> serde_json::Value {
    w.start(&w.status().generation, a).unwrap();
    result(w)
}
fn read(w: &Workbench, q: Query) -> serde_json::Value {
    w.read(&w.status().generation, q).unwrap();
    result(w)
}
fn close(w: &mut Workbench) {
    w.request_close();
    let until = Instant::now() + Duration::from_secs(20);
    while !w.poll_closed().unwrap() {
        assert!(Instant::now() < until, "{:?}", w.status());
        thread::sleep(Duration::from_millis(1));
    }
}
fn fixture(path: &Path, original: &Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE Adobe_variablesTable(id_local INTEGER PRIMARY KEY,name TEXT,value TEXT);
CREATE TABLE Adobe_images(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFile INTEGER,masterImage INTEGER,copyName TEXT);
CREATE TABLE AgLibraryRootFolder(id_local INTEGER PRIMARY KEY,absolutePath TEXT);
CREATE TABLE AgLibraryFolder(id_local INTEGER PRIMARY KEY,rootFolder INTEGER,pathFromRoot TEXT);
CREATE TABLE AgLibraryFile(id_local INTEGER PRIMARY KEY,folder INTEGER,baseName TEXT,extension TEXT,idx_filename TEXT);
CREATE TABLE Adobe_AdditionalMetadata(id_local INTEGER PRIMARY KEY,image INTEGER,xmp BLOB);
CREATE TABLE Opaque(id_local INTEGER PRIMARY KEY,payload TEXT);
INSERT INTO Adobe_variablesTable VALUES(1,'Adobe_storeProviderID','synthetic'),(2,'Adobe_DBVersion','1300000');
INSERT INTO Adobe_images VALUES(9007199254740993,'synthetic-master',1,NULL,NULL),(9007199254740994,'synthetic-copy',1,9007199254740993,'copy');
INSERT INTO AgLibraryFolder VALUES(1,1,'');
INSERT INTO AgLibraryFile VALUES(1,1,'original','CR2','original.CR2');
WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<3000) INSERT INTO Opaque SELECT x,'retained opaque data' FROM n;").unwrap();
    let mut root = original.parent().unwrap().to_string_lossy().into_owned();
    root.push(std::path::MAIN_SEPARATOR);
    db.execute("INSERT INTO AgLibraryRootFolder VALUES(1,?)", [root])
        .unwrap();
    let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><unknown/></x:xmpmeta>";
    let mut compressed = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
    compressed.write_all(xmp).unwrap();
    let mut packet = (xmp.len() as u32).to_be_bytes().to_vec();
    packet.extend(compressed.finish().unwrap());
    db.execute(
        "INSERT INTO Adobe_AdditionalMetadata VALUES(1,9007199254740993,?)",
        [packet],
    )
    .unwrap();
}

#[test]
fn owned_capture_resume_cancel_and_explicit_original_inspection_preserve_sources() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("sources");
    fs::create_dir(&originals).unwrap();
    let original = originals.join("original.CR2");
    fs::write(&original, b"synthetic raw bytes; no renderer invoked").unwrap();
    let source = originals.join("synthetic.lrcat");
    fixture(&source, &original);
    let source_before = fs::read(&source).unwrap();
    let raw_before = fs::read(&original).unwrap();
    let root = temp.path().join("inspection");
    let mut cfg = Config {
        root: NativePath::from_path(&root),
        mode: OpenMode::Create,
        capture_executable: NativePath::from_path(Path::new(env!(
            "CARGO_BIN_EXE_lightroom_inspect"
        ))),
        capture_staging: NativePath::from_path(temp.path()),
        limits: Limits::default(),
    };
    let mut w = Workbench::spawn(cfg.clone()).unwrap();
    result(&w);
    let inventory: discovery::Inventory = serde_json::from_value(action(
        &w,
        Action::Discover {
            root: NativePath::from_path(&originals),
            limits: photocatalog::lightroom::Limits::default(),
        },
    ))
    .unwrap();
    assert_eq!(inventory.candidates.len(), 1);
    action(&w, Action::RegisterInventory { inventory });
    let output = temp.path().join("captured");
    let manifest: capture::Manifest = serde_json::from_value(action(
        &w,
        Action::Capture {
            request: capture::Request {
                source: NativePath::from_path(&source),
                output: NativePath::from_path(&output),
                include_auxiliary: true,
                closed_application_evidence: None,
                limits: photocatalog::lightroom::Limits::default(),
            },
        },
    ))
    .unwrap();
    assert_eq!(manifest.state, "captured");
    assert!(w.status().capture_pid.is_none());
    let retained = read(
        &w,
        Query::CaptureManifest {
            directory: NativePath::from_path(&output),
        },
    );
    assert_eq!(
        retained["revision_id"],
        manifest.revision_id.as_deref().unwrap()
    );
    let added = action(
        &w,
        Action::AddCapture {
            directory: NativePath::from_path(&output),
        },
    );
    let revision = added["revision"].as_str().unwrap().to_owned();
    let custody_before = fs::read(output.join("logical.sqlite3")).unwrap();
    let op = w
        .start(
            &w.status().generation,
            Action::Resume {
                revision: revision.clone(),
                max_rows: U64(100_000),
            },
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let status = w.status();
        if status.processed.0 > 0 {
            w.cancel(&op).unwrap();
            break;
        }
        assert!(
            Instant::now() < until && status.phase == Phase::Running,
            "{status:?}"
        );
        thread::yield_now();
    }
    assert_eq!(wait(&w).phase, Phase::Canceled);
    let report = read(
        &w,
        Query::Report {
            revision: revision.clone(),
        },
    );
    assert!(
        report["tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["state"] != "failed")
    );
    close(&mut w);
    cfg.mode = OpenMode::OpenExisting;
    w = Workbench::spawn(cfg).unwrap();
    result(&w);
    // Opening does not resume: compare stored partial count before admission.
    let reopened = read(
        &w,
        Query::Report {
            revision: revision.clone(),
        },
    );
    assert_eq!(report["tables"], reopened["tables"]);
    for _ in 0..100 {
        let p = action(
            &w,
            Action::Resume {
                revision: revision.clone(),
                max_rows: U64(100),
            },
        );
        if p["stage"] != "pending" {
            break;
        }
    }
    let report = read(
        &w,
        Query::Report {
            revision: revision.clone(),
        },
    );
    assert!(
        report["tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["state"] == "complete")
    );
    let rows = read(
        &w,
        Query::Rows {
            revision: revision.clone(),
            table: Some("Adobe_images".into()),
            after: I64(0),
            limit: U64(2),
        },
    );
    assert_eq!(
        rows["rows"][0]["cells"][0]["value"].as_i64(),
        Some(9007199254740993)
    );
    let paths = read(
        &w,
        Query::Paths {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(10),
        },
    );
    assert_eq!(paths["rows"][0]["state"], "pending");
    action(
        &w,
        Action::InspectOriginals {
            revision: revision.clone(),
            limit: U64(10),
            inspection: OriginalInspection::MetadataOnly,
        },
    );
    let paths = read(
        &w,
        Query::Paths {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(10),
        },
    );
    assert_eq!(paths["rows"][0]["state"], "available_packets_uninspected");
    action(
        &w,
        Action::InspectOriginals {
            revision: revision.clone(),
            limit: U64(10),
            inspection: OriginalInspection::Packets,
        },
    );
    let packets = read(
        &w,
        Query::Packets {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(10),
        },
    );
    let sequence = packets["rows"][0]["sequence"].as_i64().unwrap();
    for decoded in [false, true] {
        read(
            &w,
            Query::PacketBytes {
                revision: revision.clone(),
                sequence: I64(sequence),
                decoded,
                offset: I64(0),
                limit: U64(8),
            },
        );
    }
    close(&mut w);
    assert_eq!(source_before, fs::read(&source).unwrap());
    assert_eq!(raw_before, fs::read(&original).unwrap());
    assert_eq!(
        custody_before,
        fs::read(output.join("logical.sqlite3")).unwrap()
    );
}
