//! Manual smoke run (admin): change the timezone to UTC+12, then revert.
//! Build: `cargo build -p apply --release`; run the binary from an elevated
//! shell. Prints a checklist for the operator; nothing here is automated proof.

#[cfg(windows)]
fn main() {
    use apply::{AdminCheck, Applier, TimezoneApplier, TimezoneValue};

    let admin = apply::WindowsAdminCheck;

    println!("apply-smoke: manual checklist (run under Administrator)");
    if !admin.is_admin() {
        println!("[x] 0. admin token: false -> the applier will refuse; stop.");
        return;
    }
    println!("[ ] 0. admin token: true");

    let target = TimezoneValue::new("Line Islands Standard Time", false);
    let tz_registry = apply::WindowsTzRegistry::new();
    let mut applier = TimezoneApplier::new(
        &tz_registry,
        apply::WindowsServiceController,
        apply::WindowsTzSetter,
        admin,
    );

    // 1. Remember the current TZ + tzautoupdate state (variant A capture).
    let original = apply::block_on(applier.current()).expect("read current timezone");
    let service_state = applier.capture_service_state().expect("query tzautoupdate");
    println!(
        "[ ] 1. original: {{ key_name: {:?}, dst_disabled: {}, service_state: {:?} }}",
        original.key_name, original.dst_disabled, service_state
    );

    // 2. Apply a rare zone (UTC+12: Line Islands).
    let change = apply::block_on(apply::Applier::apply(&mut applier, &target)).expect("apply");
    println!("[ ] 2. apply -> {change:?}");
    println!("      check: Get-Date / tzutil /g / HKCU...\\TimeZoneKeyName");

    // 3. Verify tzautoupdate is Disabled.
    let svc = applier.capture_service_state().expect("query tzautoupdate");
    println!("[ ] 3. tzautoupdate after apply: {svc:?} (expect Disabled)");

    // 4. Revert to the original with the captured service state.
    let mut original_to_restore = original.clone();
    original_to_restore.service_state = service_state;
    apply::block_on(applier.revert(&original_to_restore)).expect("revert");
    println!("[ ] 4. revert done; check tzutil /g and tzautoupdate = {service_state:?}");
    println!("[ ] 5. confirm every box above manually (sc qc tzautoupdate).");
}

#[cfg(not(windows))]
fn main() {
    println!("Windows only");
}
