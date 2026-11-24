//! End to end tests of the router against a real database.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use elkar_core::client::ClientSpec;
use elkar_core::ids::{ClientId, UserId};
use elkar_core::user::{Role, UserSpec};
use elkar_engine::{Engine, EngineConfig, NullNotifier, SystemRunner};
use elkar_storage::Store;
use elkar_web::{AppState, WebConfig, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

struct App {
    router: Router,
    store: Store,
    _dir: tempfile::TempDir,
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    cookie: Option<String>,
    headers: axum::http::HeaderMap,
    body: String,
}

impl App {
    async fn new(pool: PgPool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::from_pool(pool);
        let engine = Engine {
            store: store.clone(),
            config: Arc::new(EngineConfig::rooted_at(dir.path())),
            runner: Arc::new(SystemRunner),
            notifier: Arc::new(NullNotifier),
        };
        let router = router(AppState::new(engine, WebConfig::default()));
        Self {
            router,
            store,
            _dir: dir,
        }
    }

    async fn user(&self, name: &str, role: Role) -> UserId {
        let spec = UserSpec {
            username: name.into(),
            email: format!("{name}@example.com"),
            role,
            is_active: true,
            lines_per_page: 20,
        };
        self.store
            .users()
            .create(&spec, &hash_password("password123").unwrap())
            .await
            .unwrap()
    }

    async fn client(&self, name: &str, owner: Option<UserId>) -> ClientId {
        self.store
            .clients()
            .create(&ClientSpec {
                name: name.into(),
                description: String::new(),
                url: String::new(),
                is_active: true,
                quota_kb: None,
                owner_id: owner,
                ssh_args: String::new(),
                rsync_short_args: String::new(),
                rsync_long_args: String::new(),
                max_parallel_jobs: 1,
                pre_scripts: vec![],
                post_scripts: vec![],
            })
            .await
            .unwrap()
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let location = headers.get(header::LOCATION).map(|v| v.to_str().unwrap().to_owned());
        let cookie = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
            .find(|c| c.starts_with("elkar_session="));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            cookie,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, uri: &str, cookie: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post_form(&self, uri: &str, cookie: &str, form: &str) -> Reply {
        self.send(
            Request::post(uri)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await
    }

    async fn login(&self, name: &str) -> String {
        let reply = self
            .post_form("/login", "", &format!("username={name}&password=password123"))
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply.cookie.expect("session cookie")
    }

    async fn api(&self, method: &str, uri: &str, user: &str, body: Option<serde_json::Value>) -> Reply {
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:password123"));
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Basic {credentials}"));
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        self.send(request.body(body).unwrap()).await
    }
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn login_logout_flow(pool: PgPool) {
    let app = App::new(pool).await;
    app.user("admin", Role::Admin).await;

    let anonymous = app.get("/clients", "").await;
    assert_eq!(anonymous.status, StatusCode::SEE_OTHER);
    assert_eq!(anonymous.location.as_deref(), Some("/login"));

    let wrong = app.post_form("/login", "", "username=admin&password=nope").await;
    assert_eq!(wrong.status, StatusCode::OK);
    assert!(wrong.body.contains("Invalid user name or password"));
    assert!(wrong.cookie.is_none());

    let cookie = app.login("admin").await;
    let page = app.get("/clients", &cookie).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No clients yet"));

    app.post_form("/logout", &cookie, "").await;
    assert_eq!(app.get("/clients", &cookie).await.status, StatusCode::SEE_OTHER);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn users_only_reach_their_own_clients(pool: PgPool) {
    let app = App::new(pool).await;
    app.user("admin", Role::Admin).await;
    let ann = app.user("ann", Role::User).await;
    let bob = app.user("bob", Role::User).await;
    let anns = app.client("anns-server", Some(ann)).await;
    let bobs = app.client("bobs-server", Some(bob)).await;

    let cookie = app.login("ann").await;
    let list = app.get("/clients", &cookie).await;
    assert!(list.body.contains("anns-server"));
    assert!(!list.body.contains("bobs-server"));
    assert_eq!(
        app.get(&format!("/client/{anns}"), &cookie).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.get(&format!("/client/{bobs}"), &cookie).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(app.get("/policies", &cookie).await.status, StatusCode::FORBIDDEN);
    assert_eq!(app.get("/users", &cookie).await.status, StatusCode::FORBIDDEN);

    // A user cannot hand a client to someone else.
    let reply = app
        .post_form(
            &format!("/client/{anns}"),
            &cookie,
            &format!("name=anns-server&owner_id={bob}&is_active=true&max_parallel_jobs=1"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(app.store.clients().get(anns).await.unwrap().spec.owner_id, Some(ann));

    // ...nor inject arguments into the commands the server runs.
    let reply = app
        .post_form(
            &format!("/client/{anns}"),
            &cookie,
            "name=anns-server&rsync_long_args=--rsh%3Dsh&ssh_args=-oProxyCommand%3Dx&is_active=true&max_parallel_jobs=1",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let spec = app.store.clients().get(anns).await.unwrap().spec;
    assert_eq!((spec.rsync_long_args.as_str(), spec.ssh_args.as_str()), ("", ""));

    let admin = app.login("admin").await;
    let list = app.get("/clients", &admin).await;
    assert!(list.body.contains("bobs-server"));
    assert_eq!(app.get("/policies", &admin).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn forms_validate_and_save(pool: PgPool) {
    let app = App::new(pool).await;
    app.user("admin", Role::Admin).await;
    let cookie = app.login("admin").await;

    let invalid = app
        .post_form("/client/new", &cookie, "name=&url=bad+url&max_parallel_jobs=0")
        .await;
    assert_eq!(invalid.status, StatusCode::OK);
    assert!(invalid.body.contains("name is required"), "{}", invalid.body);
    assert!(invalid.body.contains("url must not contain whitespace"));

    let created = app
        .post_form(
            "/client/new",
            &cookie,
            "name=web1&url=root%40web1&is_active=true&max_parallel_jobs=2&quota_kb=",
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let id = created
        .location
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let client = app.store.clients().get(ClientId(id)).await.unwrap();
    assert_eq!(client.spec.url, "root@web1");
    assert_eq!(client.spec.max_parallel_jobs, 2);

    let flash = app.get(&format!("/client/{id}"), &cookie).await;
    assert!(flash.body.contains("Client saved"));
    let again = app.get(&format!("/client/{id}"), &cookie).await;
    assert!(!again.body.contains("Client saved"), "flash shows once");

    let policy = app
        .post_form(
            "/policy/new",
            &cookie,
            "name=Nightly&daily_enabled=true&daily_count=7&daily_times=21%3A00&weekly_enabled=true&weekly_count=4&weekly_times=21%3A00&weekly_weekdays=sun",
        )
        .await;
    assert_eq!(policy.status, StatusCode::SEE_OTHER, "{}", policy.body);
    let policies = app.store.policies().list().await.unwrap();
    assert_eq!(policies[0].retains().len(), 2);

    let bad_policy = app
        .post_form(
            "/policy/new",
            &cookie,
            "name=Bad&daily_enabled=true&daily_count=7&daily_times=25%3A00",
        )
        .await;
    assert_eq!(bad_policy.status, StatusCode::OK);
    assert!(bad_policy.body.contains("invalid time"));

    let location = app
        .post_form(
            "/location/new",
            &cookie,
            "name=Main&directory=%2Fbackups&max_parallel_jobs=1",
        )
        .await;
    assert_eq!(location.status, StatusCode::SEE_OTHER);

    let job = app
        .post_form(
            &format!("/client/{id}/job/new"),
            &cookie,
            &format!(
                "name=etc&path=%2Fetc&is_active=true&policy_id={}&location_id={}&notify_to=owner&min_notification_level=error&priority=5",
                policies[0].id,
                app.store.locations().list().await.unwrap()[0].id
            ),
        )
        .await;
    assert_eq!(job.status, StatusCode::SEE_OTHER, "{}", job.body);

    let jobs = app.store.jobs().list_for_client(ClientId(id)).await.unwrap();
    let config = app
        .get(&format!("/client/{id}/job/{}/config", jobs[0].id), &cookie)
        .await;
    assert!(config.body.contains("backup\troot@web1:/etc\t."), "{}", config.body);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn running_jobs_by_session_and_token(pool: PgPool) {
    let app = App::new(pool).await;
    app.user("admin", Role::Admin).await;
    let cookie = app.login("admin").await;
    let client = app.client("c", None).await;
    let policy = app
        .store
        .policies()
        .create(&elkar_core::policy::PolicySpec {
            name: "p".into(),
            retains: vec![elkar_core::policy::RetainRule {
                level: elkar_core::policy::RetainLevel::Daily,
                count: 1,
                schedule: elkar_core::schedule::Schedule {
                    times: elkar_core::schedule::parse::times("21:00").unwrap(),
                    ..Default::default()
                },
            }],
            ..Default::default()
        })
        .await
        .unwrap();
    let location = app
        .store
        .locations()
        .create(&elkar_core::location::LocationSpec {
            name: "l".into(),
            host: None,
            directory: "/backups".into(),
            max_parallel_jobs: 1,
        })
        .await
        .unwrap();
    let job: serde_json::Value = serde_json::from_str(
        &app.api(
            "POST",
            "/api/jobs",
            "admin",
            Some(serde_json::json!({
                "client_id": client.0, "name": "etc", "path": "/etc",
                "policy_id": policy.0, "location_id": location.0
            })),
        )
        .await
        .body,
    )
    .unwrap();
    let job_id = job["id"].as_i64().unwrap();
    let run = format!("/client/{client}/job/{job_id}/run");

    let anonymous = app.post_form(&run, "", "").await;
    assert_eq!(anonymous.status, StatusCode::FORBIDDEN);

    app.post_form(&format!("/client/{client}/job/{job_id}/token"), &cookie, "")
        .await;
    let token = app.store.jobs().get(job_id.into()).await.unwrap().token.unwrap();
    let wrong = app.post_form(&format!("{run}?token=nope"), "", "").await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let queued = app.post_form(&format!("{run}?token={token}"), "", "").await;
    assert_eq!(queued.status, StatusCode::OK);
    assert!(queued.body.contains("\"error\":false"), "{}", queued.body);

    let again = app.post_form(&run, &cookie, "").await;
    assert_eq!(again.status, StatusCode::SEE_OTHER);
    let clients = app.get("/clients", &cookie).await;
    assert!(clients.body.contains("already queued"));

    let status = app.get("/status", &cookie).await;
    assert!(status.body.contains("queued"));
    let abort = app
        .api("POST", &format!("/api/jobs/{job_id}/abort"), "admin", None)
        .await;
    assert!(abort.body.contains("\"aborting\":true"));
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn api_authenticates_and_enforces_ownership(pool: PgPool) {
    let app = App::new(pool).await;
    app.user("admin", Role::Admin).await;
    let ann = app.user("ann", Role::User).await;
    let other = app.client("other", None).await;

    let unauthenticated = app
        .send(Request::get("/api/clients").body(Body::empty()).unwrap())
        .await;
    assert_eq!(unauthenticated.status, StatusCode::UNAUTHORIZED);
    assert!(unauthenticated.headers.contains_key(header::WWW_AUTHENTICATE));

    let created = app
        .api(
            "POST",
            "/api/clients",
            "ann",
            Some(serde_json::json!({ "name": "mine", "owner_id": 999 })),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let listed: Vec<serde_json::Value> =
        serde_json::from_str(&app.api("GET", "/api/clients", "ann", None).await.body).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["owner_id"], ann.0);

    assert_eq!(
        app.api("GET", &format!("/api/clients/{other}"), "ann", None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.api("GET", "/api/users", "ann", None).await.status,
        StatusCode::FORBIDDEN
    );

    let invalid = app
        .api(
            "POST",
            "/api/clients",
            "admin",
            Some(serde_json::json!({ "name": "", "max_parallel_jobs": 0 })),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(invalid.body.contains("details"));

    let duplicate = app
        .api(
            "POST",
            "/api/clients",
            "admin",
            Some(serde_json::json!({ "name": "other" })),
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);

    let deleted = app.api("DELETE", &format!("/api/clients/{other}"), "admin", None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn browsing_stays_inside_the_snapshot(pool: PgPool) {
    let app = App::new(pool).await;
    app.user("admin", Role::Admin).await;
    let cookie = app.login("admin").await;
    let backups = app._dir.path().join("backups");
    let client = app.client("c", None).await;
    let policy = app
        .store
        .policies()
        .create(&elkar_core::policy::PolicySpec {
            name: "p".into(),
            retains: vec![elkar_core::policy::RetainRule {
                level: elkar_core::policy::RetainLevel::Daily,
                count: 1,
                schedule: elkar_core::schedule::Schedule {
                    times: elkar_core::schedule::parse::times("21:00").unwrap(),
                    ..Default::default()
                },
            }],
            ..Default::default()
        })
        .await
        .unwrap();
    let location = app
        .store
        .locations()
        .create(&elkar_core::location::LocationSpec {
            name: "l".into(),
            host: None,
            directory: backups.display().to_string(),
            max_parallel_jobs: 1,
        })
        .await
        .unwrap();
    let job = app
        .store
        .jobs()
        .create(
            client,
            &serde_json::from_value(serde_json::json!({
                "name": "etc", "path": "/etc", "policy_id": policy.0, "location_id": location.0
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let root = backups.join(client.padded()).join(job.padded());
    std::fs::create_dir_all(root.join("daily.0/etc")).unwrap();
    std::fs::write(root.join("daily.0/etc/hosts"), "127.0.0.1 localhost\n").unwrap();
    std::fs::write(app._dir.path().join("secret"), "s").unwrap();

    let base = format!("/client/{client}/job/{job}/backup");
    let listing = app.get(&format!("{base}/"), &cookie).await;
    assert!(listing.body.contains("daily.0"), "{}", listing.body);
    let file = app.get(&format!("{base}/daily.0/etc/hosts"), &cookie).await;
    assert_eq!(file.body, "127.0.0.1 localhost\n");
    let escape = app.get(&format!("{base}/..%2F..%2F..%2Fsecret"), &cookie).await;
    assert!(
        matches!(escape.status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND),
        "{}",
        escape.status
    );

    let archive = app.get(&format!("{base}/daily.0/etc?download=true"), &cookie).await;
    assert_eq!(archive.status, StatusCode::OK);
    assert_eq!(archive.headers[header::CONTENT_TYPE], "application/gzip");

    let restore = app
        .get(
            &format!("/client/{client}/job/{job}/restore/daily.0/etc/hosts"),
            &cookie,
        )
        .await;
    assert!(restore.body.contains("value=\"/etc\""), "{}", restore.body);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn health_check_needs_no_login(pool: PgPool) {
    let app = App::new(pool).await;
    assert_eq!(app.get("/healthz", "").await.status, StatusCode::NO_CONTENT);
}
