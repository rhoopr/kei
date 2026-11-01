//! Authenticated fallback controls; each library owns its own selected scope.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone)]
struct FallbackSession {
    cycle: usize,
    incomplete: bool,
    records: Arc<Vec<Value>>,
    delta: Arc<Vec<Value>>,
    rank_pages: Arc<AtomicUsize>,
    invalid_token: Arc<AtomicUsize>,
    fallback_pages: Arc<AtomicUsize>,
    incomplete_pages: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for FallbackSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"source-boundary","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(
                u64::try_from(self.records.len() / 2).unwrap(),
            ));
        }
        if url.contains("/records/query?") {
            self.rank_pages.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request["zoneID"], zone());
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            if self.cycle == 1 && self.invalid_token.load(Ordering::SeqCst) > 0 {
                self.fallback_pages.fetch_add(1, Ordering::SeqCst);
                if self.incomplete {
                    self.incomplete_pages.fetch_add(1, Ordering::SeqCst);
                    // A malformed rank response is not evidence of an empty page.
                    return Ok(json!({"records":"injected malformed rank page"}));
                }
            }
            return Ok(
                json!({"records":if offset == 0 {self.records.as_ref().clone()} else {vec![]}, "syncToken":"source-query"}),
            );
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let n = self.changes.fetch_add(1, Ordering::SeqCst);
            let cursor = request["zones"][0]["syncToken"].as_str().unwrap();
            if self.cycle == 1 && n == 0 {
                assert_eq!(cursor, "source-cycle-1");
                self.invalid_token.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"zones":[{"zoneID":zone(),"serverErrorCode":"BAD_REQUEST"}]}));
            }
            assert_eq!(
                cursor,
                match self.cycle {
                    0 => "source-boundary",
                    1 => "source-cycle-1",
                    _ => "source-query",
                }
            );
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":self.delta.as_ref(),"syncToken":format!("source-cycle-{}",self.cycle+1),"moreComing":false}]}),
            );
        }
        if url.contains("/records/lookup?") {
            assert_eq!(request["zoneID"], zone());
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|r| r["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        anyhow::bail!("unexpected fallback fixture request: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn receipt(
    db: &SqliteStateDb,
    library: &str,
    child: &str,
) -> (String, String, String, String, String) {
    db.acquire_lock("authenticated fallback receipt").unwrap().query_row(
        "SELECT scope,master,native_path,source_checksum,local_checksum FROM download_receipts WHERE library=?1 AND child=?2 AND version_size='original'",
        rusqlite::params![library, child], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).unwrap()
}

#[tokio::test]
async fn authenticated_invalid_token_fallback_requires_complete_coverage_and_preserves_peer_progress()
 {
    for incomplete in [true, false] {
        exercise_fallback(incomplete).await;
    }
}

async fn exercise_fallback(incomplete: bool) {
    let server = wiremock::MockServer::start().await;
    for target in ["/source", "/peer-old", "/peer-new"] {
        Mock::given(method("GET"))
            .and(path(target))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/source-new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(usize::from(!incomplete) as u64)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let source_records = Arc::new(pair(
        "source",
        "source.JPG",
        &format!("{}/source", server.uri()),
        61,
    ));
    let source_new = pair(
        "source-new",
        "source-new.JPG",
        &format!("{}/source-new", server.uri()),
        64,
    );
    let mut peer_old = pair(
        "peer-old",
        "peer-old.JPG",
        &format!("{}/peer-old", server.uri()),
        62,
    );
    peer_old[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
    let mut peer_new = pair(
        "peer-new",
        "peer-new.JPG",
        &format!("{}/peer-new", server.uri()),
        63,
    );
    peer_new[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut source_receipt = None;
    let mut initial_observation = None;
    for cycle in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let source_session = FallbackSession {
            cycle,
            incomplete,
            records: if cycle == 0 {
                source_records.clone()
            } else {
                Arc::new([source_records.as_ref().clone(), source_new.clone()].concat())
            },
            delta: Arc::new(vec![]),
            rank_pages: Arc::new(AtomicUsize::new(0)),
            invalid_token: Arc::new(AtomicUsize::new(0)),
            fallback_pages: Arc::new(AtomicUsize::new(0)),
            incomplete_pages: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let source = selected(db.clone(), zone(), Box::new(source_session.clone())).await;
        let peer_records = if cycle == 0 {
            peer_old.clone()
        } else {
            [peer_old.clone(), peer_new.clone()].concat()
        };
        let peer_session = HealthySession {
            cycle,
            records: Arc::new(peer_records),
            delta: Arc::new(if cycle == 0 { vec![] } else { peer_new.clone() }),
            inventory: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
        };
        let peer = selected(db.clone(), healthy_zone(), Box::new(peer_session)).await;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, excludes, groups, name| {
            let mut config = (*base(mode, excludes, groups, name)).clone();
            config.folder_structure = "none".into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = false;
            }
            Arc::new(config)
        };
        let result = run_cycle(
            &[&source, &peer],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let current_receipt = receipt(&db, ZONE, "asset-source");
        let scope: Value = serde_json::from_str(&current_receipt.0).unwrap();
        assert_eq!(scope["database"], "private");
        assert_eq!(scope["zone"], zone());
        assert_eq!(current_receipt.1, "source");
        let expected_digest = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
        assert_eq!(current_receipt.3, expected_digest);
        assert_eq!(current_receipt.4, expected_digest);
        assert_eq!(
            serde_json::from_str::<Value>(&current_receipt.2).unwrap(),
            json!({"encoding":"utf8", "path":root.join("source.JPG")})
        );
        assert_eq!(std::fs::read(root.join("source.JPG")).unwrap(), MEDIA);
        if let Some(previous) = &source_receipt {
            assert_eq!(&current_receipt, previous);
        }
        source_receipt = Some(current_receipt);
        let observed = db
            .get_metadata(&format!("observed_selection:v27:{ZONE}"))
            .await
            .unwrap();
        // A bare inventory is not promoted to a cursor-bound observation.
        assert_eq!(
            observed.is_some(),
            cycle == 0 || incomplete,
            "cycle={cycle} incomplete={incomplete}; {result:?}"
        );
        if cycle == 0 {
            initial_observation = observed.clone();
        }
        if cycle == 1 && incomplete {
            assert_eq!(
                observed, initial_observation,
                "incomplete fallback retains the exact prior observation"
            );
        }
        assert_eq!(
            db.get_metadata(&sync_token_key(HEALTHY)).await.unwrap(),
            Some(format!("healthy-cycle-{}", cycle + 1))
        );
        let token = db.get_metadata(&sync_token_key(ZONE)).await.unwrap();
        if cycle == 0 {
            assert_eq!(token.as_deref(), Some("source-cycle-1"));
        } else {
            assert_eq!(source_session.invalid_token.load(Ordering::SeqCst), 1);
            assert!(
                source_session.fallback_pages.load(Ordering::SeqCst) > 0,
                "fallback rank enumeration must be reached"
            );
            assert_eq!(
                source_session.incomplete_pages.load(Ordering::SeqCst) > 0,
                incomplete
            );
            assert_eq!(
                token.as_deref(),
                Some(if incomplete {
                    "source-cycle-1"
                } else {
                    "source-query"
                }),
                "incomplete={incomplete}; {result:?}"
            );
            if incomplete {
                assert!(
                    result.stats.enumeration_incomplete || result.stats.enumeration_errors > 0,
                    "{result:?}"
                );
            }
            let new_count: i64 = db.acquire_lock("new selected source receipt").unwrap().query_row(
                "SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-source-new'", [ZONE], |row| row.get(0)).unwrap();
            assert_eq!(new_count, i64::from(!incomplete));
            if !incomplete {
                let new_receipt = receipt(&db, ZONE, "asset-source-new");
                let new_scope: Value = serde_json::from_str(&new_receipt.0).unwrap();
                assert_eq!(new_scope["database"], "private");
                assert_eq!(new_scope["zone"], zone());
                assert_eq!(new_receipt.1, "source-new");
                assert_eq!(new_receipt.3, expected_digest);
                assert_eq!(new_receipt.4, expected_digest);
                assert_eq!(
                    serde_json::from_str::<Value>(&new_receipt.2).unwrap(),
                    json!({"encoding":"utf8", "path":root.join("source-new.JPG")})
                );
                assert_eq!(std::fs::read(root.join("source-new.JPG")).unwrap(), MEDIA);
            } else {
                assert!(!root.join("source-new.JPG").exists());
            }
            let peer_receipt = receipt(&db, HEALTHY, "asset-peer-new");
            let peer_scope: Value = serde_json::from_str(&peer_receipt.0).unwrap();
            assert_eq!(peer_scope["database"], "private");
            assert_eq!(peer_scope["zone"], healthy_zone());
            assert_eq!(peer_receipt.1, "peer-new");
            assert_eq!(peer_receipt.3, expected_digest);
            assert_eq!(peer_receipt.4, expected_digest);
            assert_eq!(
                serde_json::from_str::<Value>(&peer_receipt.2).unwrap(),
                json!({"encoding":"utf8", "path":root.join("peer-new.JPG")})
            );
            assert_eq!(std::fs::read(root.join("peer-new.JPG")).unwrap(), MEDIA);
        }
    }
    let reopened = SqliteStateDb::open_owned(&database, &baseline_owner())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_metadata(&sync_token_key(ZONE))
            .await
            .unwrap()
            .as_deref(),
        Some(if incomplete {
            "source-cycle-1"
        } else {
            "source-query"
        })
    );
    assert_eq!(
        reopened
            .get_metadata(&sync_token_key(HEALTHY))
            .await
            .unwrap()
            .as_deref(),
        Some("healthy-cycle-2")
    );
    assert_eq!(
        receipt(&reopened, ZONE, "asset-source"),
        source_receipt.unwrap()
    );
    server.verify().await;
}

#[tokio::test]
async fn fallback_reopened_delta_progress_without_selected_observation() {
    let incomplete = false;
    let server = wiremock::MockServer::start().await;
    for target in ["/source", "/peer-old", "/peer-new"] {
        Mock::given(method("GET"))
            .and(path(target))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/source-new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(usize::from(!incomplete) as u64)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/source-later"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let source_records = Arc::new(pair(
        "source",
        "source.JPG",
        &format!("{}/source", server.uri()),
        61,
    ));
    let source_new = pair(
        "source-new",
        "source-new.JPG",
        &format!("{}/source-new", server.uri()),
        64,
    );
    let source_later = pair(
        "source-later",
        "source-later.JPG",
        &format!("{}/source-later", server.uri()),
        65,
    );
    let mut peer_old = pair(
        "peer-old",
        "peer-old.JPG",
        &format!("{}/peer-old", server.uri()),
        62,
    );
    peer_old[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
    let mut peer_new = pair(
        "peer-new",
        "peer-new.JPG",
        &format!("{}/peer-new", server.uri()),
        63,
    );
    peer_new[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut source_receipt = None;
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let source_session = FallbackSession {
            cycle,
            incomplete,
            records: if cycle == 0 {
                source_records.clone()
            } else {
                Arc::new(
                    [
                        source_records.as_ref().clone(),
                        source_new.clone(),
                        if cycle == 2 {
                            source_later.clone()
                        } else {
                            vec![]
                        },
                    ]
                    .concat(),
                )
            },
            delta: Arc::new(if cycle == 2 {
                source_later.clone()
            } else {
                vec![]
            }),
            rank_pages: Arc::new(AtomicUsize::new(0)),
            invalid_token: Arc::new(AtomicUsize::new(0)),
            fallback_pages: Arc::new(AtomicUsize::new(0)),
            incomplete_pages: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let source = selected(db.clone(), zone(), Box::new(source_session.clone())).await;
        let peer_records = if cycle == 0 {
            peer_old.clone()
        } else {
            [peer_old.clone(), peer_new.clone()].concat()
        };
        let peer_session = HealthySession {
            cycle,
            records: Arc::new(peer_records),
            delta: Arc::new(if cycle == 1 { peer_new.clone() } else { vec![] }),
            inventory: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
        };
        let peer = selected(db.clone(), healthy_zone(), Box::new(peer_session)).await;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, excludes, groups, name| {
            let mut config = (*base(mode, excludes, groups, name)).clone();
            config.folder_structure = "none".into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = false;
            }
            Arc::new(config)
        };
        let result = run_cycle(
            &[&source, &peer],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let current_receipt = receipt(&db, ZONE, "asset-source");
        let scope: Value = serde_json::from_str(&current_receipt.0).unwrap();
        assert_eq!(scope["database"], "private");
        assert_eq!(scope["zone"], zone());
        assert_eq!(current_receipt.1, "source");
        let expected_digest = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
        assert_eq!(current_receipt.3, expected_digest);
        assert_eq!(current_receipt.4, expected_digest);
        assert_eq!(
            serde_json::from_str::<Value>(&current_receipt.2).unwrap(),
            json!({"encoding":"utf8", "path":root.join("source.JPG")})
        );
        assert_eq!(std::fs::read(root.join("source.JPG")).unwrap(), MEDIA);
        if let Some(previous) = &source_receipt {
            assert_eq!(&current_receipt, previous);
        }
        source_receipt = Some(current_receipt);
        let observed = db
            .get_metadata(&format!("observed_selection:v27:{ZONE}"))
            .await
            .unwrap();
        assert_eq!(observed.is_some(), cycle == 0, "cycle={cycle}; {result:?}");
        assert_eq!(
            db.get_metadata(&sync_token_key(HEALTHY)).await.unwrap(),
            Some(format!("healthy-cycle-{}", cycle + 1))
        );
        let token = db.get_metadata(&sync_token_key(ZONE)).await.unwrap();
        if cycle == 0 {
            assert_eq!(token.as_deref(), Some("source-cycle-1"));
        } else if cycle == 1 {
            assert_eq!(source_session.invalid_token.load(Ordering::SeqCst), 1);
            assert!(
                source_session.fallback_pages.load(Ordering::SeqCst) > 0,
                "fallback rank enumeration must be reached"
            );
            assert_eq!(
                source_session.incomplete_pages.load(Ordering::SeqCst) > 0,
                incomplete
            );
            assert_eq!(
                token.as_deref(),
                Some(if incomplete {
                    "source-cycle-1"
                } else {
                    "source-query"
                }),
                "incomplete={incomplete}; {result:?}"
            );
            if incomplete {
                assert!(
                    result.stats.enumeration_incomplete || result.stats.enumeration_errors > 0,
                    "{result:?}"
                );
            }
            let new_count: i64 = db.acquire_lock("new selected source receipt").unwrap().query_row(
                "SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-source-new'", [ZONE], |row| row.get(0)).unwrap();
            assert_eq!(new_count, i64::from(!incomplete));
            if !incomplete {
                let new_receipt = receipt(&db, ZONE, "asset-source-new");
                let new_scope: Value = serde_json::from_str(&new_receipt.0).unwrap();
                assert_eq!(new_scope["database"], "private");
                assert_eq!(new_scope["zone"], zone());
                assert_eq!(new_receipt.1, "source-new");
                assert_eq!(new_receipt.3, expected_digest);
                assert_eq!(new_receipt.4, expected_digest);
                assert_eq!(
                    serde_json::from_str::<Value>(&new_receipt.2).unwrap(),
                    json!({"encoding":"utf8", "path":root.join("source-new.JPG")})
                );
                assert_eq!(std::fs::read(root.join("source-new.JPG")).unwrap(), MEDIA);
            } else {
                assert!(!root.join("source-new.JPG").exists());
            }
            let peer_receipt = receipt(&db, HEALTHY, "asset-peer-new");
            let peer_scope: Value = serde_json::from_str(&peer_receipt.0).unwrap();
            assert_eq!(peer_scope["database"], "private");
            assert_eq!(peer_scope["zone"], healthy_zone());
            assert_eq!(peer_receipt.1, "peer-new");
            assert_eq!(peer_receipt.3, expected_digest);
            assert_eq!(peer_receipt.4, expected_digest);
            assert_eq!(
                serde_json::from_str::<Value>(&peer_receipt.2).unwrap(),
                json!({"encoding":"utf8", "path":root.join("peer-new.JPG")})
            );
            assert_eq!(std::fs::read(root.join("peer-new.JPG")).unwrap(), MEDIA);
        } else {
            assert_eq!(token.as_deref(), Some("source-cycle-3"));
            assert_eq!(
                source_session.rank_pages.load(Ordering::SeqCst),
                0,
                "the reopened delta must not repeat rank inventory"
            );
            assert_eq!(source_session.changes.load(Ordering::SeqCst), 1);
            let later = receipt(&db, ZONE, "asset-source-later");
            let scope: Value = serde_json::from_str(&later.0).unwrap();
            assert_eq!(scope["database"], "private");
            assert_eq!(scope["zone"], zone());
            assert_eq!(later.1, "source-later");
            assert_eq!(later.3, expected_digest);
            assert_eq!(later.4, expected_digest);
            assert_eq!(
                serde_json::from_str::<Value>(&later.2).unwrap(),
                json!({"encoding":"utf8", "path":root.join("source-later.JPG")})
            );
            assert_eq!(std::fs::read(root.join("source-later.JPG")).unwrap(), MEDIA);
            assert_eq!(receipt(&db, ZONE, "asset-source-new").1, "source-new");
        }
    }
    let reopened = SqliteStateDb::open_owned(&database, &baseline_owner())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_metadata(&sync_token_key(ZONE))
            .await
            .unwrap()
            .as_deref(),
        Some("source-cycle-3")
    );
    assert_eq!(
        reopened
            .get_metadata(&sync_token_key(HEALTHY))
            .await
            .unwrap()
            .as_deref(),
        Some("healthy-cycle-3")
    );
    assert_eq!(
        receipt(&reopened, ZONE, "asset-source"),
        source_receipt.unwrap()
    );
    assert!(
        reopened
            .get_metadata(&format!("observed_selection:v27:{ZONE}"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        receipt(&reopened, ZONE, "asset-source-later").1,
        "source-later"
    );
    server.verify().await;
}
