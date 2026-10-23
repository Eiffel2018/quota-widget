#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod ui;
use quota_widget::{
    display_time, storage,
    token_db::TokenDb,
    tokens::{Pricing, TokenStore},
    usage_views::{self, GroupBy},
};
use std::{path::PathBuf, process::ExitCode};
fn main() -> ExitCode {
    #[cfg(windows)]
    if std::env::args().skip(1).any(|a| {
        matches!(
            a.as_str(),
            "inspect" | "import" | "export" | "--help" | "-h" | "--version"
        )
    }) {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
        }
        // Reuse an invoking terminal; never create a console during normal GUI launch.
        unsafe {
            AttachConsole(u32::MAX);
        }
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("quota-widget: {e}");
            ExitCode::from(1)
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!("Quota Widget {}\nNative read-only Codex quota + local token history.\n\nGUI:\n  quota-widget [--demo|--offline] [--no-settings] [--data-dir PATH]\n  --demo         Synthetic data only; never starts CLI or scans real sessions\n  --offline      Never starts CLI; local import remains user-configurable\n  --no-settings  Ephemeral state; no preference/history loading or saving\n  --show-tokens  Open daily token details\n  --show-history Open daily quota history\n  --show-settings Open settings and connection window\n\nMachine interface:\n  quota-widget inspect --sessions PATH [--state PATH] [--pricing PATH]\n    Read-only import. JSON schema version 1 on stdout. Does not modify inputs.\n  quota-widget import --sessions PATH --state PATH [--pricing PATH]\n    Incrementally import into SQLite state, guarded by writer lock.\n  quota-widget export --state PATH --output PATH\n    Export complete current accounting to a new compatibility JSON file.\n  quota-widget --version\n\nExit codes: 0 success, 1 invalid arguments or operation failed.\nNo login, inference, purchases, or credential handling.", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|s| s == "--version") {
        println!("quota-widget {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if matches!(
        args.first().map(String::as_str),
        Some("inspect" | "import" | "export")
    ) {
        let exporting = args[0] == "export";
        let options: &[&str] = if exporting {
            &["--state", "--output"]
        } else {
            &["--sessions", "--state", "--pricing"]
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut i = 1;
        while i < args.len() {
            if !options.contains(&args[i].as_str()) {
                return Err(format!("Unknown argument: {}", args[i]).into());
            }
            if !seen.insert(args[i].clone()) {
                return Err(format!("Repeated option: {}", args[i]).into());
            }
            i += 1;
            if args.get(i).is_none_or(|v| v.starts_with("--")) {
                return Err("A path is required after each option".into());
            }
            i += 1;
        }
        let get = |key: &str| {
            args.iter()
                .position(|a| a == key)
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from)
        };
        let state = get("--state");
        if exporting {
            let state = state
                .as_ref()
                .ok_or("--state PATH is required for export")?;
            let destination = get("--output").ok_or("--output PATH is required for export")?;
            if destination.exists() {
                return Err("Export destination exists; original preserved".into());
            }
            let store = match TokenDb::open_or_migrate(state, false, &mut |_| {})? {
                Some(db) => db.export_store()?,
                None if state.exists() => TokenStore::load(state)?,
                None => return Err("No accounting state exists to export".into()),
            };
            store.save_new(&destination)?;
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"mode":"export", "output":destination,
                "events":store.events().len(),"observations":store.observation_count(),"revision":store.revision()})
            );
            return Ok(());
        }
        let root = get("--sessions").ok_or("--sessions PATH is required")?;
        let write = args[0] == "import";
        let _lock = if write {
            let path = state
                .as_ref()
                .ok_or("--state PATH is required for import")?;
            let directory = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            Some(storage::DataLock::acquire(directory)?)
        } else {
            None
        };
        let explicit_pricing: Option<Pricing> = match get("--pricing") {
            Some(path) => Some(storage::load_json(&path)?),
            None => None,
        };
        // An existing SQLite database is authoritative. Any open/read failure
        // propagates; never silently fall back to an old JSON snapshot.
        let database = match &state {
            Some(path) => TokenDb::open_or_migrate(path, write, &mut |_| {})?,
            None => None,
        };
        let (mut store, report, pricing) = if write {
            let mut db = database.ok_or("Could not open authoritative token database")?;
            let header = db.header()?;
            let pricing =
                explicit_pricing.unwrap_or_else(|| header.pricing.clone().unwrap_or_default());
            let attribution_change = header.requires_attribution_rebuild(&pricing);
            let expected_revision = header.revision;
            let mut importer = TokenStore::from_header(header);
            if attribution_change {
                for id in db.thread_ids()? {
                    importer.hydrate_thread(
                        db.load_thread(&id)?
                            .ok_or("Missing thread during repricing")?,
                    )?;
                }
                importer.reprice_hydrated(&pricing);
            }
            let report = importer.import_dir_hydrated(
                &root,
                None,
                &pricing,
                &mut |id| db.load_thread(id),
                &mut |_| {},
            )?;
            let export = importer.dirty_export();
            if export.header.revision != expected_revision || !export.threads.is_empty() {
                db.commit(&export, expected_revision)?;
            }
            importer.mark_committed();
            // This explicit machine interface returns every event. Its final
            // export is intentionally full-cost; GUI queries remain bounded.
            (db.export_store()?, report, pricing)
        } else {
            let mut store = match database {
                Some(db) => db.export_store()?,
                None => match &state {
                    Some(path) if path.exists() => TokenStore::load(path)?,
                    _ => TokenStore::default(),
                },
            };
            let pricing =
                explicit_pricing.unwrap_or_else(|| store.pricing().cloned().unwrap_or_default());
            let report = store.import_dir(&root, &pricing)?;
            (store, report, pricing)
        };
        store.refresh_cached_prices(&pricing);
        let included: Vec<_> = store
            .events()
            .iter()
            .filter(|e| !e.is_excluded_auto_review())
            .collect();
        let excluded: Vec<_> = store
            .events()
            .iter()
            .filter(|e| e.is_excluded_auto_review())
            .collect();
        let mut output = serde_json::json!({"schema_version":1,"mode":args[0],"pricing_assumption":pricing.tier_label,"pricing_verified_at":pricing.verified_at,"long_context_adjustment":pricing.apply_long_context,"events":included,"excluded_auto_review_events":excluded,"exclusion_is_not_free_billing_claim":true,"turn_summaries":usage_views::group(&included,GroupBy::Turn),"task_summaries":usage_views::group(&included,GroupBy::Task),"quota_observations":store.quota_observations(),"files_scanned":report.files_scanned,"lines_read":report.lines_read,"warnings":report.warnings});
        output["display_timezone"] = serde_json::json!(display_time::DISPLAY_ZONE);
        output["timestamp_basis"] =
            serde_json::json!("UTC Unix seconds; *_hk fields are fixed +08:00 displays");
        display_time::annotate_timestamps(&mut output);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    let allowed = [
        "--demo",
        "--offline",
        "--no-settings",
        "--show-tokens",
        "--show-history",
        "--show-settings",
        "--data-dir",
    ];
    let mut i = 0;
    while i < args.len() {
        if !allowed.contains(&args[i].as_str()) {
            return Err(format!("Unknown argument: {}. Use --help.", args[i]).into());
        }
        if args[i] == "--data-dir" {
            i += 1;
            if args.get(i).is_none_or(|v| v.starts_with("--")) {
                return Err("--data-dir requires a path".into());
            }
        }
        i += 1;
    }
    ui::launch(args).map_err(|e| e.to_string().into())
}
