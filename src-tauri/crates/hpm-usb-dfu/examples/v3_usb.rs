//! Explicit bench helper using the same Rust updater as the GUI.
//! cargo run -p hpm-usb-dfu --example v3_usb -- signed.hpmota [silent-seconds]
use hpm_usb_dfu::{v3, FlashOutcome};
use std::{sync::atomic::AtomicBool, time::Duration};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let bytes = std::fs::read(args.next().ok_or("provide a signed .hpmota file")?)?;
    let wait = args
        .next()
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    let cancel = AtomicBool::new(false);
    println!("WAITING_FOR_USB_TARGET");
    let v3::UsbCapture::V3(mut session) = v3::capture_usb(&cancel, Duration::from_secs(30))? else {
        return Err("this helper requires V3".into());
    };
    let artifact = v3::Artifact::parse(bytes, session.info.clone())?;
    println!("CAPTURED_SILENT_WAIT_{wait}_SECONDS");
    std::thread::sleep(Duration::from_secs(wait));
    let outcome = session.flash(&artifact, &cancel, |p| {
        println!("{:?} {}/{}", p.stage, p.completed, p.total)
    });
    session.close();
    let outcome = outcome?;
    if outcome != FlashOutcome::JumpAckedStartupUnconfirmed {
        return Err(format!("unexpected outcome {outcome:?}").into());
    }
    println!("USB_V3_SILENT_WAIT_AND_UPGRADE_PASS (APP health requires a separate channel)");
    Ok(())
}
