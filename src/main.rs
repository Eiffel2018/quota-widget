#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod ui;
use quota_widget::{
    storage,
    tokens::{Pricing, TokenStore},
    usage_views::{self, GroupBy},
};
use std::{path::PathBuf, process::ExitCode};
fn main() -> ExitCode {
    #[cfg(windows)]
    if std::env::args().skip(1).any(|a| {
        matches!(
            a.as_str(),
            "inspect" | "import" | "--help" | "-h" | "--version"
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
        println!("Quota Widget 0.3.2\nNative read-only Codex quota + local token history.\n\nGUI:\n  quota-widget [--demo|--offline] [--no-settings] [--data-dir PATH]\n  --demo         Synthetic data only; never starts CLI or scans real sessions\n  --offline      Never starts CLI; local import remains user-configurable\n  --no-settings  Ephemeral state; no preference/history loading or saving\n  --show-tokens  Open daily token details\n  --show-history Open daily quota history\n\nMachine interface:\n  quota-widget inspect --sessions PATH [--state PATH] [--pricing PATH]\n    Read-only import. JSON schema version 1 on stdout. Does not modify inputs.\n  quota-widget import --sessions PATH --state PATH [--pricing PATH]\n    Incrementally import into specified state, guarded by writer lock.\n  quota-widget --version\n\nExit codes: 0 success, 1 invalid arguments or operation failed.\nNo login, inference, purchases, or credential handling.");
        return Ok(());
    }
    if args.iter().any(|s| s == "--version") {
        println!("quota-widget 0.3.2");
        return Ok(());
    }
    if matches!(args.first().map(String::as_str), Some("inspect" | "import")) {
        let mut i = 1;
        while i < args.len() {
            if !["--sessions", "--state", "--pricing"].contains(&args[i].as_str()) {
                return Err(format!("Unknown argument: {}", args[i]).into());
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
        let root = get("--sessions").ok_or("--sessions PATH is required")?;
        let state = get("--state");
        let write = args[0] == "import";
        let _lock = if write {
            Some(storage::DataLock::acquire(
                state
                    .as_ref()
                    .ok_or("--state PATH is required for import")?
                    .parent()
                    .ok_or("State needs a parent directory")?,
            )?)
        } else {
            None
        };
        let pricing: Pricing = match get("--pricing") {
            Some(p) => storage::load_json(&p)?,
            None => Pricing::default(),
        };
        let mut store = match &state {
            Some(p) if p.exists() => TokenStore::load(p)?,
            _ => TokenStore::default(),
        };
        let report = store.import_dir(&root, &pricing)?;
        if write {
            store.save(state.as_ref().unwrap())?;
        }
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
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"schema_version":1,"mode":args[0],"pricing_assumption":pricing.tier_label,"pricing_verified_at":pricing.verified_at,"long_context_adjustment":pricing.apply_long_context,"events":included,"excluded_auto_review_events":excluded,"exclusion_is_not_free_billing_claim":true,"turn_summaries":usage_views::group(&included,GroupBy::Turn),"task_summaries":usage_views::group(&included,GroupBy::Task),"quota_observations":store.quota_observations(),"files_scanned":report.files_scanned,"lines_read":report.lines_read,"warnings":report.warnings})
            )?
        );
        return Ok(());
    }
    let allowed = [
        "--demo",
        "--offline",
        "--no-settings",
        "--show-tokens",
        "--show-history",
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
