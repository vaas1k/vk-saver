#![no_main]
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;
use vk_engine::export::unzip::unzip;
use vk_engine::testing::{tmpdir, FakeSpace, NoProgress};

// The ZIP archive the user downloads from VK is untrusted input. This fuzzes the
// unzip pipeline (plan + extract): any Stop error is fine, a panic or UB is a bug.
fuzz_target!(|data: &[u8]| {
    let dir = tmpdir("fuzz-unzip");
    let zip = dir.join("input.zip");
    if std::fs::write(&zip, data).is_err() {
        return;
    }
    let cancel = AtomicBool::new(false);
    let space = FakeSpace(Ok(u64::MAX));
    let _ = unzip(&zip, &space, &cancel, &NoProgress);
    let _ = std::fs::remove_dir_all(&dir);
});
