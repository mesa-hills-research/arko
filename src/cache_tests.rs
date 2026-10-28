// End-to-end tests of the prompt cache's time across closing a session and
// resuming it, as `arko -c` and an app do: through the engine, with the
// mock model. (Included in `engine`'s tests, whose helpers they use.)

const RECORD: &str = "prompt-cache.json";

/// A model that answers "Noted." and says its request read `cached` of its
/// 1000 input tokens from the prompt cache. `cached` changes as the test
/// goes: a cache that lapsed reads nothing.
async fn caching_model(cached: Arc<std::sync::atomic::AtomicUsize>) -> ModelServer {
    ModelServer::start(Arc::new(move |_, body: String| {
        let cached = cached.load(std::sync::atomic::Ordering::SeqCst);
        // (A request that only keeps the cache warm isn't streamed.)
        let streamed = body_streams(&body);
        Box::pin(async move {
            if !streamed {
                return crate::mock_model::whole_reply("a", "Noted.", cached);
            }
            reply("a", "Noted.").replace(
                "\"input_tokens\":10",
                &format!(
                    "\"input_tokens\":1000,\"input_tokens_details\":{{\"cached_tokens\":{cached}}}"
                ),
            )
        })
    }))
    .await
}

/// The session's record of its cache, as a JSON value.
fn cache_record(root: &std::path::Path, id: &str) -> serde_json::Value {
    let path = Session::open(root, id).unwrap().dir.join(RECORD);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Changes the first conversation's entry in the record.
fn change_record(root: &std::path::Path, id: &str, change: impl FnOnce(&mut serde_json::Value)) {
    let path = Session::open(root, id).unwrap().dir.join(RECORD);
    let mut record = cache_record(root, id);
    change(&mut record["conversations"][0]);
    std::fs::write(path, record.to_string()).unwrap();
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// GPT-6 Luna's cache on an API key.
const THIRTY_MINUTES: Duration = Duration::from_secs(30 * 60);

/// Sends `text`, and waits until the turn it starts ended with a request
/// that `ended` says is its own, then gives the cache's status.
async fn say_and_wait(
    seam: &Seam,
    text: &str,
    ended: impl Fn(&crate::remote::CacheStatus) -> bool,
) -> crate::remote::CacheStatus {
    seam.input.send(Input::Message(app_message(text))).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let cache = seam.meter.read().cache;
            if ended(&cache) && !seam.feed.activity().working {
                return cache;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

/// A session closed after a reply and resumed has its cache's time from the
/// wall clock: what is left of it, or that it expired, whatever the
/// record's request's age. A request after it expired misses the cache,
/// and starts a new time.
#[tokio::test]
async fn a_resumed_session_counts_its_cache_from_the_wall_clock() {
    let temp = tempfile::tempdir().unwrap();
    let cached = Arc::new(std::sync::atomic::AtomicUsize::new(900));
    let server = caching_model(cached.clone()).await;
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
    let id = seam.session.id.clone();
    assert_eq!(seam.meter.read().cache.refreshed_at, None);
    let first = say_and_wait(&seam, "One.", |cache| cache.read.is_some()).await;
    let at = first.refreshed_at.unwrap();
    assert_eq!(first.lifetime, Some(THIRTY_MINUTES));
    assert_eq!((first.read, first.written), (Some(900), Some(0)));
    quit(seam, ended).await;
    // Kept in the session's folder, with the request's own time.
    let record = cache_record(&root, &id);
    assert_eq!(record["version"], 1);
    assert_eq!(record["conversations"][0]["at"], at);
    assert_eq!(record["conversations"][0]["lifetime"], 1800);
    assert_eq!(
        record["conversations"][0]["model"],
        resolve_model("luna").unwrap().id
    );

    // Resumed at once: the request's time and what it read, and the time
    // the cache has left for two clocks.
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id.clone())).await;
    let restored = seam.meter.read().cache;
    assert_eq!(restored.refreshed_at, Some(at));
    assert_eq!(restored.lifetime, Some(THIRTY_MINUTES));
    assert_eq!((restored.read, restored.written), (Some(900), Some(0)));
    assert_eq!(restored.tokens, first.tokens);
    // For two clocks, 1:48 and a little over thirty minutes after the
    // request.
    use crate::cache_timer::CacheTimer;
    let timer = |cache: &crate::remote::CacheStatus, now: i64| CacheTimer::of(cache, false, now);
    let counting = timer(&restored, at + 108_000).unwrap();
    assert_eq!(counting.label(), "cache 28:12");
    assert!(!counting.warns());
    assert!(timer(&restored, at + 1_750_000).unwrap().warns());
    assert_eq!(timer(&restored, at + 1_801_000), Some(CacheTimer::Expired));
    assert!(
        CacheTimer::of(&restored, false, now_ms())
            .is_some_and(|timer| timer != CacheTimer::Expired)
    );
    quit(seam, ended).await;

    // Closed for forty minutes: the record's request is that old now, so
    // the cache expired meanwhile.
    change_record(&root, &id, |entry| {
        entry["at"] = (at - 2_400_000).into();
    });
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id.clone())).await;
    let expired = seam.meter.read().cache;
    assert_eq!(expired.refreshed_at, Some(at - 2_400_000));
    assert_eq!(
        CacheTimer::of(&expired, false, now_ms()),
        Some(CacheTimer::Expired)
    );
    assert_eq!(expired.tokens, first.tokens, "what a miss writes again");

    // The next message misses the cache, and the time starts over.
    cached.store(0, std::sync::atomic::Ordering::SeqCst);
    let before = now_ms();
    let fresh = say_and_wait(&seam, "Two.", |cache| cache.read == Some(0)).await;
    assert!(fresh.refreshed_at.unwrap() >= before);
    let counting = CacheTimer::of(&fresh, false, now_ms()).unwrap();
    assert!(matches!(counting, CacheTimer::Counting { left } if left > 1_790_000));
    assert_eq!((fresh.read, fresh.written), (Some(0), Some(0)), "a miss");
    quit(seam, ended).await;
    assert_eq!(
        cache_record(&root, &id)["conversations"][0]["at"],
        fresh.refreshed_at.unwrap()
    );
}

