//! `cargo xtask <task>`: developer tasks for the Commons workspace.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

mod release;

/// Crates the UI must never link, directly or transitively (decision 2).
const UI_FORBIDDEN: &[&str] = &[
    "client-core",
    "server",
    "rusqlite",
    "libsqlite3-sys",
    "sqlx",
    "reqwest",
    "hyper",
    "tokio-tungstenite",
    "tungstenite",
    "keyring",
    "object_store",
    "mio",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("dev") => dev(&args[1..]),
        Some("app") => app(&args[1..]),
        Some("check-deps") => check_deps(),
        Some("spike") => spike(),
        Some("openapi") => openapi(),
        Some("release") => release::release(&args[1..]),
        _ => {
            eprintln!("usage: cargo xtask <dev | app | check-deps | spike | openapi | release>");
            eprintln!("  dev         start Postgres in Docker, run commonsd and the desktop app");
            eprintln!("  app         build the desktop app once and run it (no file watcher)");
            eprintln!("  check-deps  fail if the UI crate links storage or network crates");
            eprintln!("  spike       build the app and measure the M0 webview spike");
            eprintln!("  openapi     write docs/api/openapi.json");
            eprintln!("  release     build the signed, notarized Mac app, dmg and update files");
            std::process::exit(2);
        }
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed: {status}");
    }
    Ok(())
}

fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---- dev ----

const DEV_PG: &str = "commons-dev-pg";
const DEV_URL: &str = "postgres://commons:commons@127.0.0.1:5544/commons";

/// Builds the desktop app once, with the trunk-built UI embedded, and returns its binary.
/// Not `cargo tauri dev`: it rebuilds on any workspace change, and on macOS a Ctrl+C can
/// orphan its cleanup script, which then spins forever.
fn build_app() -> Result<PathBuf> {
    run(Command::new("cargo")
        .args(["tauri", "build", "--debug", "--no-bundle"])
        .current_dir(root().join("apps/desktop")))?;
    Ok(root().join(format!(
        "target/debug/commons-desktop{}",
        std::env::consts::EXE_SUFFIX
    )))
}

/// Runs the app with these arguments and this process's environment.
fn run_app(bin: &Path, args: &[String]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("launching {}", bin.display()))?;
    if !status.success() {
        bail!("the app exited with {status}");
    }
    Ok(())
}

fn app(args: &[String]) -> Result<()> {
    run_app(&build_app()?, args)
}

fn dev(args: &[String]) -> Result<()> {
    let running =
        output(Command::new("docker").args(["ps", "-q", "-f", &format!("name=^{DEV_PG}$")]))?;
    if running.trim().is_empty() {
        let _ = Command::new("docker").args(["rm", "-f", DEV_PG]).output();
        run(Command::new("docker").args([
            "run",
            "-d",
            "--name",
            DEV_PG,
            "-p",
            "127.0.0.1:5544:5432",
            "-e",
            "POSTGRES_USER=commons",
            "-e",
            "POSTGRES_PASSWORD=commons",
            "-e",
            "POSTGRES_DB=commons",
            "-v",
            "commons-dev-pg:/var/lib/postgresql/data",
            "postgres:16-alpine",
        ]))?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while !Command::new("docker")
        .args(["exec", DEV_PG, "pg_isready", "-U", "commons"])
        .stdout(Stdio::null())
        .status()?
        .success()
    {
        if Instant::now() > deadline {
            bail!("Postgres did not become ready");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let app = build_app()?;
    run(Command::new("cargo")
        .args(["build", "-p", "server", "--bin", "commonsd"])
        .current_dir(root()))?;
    // The binary itself, not `cargo run`, so stopping it stops the server.
    let mut server: Child = Command::new(root().join(format!(
        "target/debug/commonsd{}",
        std::env::consts::EXE_SUFFIX
    )))
    .env("COMMONS_DATABASE_URL", DEV_URL)
    .env("COMMONS_FILES_DIR", root().join("target/dev-files"))
    .current_dir(root())
    .spawn()?;
    let result = run_app(&app, args);
    let _ = server.kill();
    let _ = server.wait();
    result
}

// ---- check-deps ----

fn check_deps() -> Result<()> {
    let meta = output(Command::new("cargo").current_dir(root()).args([
        "metadata",
        "--format-version",
        "1",
        "--filter-platform",
        "wasm32-unknown-unknown",
    ]))?;
    let v: serde_json::Value = serde_json::from_str(&meta)?;
    let names: HashMap<String, String> = v["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p["id"].as_str()?.to_string(),
                p["name"].as_str()?.to_string(),
            ))
        })
        .collect();
    let nodes: HashMap<String, Vec<String>> = v["resolve"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let deps = n["deps"]
                .as_array()?
                .iter()
                .filter(|d| {
                    d["dep_kinds"].as_array().is_some_and(|k| {
                        k.iter()
                            .any(|k| k["kind"].is_null() || k["kind"] == "build")
                    })
                })
                .filter_map(|d| d["pkg"].as_str().map(str::to_string))
                .collect();
            Some((n["id"].as_str()?.to_string(), deps))
        })
        .collect();
    let ui = names
        .iter()
        .find(|(_, n)| *n == "desktop-ui")
        .map(|(id, _)| id.clone())
        .context("desktop-ui not in workspace")?;
    let mut seen = HashSet::new();
    let mut stack = vec![ui];
    while let Some(id) = stack.pop() {
        if seen.insert(id.clone()) {
            stack.extend(nodes.get(&id).cloned().unwrap_or_default());
        }
    }
    let linked: BTreeSet<&str> = seen
        .iter()
        .filter_map(|id| names.get(id).map(String::as_str))
        .collect();
    let bad: Vec<&&str> = UI_FORBIDDEN
        .iter()
        .filter(|f| linked.contains(**f))
        .collect();
    if !bad.is_empty() {
        bail!("desktop-ui must not depend on storage or network crates, but links {bad:?}");
    }
    println!(
        "desktop-ui links {} crates; none are storage or network crates",
        linked.len()
    );
    Ok(())
}

