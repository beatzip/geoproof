//! Manual smoke: change locale/geo/preload, print a checklist, revert.
//! Runs under a normal user (all writes are HKCU). Run WITHOUT arguments,
//! or with `--hold N` to keep the applied state for N seconds before
//! waiting for ENTER (useful for automated observation windows).
//!
//! 1. The current values are captured.
//! 2. "en-US" / "840" / ["00000409"] are applied.
//! 3. The operator verifies the registry and Chrome (see the checklist).
//! 4. Enter reverts everything to the captured values.

use apply::{
    Applier, LocaleApplier, LocaleValue, WindowsLocaleRegistry, WindowsSettingChangeBroadcast,
};

fn main() {
    let hold_secs = parse_hold_arg();
    let target = target_value();
    let registry = WindowsLocaleRegistry::new();
    let broadcast = WindowsSettingChangeBroadcast::new();
    let mut applier = LocaleApplier::new(registry, broadcast);

    let before = apply::block_on(applier.current()).expect("read current locale state");
    println!("current: {before:?}");

    let res = apply::block_on(applier.apply(&target)).expect("apply failed");
    println!("applied: {res:?}");

    println!("\nCHECKLIST (manual):");
    println!("  1. reg query \"HKCU\\Control Panel\\International\" /v LocaleName   -> en-US");
    println!("  2. reg query \"HKCU\\Control Panel\\International\\Geo\" /v Nation -> 840");
    println!("  3. reg query \"HKCU\\Keyboard Layout\\Preload\"           -> values \"1\"=\"00000409\" (REG_SZ)");
    println!("  4. Fresh Chrome: new Intl.DateTimeFormat().resolvedOptions().locale starts with \"en-US\" (partial pickup is normal; restart Chrome if not)");
    println!("\nPress ENTER to revert...");
    if let Some(secs) = hold_secs {
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("stdin");

    apply::block_on(applier.revert(&before)).expect("revert failed");
    let after = apply::block_on(applier.current()).expect("re-read");
    println!("reverted: {after:?}");
    if after == before {
        println!("OK: system matches the captured values");
    } else {
        println!("MISMATCH: revert did not restore the captured values");
        std::process::exit(1);
    }
}

fn target_value() -> LocaleValue {
    // A const with a Vec is not possible; build the target here.
    LocaleValue {
        locale_name: "en-US".to_string(),
        geo_nation: "244".to_string(),
        kbd_preload: vec!["00000409".to_string()],
    }
}

/// `--hold N`: keep the applied state for N seconds before waiting for
/// ENTER. Unrecognized arguments exit with an error so a typo cannot
/// silently turn into an instant revert.
fn parse_hold_arg() -> Option<u64> {
    let mut args = std::env::args().skip(1);
    let flag = args.next()?;
    if flag != "--hold" {
        eprintln!("unknown argument: {flag} (supported: --hold N)");
        std::process::exit(2);
    }
    let n = args
        .next()
        .unwrap_or_else(|| {
            eprintln!("--hold requires a number of seconds");
            std::process::exit(2);
        })
        .parse()
        .unwrap_or_else(|_| {
            eprintln!("--hold requires a number of seconds");
            std::process::exit(2);
        });
    Some(n)
}