/// A record from a clock that was ahead (its request dated after now) counts
/// from now: never more than the lifetime left. One of another model, an
/// unreadable one and a session saved before it existed show no time.
#[tokio::test]
async fn a_record_that_cannot_be_trusted_shows_no_time() {
    let temp = tempfile::tempdir().unwrap();
    let cached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = caching_model(cached).await;
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
    let id = seam.session.id.clone();
    say_and_wait(&seam, "One.", |cache| cache.read.is_some()).await;
    quit(seam, ended).await;
    let resume = || async {
        let (seam, ended) =
            open_app(&root, &project, &server, SessionChoice::Resume(id.clone())).await;
        let cache = seam.meter.read().cache;
        quit(seam, ended).await;
        cache
    };

    let ahead = now_ms() + 3_600_000;
    change_record(&root, &id, |entry| entry["at"] = ahead.into());
    let cache = resume().await;
    assert!(cache.refreshed_at.unwrap() <= now_ms());
    let timer = crate::cache_timer::CacheTimer::of(&cache, false, now_ms()).unwrap();
    assert!(matches!(timer, crate::cache_timer::CacheTimer::Counting { left } if left <= 1_800_000));
    // (The resume kept the record as it was: it saves at the next request.)

    change_record(&root, &id, |entry| {
        entry["model"] = "claude-opus-5-5".into()
    });
    assert_eq!(resume().await.refreshed_at, None);

    // The cache of a lifetime that isn't the one the session counts.
    change_record(&root, &id, |entry| {
        entry["model"] = resolve_model("luna").unwrap().id.into();
        entry["lifetime"] = 3600.into();
    });
    assert_eq!(resume().await.refreshed_at, None);

    let path = Session::open(&root, &id).unwrap().dir.join(RECORD);
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(resume().await.refreshed_at, None);

    // Saved before the record existed.
    std::fs::remove_file(&path).unwrap();
    let cache = resume().await;
    assert_eq!(cache.refreshed_at, None);
    assert_eq!(cache.lifetime, Some(THIRTY_MINUTES), "its length is known");
    assert!(cache.tokens.is_some());
}

/// `/new` leaves no cache for the new conversation, and `/resume` back to
/// the first one has its time again. A session closed in the new
/// conversation resumes in it, without one.
#[tokio::test]
async fn the_cache_follows_the_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let cached = Arc::new(std::sync::atomic::AtomicUsize::new(900));
    let server = caching_model(cached).await;
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
    let id = seam.session.id.clone();
    let say = |text: &'static str| {
        seam.input.send(Input::Message(app_message(text))).unwrap();
    };
    let first = say_and_wait(&seam, "One.", |cache| cache.read.is_some()).await;
    let at = first.refreshed_at.unwrap();
    let settled = |wanted: Option<i64>| {
        let seam = &seam;
        async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                while seam.meter.read().cache.refreshed_at != wanted {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    say("/new");
    settled(None).await;
    assert_eq!(seam.meter.read().cache.tokens, None);
    let second = say_and_wait(&seam, "Two.", |cache| cache.read.is_some())
        .await
        .refreshed_at
        .unwrap();
    assert!(second >= at);
    say("/resume 1");
    settled(Some(at)).await;
    let back = seam.meter.read().cache;
    assert_eq!((back.read, back.written), (first.read, first.written));
    assert_eq!(back.lifetime, Some(THIRTY_MINUTES));
    // The second conversation has its own, kept apart.
    say("/resume 2");
    settled(Some(second)).await;
    say("/new");
    settled(None).await;
    quit(seam, ended).await;
    // The record keeps both, and a session resumed in a new conversation
    // has none.
    assert_eq!(
        cache_record(&root, &id)["conversations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
    assert_eq!(seam.meter.read().cache.refreshed_at, None);
    quit(seam, ended).await;
}

/// Keeping the cache warm saves its new time, so a resumed session counts
/// from the refresh.
#[tokio::test]
async fn a_refresh_is_kept_for_a_resume() {
    let temp = tempfile::tempdir().unwrap();
    let cached = Arc::new(std::sync::atomic::AtomicUsize::new(900));
    let server = caching_model(cached).await;
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
    let id = seam.session.id.clone();
    let at = say_and_wait(&seam, "One.", |cache| cache.read.is_some())
        .await
        .refreshed_at
        .unwrap();
    // Closed and resumed after a time away, the cache lapsing meanwhile.
    quit(seam, ended).await;
    change_record(&root, &id, |entry| entry["at"] = (at - 200_000).into());
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id.clone())).await;
    assert_eq!(seam.meter.read().cache.refreshed_at, Some(at - 200_000));
    let before = now_ms();
    let refreshed = ask_for(&seam, |reply| Request::RefreshCache { reply }).await;
    assert!(matches!(
        refreshed,
        Ok(crate::engine::Refreshed::OutOfBand { .. })
    ));
    let refreshed_at = seam.meter.read().cache.refreshed_at.unwrap();
    assert!(refreshed_at >= before);
    quit(seam, ended).await;
    assert_eq!(
        cache_record(&root, &id)["conversations"][0]["at"],
        refreshed_at
    );
}