// ---- spike ----

fn webkit_pids() -> Result<HashSet<u32>> {
    let out = output(Command::new("pgrep").args(["-f", "com.apple.WebKit"]))?;
    Ok(out.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

fn rss_kb(pid: u32) -> Option<u64> {
    output(Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn proc_name(pid: u32) -> String {
    output(Command::new("ps").args(["-o", "comm=", "-p", &pid.to_string()]))
        .unwrap_or_default()
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn spike() -> Result<()> {
    let root = root();
    run(Command::new("cargo")
        .args(["tauri", "build", "--no-bundle"])
        .current_dir(root.join("apps/desktop")))?;
    let bin = root.join("target/release/commons-desktop");
    let out_dir = root.join("target/spike");
    std::fs::create_dir_all(&out_dir)?;
    let ui_report = out_dir.join("ui.json");
    let _ = std::fs::remove_file(&ui_report);
    let before = webkit_pids()?;
    let started = Instant::now();
    let mut child = Command::new(&bin)
        .env("COMMONS_SPIKE", "1")
        .env("COMMONS_SPIKE_OUT", &ui_report)
        .spawn()
        .with_context(|| format!("launching {}", bin.display()))?;
    let pid = child.id();
    let mut samples = vec![];
    let mut peak_total = 0u64;
    let mut peak_by: HashMap<String, u64> = HashMap::new();
    let mut complete = true;
    loop {
        if let Some(status) = child.try_wait()? {
            println!("app exited: {status}");
            break;
        }
        if started.elapsed() > Duration::from_secs(300) {
            // Frame timing stalls when the screen is locked or the window hidden; keep the
            // memory samples and mark the run incomplete.
            let _ = child.kill();
            complete = false;
            eprintln!("spike did not finish within 300 s; is the screen unlocked?");
            break;
        }
        let app = rss_kb(pid).unwrap_or(0);
        let mut webkit = 0;
        let mut parts = vec![("app".to_string(), app)];
        for p in webkit_pids()?.difference(&before) {
            if let Some(r) = rss_kb(*p) {
                webkit += r;
                parts.push((format!("{}:{p}", proc_name(*p)), r));
            }
        }
        let total = app + webkit;
        peak_total = peak_total.max(total);
        for (name, r) in &parts {
            let key = name.split(':').next().unwrap_or("").to_string();
            let e = peak_by.entry(key).or_default();
            *e = (*e).max(*r);
        }
        samples.push(serde_json::json!({
            "t_ms": started.elapsed().as_millis() as u64,
            "app_kb": app, "webkit_kb": webkit, "total_kb": total,
        }));
        std::thread::sleep(Duration::from_millis(500));
    }
    let ui: serde_json::Value = std::fs::read(&ui_report)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null);
    let totals: Vec<u64> = samples
        .iter()
        .filter_map(|s| s["total_kb"].as_u64())
        .collect();
    let peak_by_mb: HashMap<String, f64> = peak_by
        .iter()
        .map(|(k, v)| (k.clone(), *v as f64 / 1024.0))
        .collect();
    let results = serde_json::json!({
        "complete": complete,
        "messages": 10_000, "accounts": 3, "workspaces": 20,
        "peak_total_mb": peak_total as f64 / 1024.0,
        "final_total_mb": totals.iter().rev().nth(1).or(totals.last()).copied().unwrap_or(0) as f64 / 1024.0,
        "peak_by_process_mb": peak_by_mb,
        "ui": ui,
        "samples": samples,
    });
    let path = out_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&results)?)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "complete": complete,
            "peak_total_mb": results["peak_total_mb"],
            "final_total_mb": results["final_total_mb"],
            "peak_by_process_mb": results["peak_by_process_mb"],
            "ui": results["ui"],
        }))?
    );
    println!("full results: {}", path.display());
    Ok(())
}

// ---- openapi ----

fn openapi() -> Result<()> {
    use utoipa::OpenApi;
    let doc = server::openapi::ApiDoc::openapi().to_pretty_json()?;
    let path = root().join("docs/api/openapi.json");
    std::fs::create_dir_all(path.parent().context("docs/api")?)?;
    std::fs::write(&path, doc + "\n")?;
    println!("wrote {}", path.display());
    Ok(())
}
