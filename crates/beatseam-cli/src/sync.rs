//! `beatseam sync <server>`: sign in with a device code (once), then check in and report what landed in the local library.

use std::path::Path;
use std::sync::Arc;

use beatseam_api::client::{DeviceSignInProgress, DeviceSignInResult};
use beatseam_core::library_store::LibraryStore;
use beatseam_core::tasks::Runtime;
use beatseam_native::SqliteLibraryStore;
use beatseam_server_mode::{
    DEFAULT_ID_LENGTH, NoSecretStore, ServerMode, ServerModeDeps, SyncKind, random_id,
};

use crate::runtime::{CliRuntime, SharedRuntime};
use crate::transport::UreqTransport;

fn random_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the OS has a random source");
}

/// Drives `future` on the CLI runtime against the wall clock until it finishes.
fn drive<T: 'static>(runtime: &Arc<CliRuntime>, future: impl Future<Output = T> + 'static) -> T {
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let out = slot.clone();
    runtime.spawn(Box::pin(async move {
        *out.borrow_mut() = Some(future.await);
    }));
    let start = std::time::Instant::now();
    loop {
        runtime.set_now(start.elapsed().as_millis() as u64);
        runtime.run_until_stalled();
        if let Some(value) = slot.borrow_mut().take() {
            return value;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub fn run(server: &str, db: &Path) -> Result<(), String> {
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let store = Arc::new(SqliteLibraryStore::open(db).map_err(|e| e.to_string())?);
    let runtime = CliRuntime::new();
    let mode = drive(
        &runtime,
        ServerMode::open(ServerModeDeps {
            store: store.clone(),
            transport: UreqTransport::new(),
            runtime: SharedRuntime(runtime.clone()),
            secret_store: NoSecretStore,
            device_name: Box::new(|| "Beatseam CLI".to_owned()),
            new_id: Box::new(|| random_id(&mut random_bytes, DEFAULT_ID_LENGTH)),
            same_origin: false,
        }),
    )
    .map_err(|e| e.to_string())?;

    if !mode.is_active() {
        let m = mode.clone();
        let server = server.to_owned();
        let result = drive(&runtime, async move {
            m.sign_in_with_device_code(
                &server,
                |progress| match progress {
                    DeviceSignInProgress::Code {
                        user_code,
                        verification_uri,
                        ..
                    } => {
                        eprintln!("Open {verification_uri} and enter the code {user_code}");
                    }
                    DeviceSignInProgress::Waiting => {}
                },
                None,
            )
            .await
        })
        .map_err(|e| e.to_string())?;
        match result {
            DeviceSignInResult::SignedIn { me, .. } => {
                eprintln!("Signed in as {}", me.display_name)
            }
            DeviceSignInResult::Denied => return Err("the sign-in was denied".into()),
            DeviceSignInResult::Expired => {
                return Err("the code expired before it was approved".into());
            }
        }
    }

    let m = mode.clone();
    let outcome = drive(&runtime, async move { m.sync().await });
    let kind = match outcome.kind {
        SyncKind::Snapshot => "snapshot",
        SyncKind::Delta => "delta",
        SyncKind::Offline => "offline (server unreachable)",
        SyncKind::SignedOut => "signed out by the server",
        SyncKind::Failed => "failed",
        SyncKind::Inactive => "not signed in",
    };
    let status = mode.get_status();
    if let Some(error) = &status.error {
        eprintln!("sync error: {error}");
    }
    let s = store.clone();
    let (tracks, playlists) = drive(&runtime, async move {
        (
            s.list_all_tracks().await.map(|t| t.len()),
            s.list_all_playlists().await.map(|p| p.len()),
        )
    });
    println!(
        "{kind}{}: {} tracks, {} playlists",
        if outcome.changed {
            " (library changed)"
        } else {
            ""
        },
        tracks.map_err(|e| e.to_string())?,
        playlists.map_err(|e| e.to_string())?
    );
    match outcome.kind {
        SyncKind::Snapshot | SyncKind::Delta => Ok(()),
        _ => Err(format!("sync did not complete: {kind}")),
    }
}
