//! Embeds the app icon (`assets/truffle.ico`, resource id 1) and version information into
//! truffle.exe. Windows targets only. A missing resource compiler (rc.exe from the Windows SDK)
//! never fails the build: Truffle then builds without an exe icon and a cargo warning says why.
//! The window, taskbar and tray icons don't depend on this (they come from the embedded PNGs).
//!
//! Also hands the app its build date for Settings > About (`TRUFFLE_BUILD_DATE`, "25 Oct 2026",
//! UTC), on every target.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "src/build_date.rs"]
mod build_date;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Run again whenever the code changes, so the date is the day of the build that made the exe.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rustc-env=TRUFFLE_BUILD_DATE={}", build_date::date_text(build_day()));
    println!("cargo:rerun-if-changed=assets/truffle.ico");
    // The version info is taken from the package version.
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=RC_PATH");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // winresource unwraps a few things internally (environment variables, the SDK lookup). Keep
    // any surprise there from failing the whole build.
    match std::panic::catch_unwind(embed_resources) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn(&e),
        Err(_) => warn("the resource tool stopped unexpectedly"),
    }
}

fn embed_resources() -> Result<(), String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?;
    let icon = Path::new(&manifest_dir).join("assets").join("truffle.ico");
    if !icon.is_file() {
        return Err(format!("{} is missing (run `cargo run --example make_icon`)", icon.display()));
    }
    let icon = icon.to_str().ok_or("the project path is not valid Unicode")?;

    let mut res = winresource::WindowsResource::new();
    // Windows shows FileDescription as the app's name (Task Manager, Startup apps, the volume
    // mixer, the media overlay), so it is just the name; the tagline goes in Comments.
    res.set_icon(icon)
        .set("ProductName", "Truffle")
        .set("FileDescription", "Truffle")
        .set("Comments", "Lightweight Spotify player")
        .set("InternalName", "truffle")
        .set("OriginalFilename", "truffle.exe");
    res.compile().map_err(|e| {
        let hint = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            "install the Windows SDK (Visual Studio Installer, \"Desktop development with C++\") \
             or point RC_PATH at rc.exe"
        } else {
            "install windres (MinGW binutils) or set WINDRES"
        };
        format!("{e}; {hint}")
    })
}

/// Today, counted in days from 1 January 1970 (UTC). `SOURCE_DATE_EPOCH` (seconds) fixes it, for
/// builds that must come out the same every time.
fn build_day() -> i64 {
    let secs = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
    });
    secs.div_euclid(86_400)
}

fn warn(reason: &str) {
    println!(
        "cargo:warning=truffle.exe is built without its icon and version info: {reason}. \
         The app works normally."
    );
}
